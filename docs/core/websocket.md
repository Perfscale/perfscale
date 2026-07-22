# WebSocket load testing

perfscale drives **WebSocket** endpoints under load with the native step
engine: open connections, stream messages from templates, wait for matching
replies, and measure both the handshake and the application-level message
round trip alongside your HTTP/TCP/UDP metrics.

This page is the guide — what the pieces are and when to reach for which.
Per-step parameters, outputs, and error semantics live in the
[actions reference](actions.md#websocket-stdwsv1-and-the-stdws-v1-family);
a runnable scenario ships as
[`examples/websocket.test.yaml`](../../examples/websocket.test.yaml).

## Two styles

- **One-shot session** (`std/ws@v1`) — connect, exchange, close in one step.
  The whole session is timed as one latency sample, like a FIX session.
  Simplest; right for request/reply-shaped exchanges.
- **Live connection** (`std/ws-connect@v1` + friends) — a connection held
  across steps within an iteration, addressed by the id the connect step
  returns. Interleave WS traffic with other steps: subscribe over WS, trigger
  via REST, assert the push arrives.

Freely mixable in one scenario.

## One-shot session

```yaml
steps:
  - name: subscribe and await first trade
    use: std/ws@v1
    with:
      url: wss://stream.example.com/feed
      messages:
        - send: '{"op":"subscribe","channel":"trades","id":"sub-${seq}"}'
          until_json: { type: trade }
    check:
      message_matches: { type: trade }
```

Each `messages` entry sends a payload; an entry with an `until_*` rule waits
for its matching reply before the next entry — and yields one **message RTT**
sample. The step fails on handshake/transport errors or on any entry whose
rule did not match in time. Full parameter list:
[`std/ws@v1`](actions.md#stdwsv1--one-shot-session).

## Live connection

```yaml
# config.yaml — 25 concurrent VUs, each holding its own connection per iteration
vus: 25
duration: 5m
```

```yaml
# test.yaml
steps:
  - name: open feed
    use: std/ws-connect@v1
    with: { url: "wss://stream.example.com/feed" }
    outputs: feed

  - name: subscribe
    use: std/ws-send@v1
    with:
      id: "${{ feed.id }}"
      send: '{"op":"subscribe","id":"sub-${seq}"}'

  - name: await confirmation
    use: std/ws-recv@v1
    with:
      id: "${{ feed.id }}"
      until_json: { type: subscribed }
    outputs: got

  - name: hang up
    use: std/ws-close@v1
    with: { id: "${{ feed.id }}" }
```

Five steps share the connection:

- [`std/ws-connect@v1`](actions.md#stdws-connectv1) — opens it, returns
  `{ id, subprotocol, … }`.
- [`std/ws-send@v1`](actions.md#stdws-sendv1) — sends a text template
  (`${…}` tokens expand per send) or binary `send_base64`;
  `repeat` + `interval_ms` stream N messages from one template.
- [`std/ws-recv@v1`](actions.md#stdws-recvv1) — reads until a **stopping
  rule**: `until_contains` (substring), `until_json` (JSON-subset match), or
  plain `count`. Not reaching it within `timeout` fails the step.
- [`std/ws-ping@v1`](actions.md#stdws-pingv1) — transport ping→pong; RTT in
  the step's `duration_ms`.
- [`std/ws-close@v1`](actions.md#stdws-closev1) — graceful close handshake.

Connection ids are minted per VU (`ws-1`, `ws-2`, …) and are valid only
inside that VU's current iteration — an id never crosses into the next
iteration or another VU. Whatever a scenario leaves open is dropped at
iteration end (abruptly — use `ws-close` for a clean shutdown), and
`ws-connect` inside a config's `before:` setup is not useful: the setup
context and its sockets are gone before VUs start.

## Dynamic messages

Text payloads may embed single-brace `${…}` tokens (`${seq}`, `${uuid}`,
`${now}`/`${now_ms}`/`${now_iso}`, `${rand(a,b)}`, `${randf(a,b[,dp])}`,
`${choice(x|y|z)}`), expanded anew per send — distinct from `${{ … }}`, which
resolves once before the action runs:

```yaml
- use: std/ws-send@v1
  with:
    id: "${{ feed.id }}"
    send: '{"op":"order","id":"ord-${seq}","px":${randf(1.05,1.15,5)}}'
    repeat: 100
    interval_ms: 50
```

The full token table is in the
[`std/ws-send@v1` reference](actions.md#stdws-sendv1).

## Asserting messages

Receive steps expose a `messages` list; `std/check@v1` asserts over it with
the **any** quantifier (at least one message matches — streams carry
heartbeats and unrelated events):

```yaml
check:
  message_contains: "trade"              # some message contains the substring
  message_matches: { type: trade }       # some message JSON-subset-matches
  messages_count_gte: 5                  # at least 5 messages arrived
```

For deterministic exchanges, address one message by index:
`check: { on: got.messages.0, message_matches: { type: welcome } }`.

## Metrics

- **Handshakes** and **one-shot sessions** feed the shared latency histogram
  (`http_req_duration`) — comparable with HTTP/TCP/UDP percentiles; failed
  handshakes count in `http_req_failed`.
- **`ws_msg_rtt`** — application-level message RTT: time from a send to the
  first reply matching your until-rule, aggregated as a histogram (p50 / p95
  / max in the summary).
- **`ws_msgs_sent` / `ws_msgs_received`** — message throughput counters.
- Waiting on a server-push stream is deliberately *not* counted as latency —
  how long a server chooses to wait before pushing is not target latency and
  would poison the shared percentiles.

## Limits

Inbound protocol limits come from the WebSocket library defaults: messages up
to 64 MiB, single frames up to 16 MiB — a larger inbound message errors the
connection (and therefore the step reading it). Timeout values, `repeat`
counts, and the `messages` list have no built-in caps.
