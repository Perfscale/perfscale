# Shared variables

perfscale's `variables:` and step outputs are **immutable and per-VU** — one
VU cannot see another's values, and nothing can be updated mid-run. Shared
variables fill that gap: one run-scoped key/value store with **atomic**
operations, mutated and read from steps with `std/set_shared_variable@v1` /
`std/get_shared_variable@v1`. They express the coordination patterns
per-VU variables cannot:

- **Shared counters** — count events across all VUs (`increment`).
- **Producer/consumer queues** — one VU appends work items, others `pop`
  them FIFO.
- **Barriers** — a consumer blocks with `wait_for` until N items have
  accumulated or a flag flips.

Every operation is a single atomic acquisition — there is no
read-modify-write pair of steps to race, and no lock primitive to deadlock.

This page is the guide. Per-step parameters and outputs live in the
[actions reference](actions.md#stdset_shared_variablev1).

## Declaration (mandatory)

Shared variables are declared in the **config file**, next to `variables:`,
as a map of name → initial JSON value. The type is inferred from the initial
value and constrains which ops are legal:

```yaml
# config.yaml
vus: 8
duration: 30s

shared_variables:
  pending_orders: []      # list  → append / pop / length_gte
  approved_count: 0       # number → increment
  last_error: null        # any   → set / get
```

A step referencing an undeclared name, or an `op` incompatible with the
declared type (`increment` on a list), fails **configuration validation**
before any VU starts — a typo never becomes a silent `null` mid-test. There
is no dynamic creation: steps only use declared names. At run start the
declared names are (re)set to their initial values.

## Producer / consumer queue

One VU (or one test file) produces work items, the rest consume. `append`
returns the new list length; `pop` removes and returns the first element
(`null` on an empty list — gate on it with `check:` when emptiness is an
error, or use `wait_for` to block instead):

```yaml
# config.yaml
shared_variables:
  pending_orders: []
```

```yaml
# producer.yaml — every iteration enqueues one order
steps:
  - name: enqueue order
    use: std/set_shared_variable@v1
    with:
      name: pending_orders
      op: append
      value: { id: "ord-${seq}", total: "${randf(10,100,2)}" }
```

```yaml
# consumer.yaml — block up to 10s for an item, then take it
steps:
  - name: dequeue order
    use: std/get_shared_variable@v1
    with:
      name: pending_orders
      op: pop
      wait_for: { length_gte: 1, timeout_ms: 10000 }
      extract: { order_id: $.id, total: $.total }
    check:
      body_contains: ord-
```

Payloads interpolate like any other step — `${seq}`, `${randf(10, 100, 2)}`,
`${{ vars.* }}`, outputs of previous steps. `extract` pulls fields out of the
value with the same dotted-path syntax (`$.foo.bar`, `$.[0].x`) as
[`std/llm@v1`](llm.md)'s extract; each extracted key lands at the top level
of the step output, so `outputs:` + `${{ name.order_id }}` works downstream.

## Barrier: accumulate N items

A consumer that must not start until N producers have contributed — the
classic load-test rendezvous. `wait_for` blocks (polling with a small async
sleep, no spinning) until the condition holds:

```yaml
steps:
  - name: wait for batch of 10 orders
    use: std/get_shared_variable@v1
    with:
      name: pending_orders
      wait_for: { length_gte: 10, timeout_ms: 30000 }
```

`wait_for` takes exactly one condition — `exists`, `equals: <json>`, or
`length_gte: <int>` — plus `timeout_ms` (default `5000`). On timeout the
step **fails** with an `[err]` line reporting the last observed value (the
same contract as `std/pubsub@v1`'s subscribe timeout): a producer that fell
behind is the signal you are sizing for, not a silent pass. The wait time
lands in `waited_ms` and a `shared_variable_wait_ms` metric sample, so slow
barriers show up in the summary.

A shared counter pairs naturally with a barrier — `increment` returns the
new value:

```yaml
# config.yaml
shared_variables:
  approved_count: 0
```

```yaml
steps:
  - name: approve
    use: std/set_shared_variable@v1
    with: { name: approved_count, op: increment, value: 1 }

  - name: proceed once 50 approvals are in
    use: std/get_shared_variable@v1
    with:
      name: approved_count
      wait_for: { equals: 50, timeout_ms: 60000 }
```

## Drivers: memory vs redis

The backing store is pluggable behind a driver, chosen per step:

| Driver | Store |
|---|---|
| `memory` (default) | Process-global map — shared by every VU **in one process**, no external service needed |

Vendor drivers — **Redis**, … — are a pro capability registered on the same
driver seam; an unknown `driver` value fails the step with the list of
registered drivers, so it is always clear which build you are running.

The `memory` store is **per process**: when a distributed run spreads VUs
across multiple `perfscaled` agents, each agent has its own store — an
`append` on agent A is invisible to agent B. Cross-agent coordination (a
cluster-wide queue or barrier) requires a networked driver such as Redis;
within one agent, `memory` is the fastest option and needs nothing running.

## Limits

- No dynamic creation — undeclared names are config errors, so a test's
  shared state is fully listed in `shared_variables:`.
- `pop` is FIFO and there is no peek-by-index; consumer-side assertions on
  payload fields go through `extract` or a `std/check@v1` step.
- `wait_for` is polling, not a push subscription — keep `timeout_ms` tight
  in hot loops so a stuck producer fails fast.
