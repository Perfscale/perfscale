# gRPC load testing

perfscale drives **gRPC** endpoints under load with the native step engine:
open HTTP/2 channels, make unary calls from JSON templates, run
client/server/bidi streams, and measure both the request round trip and the
application-level message RTT alongside your HTTP/TCP/UDP/WebSocket metrics.

Calls are **dynamic**: no protobuf codegen. The schema arrives at run time
(from a descriptor set or server reflection), requests and responses are
JSON, and perfscale maps between the two using protobuf-JSON rules.

This page is the guide — what the pieces are and when to reach for which.
Per-step parameters, outputs, and error semantics live in the
[actions reference](actions.md#grpc-stdgrpcv1-and-the-stdgrpc-v1-family);
a runnable scenario ships as
[`examples/grpc.test.yaml`](../../examples/grpc.test.yaml), together with a
local echo server to run it against
(`cargo run -p perfscale-core --example grpc_echo_server`).

## Two styles

- **One-shot call** (`std/grpc@v1`) — connect, load the schema, make one
  unary call, close in one step. Simplest for occasional probes.
- **Live channel** (`std/grpc-connect@v1` + friends) — a channel held across
  steps within an iteration, addressed by the id the connect step returns.
  Unary calls and streams ride the same HTTP/2 connection: the connect and
  the schema load are paid once per iteration, not per call. Use this for any
  serious load.

Freely mixable in one scenario.

## Schema sources

Dynamic calls need the protobuf schema at run time. Both connect-capable
steps (`std/grpc@v1`, `std/grpc-connect@v1`) take exactly one source — the
two are mutually exclusive:

- **`descriptor_set`** — base64 of a serialized `FileDescriptorSet`. Produce
  one from your protos:

  ```bash
  protoc --descriptor_set_out=echo.pb --include_imports echo.proto
  base64 -i echo.pb   # macOS; on Linux: base64 -w0 echo.pb
  ```

  Or fetch it over HTTP in an earlier step and pipe the binary body straight
  in — a `std/http@v1` step returns binary payloads as `body_base64`:

  ```yaml
  steps:
    - name: fetch schema
      use: std/http@v1
      with: { url: "https://schema.example.com/echo.pb" }
      outputs: fetch

    - name: unary probe
      use: std/grpc@v1
      with:
        url: grpcs://api.example.com:443
        descriptor_set: "${{ fetch.body_base64 }}"
        method: "echo.v1.Echo/Unary"
        payload: { message: "ping ${seq}" }
      check:
        duration_ms_lt: 500
  ```

- **`reflection: true`** — fetch the schema from the server's reflection
  service (v1 protocol); the server must have reflection enabled. The fetched
  pool is cached per URL for the rest of the run, so repeated connects to one
  server pay one reflection round trip.

A bad `descriptor_set` fails fast, before any network I/O. Methods are named
`"package.Service/Method"`; a typo fails with a did-you-mean suggestion when
a known method is close enough.

## Live channel

```yaml
# config.yaml — 25 concurrent VUs, each holding its own channel per iteration
vus: 25
duration: 5m
```

```yaml
# test.yaml
steps:
  - name: open channel
    use: std/grpc-connect@v1
    with:
      url: grpcs://api.example.com:443
      reflection: true
      metadata: { authorization: "Bearer ${{ vars.token }}" }
    outputs: conn

  - name: unary echo
    use: std/grpc-call@v1
    with:
      id: "${{ conn.id }}"
      method: "echo.v1.Echo/Unary"
      payload: { message: "ping-${seq}" }
    check:
      duration_ms_lt: 250

  - name: open bidi stream
    use: std/grpc-stream-open@v1
    with:
      id: "${{ conn.id }}"
      method: "echo.v1.Echo/Bidi"
    outputs: stream

  - name: send events
    use: std/grpc-stream-send@v1
    with:
      id: "${{ stream.id }}"
      payload: { message: "evt-${seq}" }
      repeat: 5
      interval_ms: 20

  - name: await echoes
    use: std/grpc-stream-recv@v1
    with:
      id: "${{ stream.id }}"
      until_contains: "evt-5"
      timeout: 5000
    check:
      messages_count_gte: 5

  - name: close stream
    use: std/grpc-stream-close@v1
    with: { id: "${{ stream.id }}" }
```

Seven steps, each with a short alias:

- [`std/grpc-connect@v1`](actions.md#stdgrpc-connectv1) — opens the channel
  and loads the schema, returns `{ id, connected, duration_ms }`.
- [`std/grpc-call@v1`](actions.md#stdgrpc-callv1) — one unary call on the
  channel. A failed RPC fails the step but the channel stays usable: HTTP/2
  channels recover, unlike a dead WebSocket.
- [`std/grpc@v1`](actions.md#stdgrpcv1--one-shot-call) — the one-shot
  variant (connect → schema → call → close).
- [`std/grpc-stream-open@v1`](actions.md#stdgrpc-stream-openv1) — starts a
  client-streaming, bidi, or server-streaming call, returns a stream id. For
  server-streaming the one request goes out at open; a server-side failure
  (UNIMPLEMENTED, auth, …) surfaces at the first recv/close, not at open.
- [`std/grpc-stream-send@v1`](actions.md#stdgrpc-stream-sendv1) — sends
  `payload` (JSON, `${…}` tokens expand per send) or `payload_base64`;
  `repeat` + `interval_ms` stream N messages from one template.
- [`std/grpc-stream-recv@v1`](actions.md#stdgrpc-stream-recvv1) — reads
  until a **stopping rule**: `until_contains`, `until_json`, or plain
  `count`.
- [`std/grpc-stream-close@v1`](actions.md#stdgrpc-stream-closev1) —
  half-closes the request side and drains the server side to the final
  status.

Channel ids are minted per VU (`grpc-1`, `grpc-2`, …), stream ids likewise
(`grpcs-1`, …); both are valid only inside that VU's current iteration. A
channel never outlives its iteration: whatever a scenario leaves open is
dropped at iteration end (streams are cancelled — use `grpc-stream-close`
for a clean, status-checked shutdown), and `grpc-connect` inside a config's
`before:` setup is not useful: the setup context and its channels are gone
before VUs start.

## Dynamic payloads

Requests take `payload` (JSON → dynamic protobuf message) or
`payload_base64` (serialized protobuf bytes) — mutually exclusive. The JSON
mapping follows protobuf-JSON rules: field names accept both the proto name
and its camelCase `json_name`, 64-bit ints are strings, enums are names.
Responses appear in `body` (unary) and `messages` (streams) under the same
rules.

String leaves of `payload` may embed the same single-brace `${…}` tokens as
[WebSocket sends](websocket.md#dynamic-messages), expanded per call/send
(`${seq}` keeps counting per channel on unary calls, per stream on stream
sends). `payload_base64` is decoded once and sent as-is — no token
expansion. Metadata values support `${{ … }}` interpolation only.

## Asserting responses

Stream receive/close steps expose a `messages` list; `std/check@v1` asserts
over it with the **any** quantifier (at least one message matches):

```yaml
check:
  message_contains: "trade"              # some message contains the substring
  message_matches: { type: trade }       # some message JSON-subset-matches
  messages_count_gte: 5                  # at least 5 messages arrived
```

For deterministic exchanges, address one message by index:
`check: { on: got.messages.0, message_matches: { type: welcome } }`.

Unary calls assert on the status code with `expect_status` (e.g.
`expect_status: 5` passes when the server returns NOT_FOUND and fails on OK)
and on latency with `check: { duration_ms_lt: … }`; the JSON `body` can be
addressed field by field via `outputs` and `on:`.

## Metrics

- **`grpc_req_duration`** — unary call latency histogram (`std/grpc@v1` and
  `std/grpc-call@v1` only). Streams deliberately do not feed it: their
  lifetimes span user steps.
- **`grpc_msg_rtt`** — application-level message RTT. On a successful unary
  call it equals the request duration; on a stream recv it is the send→match
  time, reported only when an `until_*` rule matched and a
  `grpc-stream-send` preceded it on the same stream.
- **`grpc_msgs_sent` / `grpc_msgs_received`** — message throughput counters,
  per call and per stream step.
- **`grpc_req_failed`** — RPCs that did not meet `expect_status`. For
  streams, `grpc-stream-close` is what turns the final status into this
  counter.

Unlike the WebSocket handshake, gRPC steps never feed `http_req_duration` /
`http_req_failed` — the `grpc_*` series are the whole story, and a failed
connect emits no metrics at all (no RPC was made).

## Limits

Inbound messages are capped by `max_recv_size` (default 16 MiB); an
oversized message fails the call with RESOURCE_EXCEEDED. Binary (`-bin`)
metadata keys are not supported — metadata values are strings. Server
reflection must be explicitly enabled on the server for `reflection: true`
to work. Timeout values, `repeat` counts, and drain lengths have no built-in
caps.
