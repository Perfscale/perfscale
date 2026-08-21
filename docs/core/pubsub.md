# Pub/Sub load testing

perfscale drives **pub/sub messaging** under load with the native step
engine: publish message bursts to a subject, wait for matching messages, and
measure both publish throughput and end-to-end (publish → consumed) latency —
alongside your HTTP/gRPC/WebSocket metrics.

One step = one exchange: (optional) subscribe → (optional) publish →
(optional) wait for `count` matching messages, bounded by a timeout. When
both sides are given the subscription is established **first**, so a
same-subject roundtrip always sees its own messages.

This page is the guide. Per-step parameters and outputs live in the
[actions reference](actions.md#stdpubsubv1).

## Drivers

The transport is pluggable behind a driver, chosen per step:

| Driver | Transport |
|---|---|
| `memory` (default) | In-process broadcast bus — no broker needed. One channel per subject, shared by all VUs in the process: one VU's publish reaches every VU subscribed to that subject (cross-VU fan-out is a feature, not leakage) |
| `nats` | A real [NATS](https://nats.io) server (core NATS, no JetStream) via `async-nats` |

Vendor drivers — **Kafka, Redis, MQTT** — are a pro capability registered on
the same driver seam; their tuning travels in the step's `options` object. An
unknown `driver` value fails the step with the list of registered drivers, so
it is always clear which build you are running.

The `memory` bus needs no `url` and no broker — it is the fastest way to
build the test logic, and under load it shows the engine's raw ceiling. Point
the same YAML at `nats` (or a pro driver) to add the real broker round trip;
diffing the two isolates what the broker costs you.

## Roundtrip in one step

Publish a burst and wait for it on the same subject — the basic broker
health/latency probe:

```yaml
steps:
  - name: order events roundtrip
    use: std/pubsub@v1
    with:
      driver: nats
      url: nats://127.0.0.1:4222
      subject: orders.created
      publish:
        - '{"id":"ord-1","total":42.50}'
        - '{"id":"ord-2","total":17.00}'
      subscribe:
        count: 2                    # wait for both messages
        until_contains: '"id"'      # each counted message must match
        timeout_ms: 2000
    check:
      body_contains: ord-2
```

The step fails on connect failure, publish error, or a subscribe timeout —
the error reports how many of `count` arrived and how many the
`until_contains` matcher rejected. A subscribe shortfall is an assertion
failure, not a transport error: the run continues and the failed exchanges
show up in `pubsub_e2e_ms_failed`.

## Producer-only load

Hammer a broker without consuming — success means every publish was accepted:

```yaml
steps:
  - name: produce order events
    use: std/pubsub@v1
    with:
      driver: nats
      url: nats://nats.internal:4222
      subject: orders.created
      publish:
        - '{"id":"ord-${seq}","total":${randf(10,100,2)}}'
```

Message payloads interpolate like any other step — `${seq}`,
`${randf(10, 100, 2)}`, `${{ vars.* }}`, outputs of previous steps — so every
iteration can publish a fresh payload.

## Consumer with assertions

Subscribe-only is a pure consumer step: wait for a specific event and assert
on the payload. `outputs` exposes `published`, `received`, `duration_ms`, and
`body` (the newline-joined matched payloads) to later steps:

```yaml
steps:
  - name: await shipment event
    use: std/pubsub@v1
    with:
      driver: nats
      url: nats://nats.internal:4222
      subject: orders.shipped
      subscribe:
        count: 1
        until_contains: '"id":"ord-1"'
        timeout_ms: 3000
    check:
      body_contains: ord-1
    outputs: shipment
```

## Producer / consumer pair

The realistic broker pattern: one process generates load, others consume.
Two test files share one config; run them concurrently against one broker:

```yaml
# producer.yaml
steps:
  - name: produce order events
    use: std/pubsub@v1
    with:
      driver: nats
      url: nats://127.0.0.1:4222
      subject: orders.created
      publish: ['{"id":"ord-${seq}"}']
```

```yaml
# consumer.yaml — 10 events per iteration or the step fails
steps:
  - name: consume order events
    use: std/pubsub@v1
    with:
      driver: nats
      url: nats://127.0.0.1:4222
      subject: orders.created
      subscribe: { count: 10, until_contains: '"id"', timeout_ms: 30000 }
```

```yaml
# config.yaml
vus: 4
duration: 30s
```

```sh
perfscale run -f consumer.yaml -c config.yaml &
perfscale run -f producer.yaml -c config.yaml
```

A consumer that falls behind fails its step on the subscribe timeout — that
is the signal you are sizing consumers for. (With the `memory` driver the
same pair works inside one process: the bus fans out across VUs.)

## Metrics and thresholds

Every exchange folds into the run summary as custom metrics:

- `pubsub_msgs_published` — counter of messages accepted by the transport.
- `pubsub_msgs_received` — counter of matched messages (only when
  `subscribe` is given).
- `pubsub_e2e_ms` — trend with one sample per matched message: start of the
  publish phase → consumed. For subscribe-only steps this is the wait time
  (there is no publish to anchor on).

Gate a run on them with `std/thresholds@v1`:

```yaml
  - use: std/thresholds@v1
    with:
      pubsub_e2e_ms:
        - "p(95)<50"              # 95% of messages land within 50 ms
      pubsub_e2e_ms_failed:
        - "rate<0.01"             # fewer than 1% timed-out/failed exchanges
      pubsub_msgs_received:
        - "count>=10000"          # consumers actually kept up
```

## Connection posture

Every exchange is a full cycle: connect → subscribe → publish → collect →
drop. Connections are not pooled across steps or VU iterations — under load
each VU opens a fresh broker connection per iteration (except `memory`,
which is in-process). That keeps the e2e measurement honest — connect cost is
included — but brokers see real connection churn; size test rates
accordingly, and prefer `duration`-bounded runs when pointing at a shared
staging broker.

## Limits

- The `nats` driver is core NATS: no JetStream persistence, streams, or
  queue groups — a subscriber that is not connected yet misses messages (the
  step subscribes first, so same-step roundtrips are safe).
- `until_contains` is a substring match, not a JSONPath/regex — for
  field-level assertions read `body` via a `std/check@v1` step.
- One subject per step; fan-in across subjects needs one step per subject.
