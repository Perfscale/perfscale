# Built-in actions

Actions are the units a native test's `steps` are made of. Each step names an
action in `use`, passes parameters in `with`, and may assert on the result in
`check`.

Full IDs carry a namespace and version (`std/http@v1`); the short aliases
(`http`, `tcp`, `udp`, `ws`, `ws-connect`, `ws-send`, `ws-recv`, `ws-ping`,
`ws-close`, `grpc`, `grpc-connect`, `grpc-call`, `grpc-stream-open`,
`grpc-stream-send`, `grpc-stream-recv`, `grpc-stream-close`, `db-connect`,
`db-query`, `db-tx-begin`, `db-tx-commit`, `db-tx-rollback`, `db-close`,
`check`, `sleep`,
`log`, `file-read`, `file-write`, `child_process`, `kill_process`) resolve to the
same implementations.

## `std/http@v1`

Perform one HTTP request per iteration. Timing feeds the run's metrics.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `url` | string | **required** | Absolute URL |
| `method` | string | `GET` | Any HTTP method, including extension methods like `QUERY` (safe method with a body, [draft-ietf-httpbis-safe-method-w-body](https://datatracker.ietf.org/doc/draft-ietf-httpbis-safe-method-w-body/)) |
| `headers` | object | — | `{ "Name": "Value" }`, string values only |
| `body` | string \| object | — | String → `text/plain`; object → serialized JSON with `application/json`. Mutually exclusive with `multipart` |
| `multipart` | array | — | Send `multipart/form-data` — see [Multipart uploads](#multipart-uploads). Mutually exclusive with `body` |
| `timeout` | integer (ms) | `10000` | Per-request timeout |
| `insecure` | boolean | `false` | Skip TLS certificate verification — for self-signed targets like `perfscale serve --tls`. Never use against hosts you don't control |

**Output** (available via `outputs` / `__last__`):

```json
{
  "status": 200,
  "body": "...",
  "duration_ms": 42.37,
  "headers": { "content-type": "application/json", "x-request-id": "abc-123" }
}
```

Header names are lowercase; repeated headers are joined with `", "`. Reuse
them in later steps via `${{ resp.headers.x-request-id }}`.

Responses with a textual content type (`text/*`, `application/json`,
`application/*+json`, `application/*+xml`, …) arrive in `body` as before.
Binary payloads (e.g. `application/octet-stream`) instead return an empty
`body` plus a `body_base64` field — which is how a fetched protobuf
FileDescriptorSet flows into a gRPC step:
`descriptor_set: "${{ fetch.body_base64 }}"`. A missing content type is
sniffed: valid UTF-8 behaves as text, anything else as binary.

Statuses ≥ 400, transport errors, and timeouts count as failed requests in
`http_req_failed`. A timeout is logged distinctly (`→ TIMEOUT after ...ms`).

### Multipart uploads

`multipart` sends `multipart/form-data` (file uploads, HTML-form-style
endpoints). Each array element is one part:

| Part field | Required | Description |
|---|---|---|
| `name` | yes | Form field name |
| `value` | one of value/file | Text field content |
| `file` | one of value/file | Path to a file on disk (relative to the working directory) |
| `filename` | no | Filename sent to the server; defaults to the file's basename |
| `content_type` | no | MIME type of the part (e.g. `application/octet-stream`) |

```yaml
steps:
  - name: upload report
    use: std/http@v1
    with:
      method: POST
      url: "https://api.example.com/upload"
      multipart:
        - name: file
          file: ./fixtures/report.csv
          content_type: text/csv
        - name: description
          value: "uploaded by ${{ __last__.status }} check run"
    check:
      status: 201
```

Notes:

- The `Content-Type: multipart/form-data; boundary=…` header is set
  automatically — don't add it to `headers`.
- `${{ ... }}` placeholders work in part values and paths, like everywhere
  else in `with:`.
- Files are read from disk on every iteration: the OS page cache makes
  repeats cheap, and a file changed between runs is picked up. Under high
  RPS prefer small fixture files.
- A missing/unreadable file fails the step before any request is sent.

## `std/tcp@v1`

Open a raw TCP connection, optionally send a payload, optionally read a
response, and time the whole exchange. No protocol framing — the building block
for probing arbitrary line/binary services (Redis, SMTP, custom gateways).

| Parameter | Type | Default | Description |
|---|---|---|---|
| `host` | string | **required*** | Target host (with `port`) |
| `port` | integer | **required*** | Target port |
| `address` | string | — | `host:port` shorthand, instead of `host`+`port` |
| `send` | string | — | Text payload to write after connecting |
| `send_base64` | string | — | Base64 payload for binary protocols. Mutually exclusive with `send` |
| `read` | boolean | `true` if `expect` is set, else `false` | Read one response chunk |
| `read_bytes` | integer | `65536` | Cap on bytes read |
| `expect` | string | — | Substring the response must contain (implies `read`) |
| `timeout` | integer (ms) | `10000` | Timeout for connect + exchange |

\* Provide either `address` **or** `host` + `port`.

**Output** (available via `outputs` / `__last__`):

```json
{ "connected": true, "sent": 4, "received": 4, "response": "pong", "duration_ms": 0.63 }
```

`response` is UTF-8 lossy — for binary services assert on `received` rather than
the string. Connection failures, timeouts, and an `expect` mismatch mark the
step failed and count toward `http_req_failed`; timing lands in
`http_req_duration` alongside HTTP and UDP, so percentiles are comparable across
transports.

## `std/udp@v1`

Send a UDP datagram to the target and optionally wait for a reply. Round-trip
latency is measured from send to the reply datagram (or just the send when no
reply is expected). Same `with` fields as `std/tcp@v1`, except `send`
(or `send_base64`) is **required**.

UDP is connectionless: a "successful" send only means the datagram left the
host. Set `read` (or `expect`) to actually validate a response.

**Output:**

```json
{ "sent": 4, "received": 4, "response": "pong", "duration_ms": 0.21 }
```

## WebSocket: `std/ws@v1` and the `std/ws-*@v1` family

> Concept-level walkthrough (styles, dynamic messages, assertions, metrics):
> [WebSocket guide](websocket.md).

Two ways to load-test a WebSocket endpoint:

- **One-shot session** — `std/ws@v1` opens a connection, exchanges messages,
  and closes, all in one step (like a FIX session). Simplest; the whole
  session is timed as one `http_req_duration` sample.
- **Live connection** — `std/ws-connect@v1` opens a connection that stays up
  across steps within the iteration; `ws-send` / `ws-recv` / `ws-ping` /
  `ws-close` address it by the **id** the connect step returned. Use this to
  interleave WS traffic with other steps (e.g. subscribe over WS, trigger via
  HTTP, assert the push arrives).

A connection left open at the end of an iteration is dropped abruptly (no
Close handshake) — call `std/ws-close@v1` for a graceful shutdown. Live
connections never survive into the next iteration, and `ws-connect` inside
`before:` setup is not useful (the setup context is gone before VUs start).

### Connection profile

All connect-capable steps (`std/ws@v1`, `std/ws-connect@v1`) accept the same
target parameters, inline or bundled as a profile object under `connection`
(inline fields win). A profile defined in a config `before:` step travels as
`connection: "${{ config.<name> }}"`.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `url` | string | **required** | `ws://` or `wss://` target |
| `headers` | object | — | Extra handshake headers (auth tokens etc.) |
| `subprotocols` | array \| string | — | Offered `Sec-WebSocket-Protocol` values (e.g. `graphql-ws`) |
| `skipTLSVerify` | boolean | `false` | Accept any server certificate (self-signed staging only) |
| `connection` | object \| string | — | A profile supplying defaults for any field above |

`timeout` (integer ms, default `10000`) is accepted inline by both steps but
is **not** part of the profile — a `timeout` inside `connection` is ignored.
For `std/ws-connect@v1` it bounds the handshake; for `std/ws@v1` the whole
session.

### `std/ws-connect@v1`

Opens a live connection. Output:

```json
{ "id": "ws-1", "connected": true, "subprotocol": "graphql-ws", "duration_ms": 3.1 }
```

`subprotocol` is the server-negotiated protocol, or `null` when the server
picked none. The handshake feeds `http_req_duration`; a failed handshake
counts in `http_req_failed` and the output is
`{ "connected": false, "error": "…", "duration_ms": … }`.

Store the output (`outputs: feed`) and pass `id: "${{ feed.id }}"` to the
other `ws-*` steps. Ids are minted per VU (`ws-1`, `ws-2`, …) and are valid
only inside that VU's current iteration.

### `std/ws-send@v1`

| Parameter | Type | Default | Description |
|---|---|---|---|
| `id` | string | **required** | Connection id from `std/ws-connect@v1` |
| `send` | string | one of send/send_base64 | Text payload; `${…}` tokens expand per send (see below) |
| `send_base64` | string | one of send/send_base64 | Binary payload |
| `repeat` | integer | `1` | Emit N messages from the one template |
| `interval_ms` | integer | `0` | Gap between repeated sends |
| `timeout` | integer (ms) | `10000` | For the whole send loop |

**Output**: `{ "sent": N, "bytes": B, "duration_ms": …, "metrics": { "ws_msgs_sent": N } }`.
Counts toward the `ws_msgs_sent` rate. A transport error fails the step and
drops the connection — later steps on that id get an "unknown connection id"
error. A parameter error (e.g. both `send` and `send_base64`) leaves the
connection usable.

Text payloads may embed single-brace `${…}` tokens, expanded anew per send —
distinct from the engine's `${{ … }}`, which resolves once before the action
runs:

| Token | Expands to |
|---|---|
| `${seq}` | Monotonic counter, unique per message (keeps counting across sends on the same connection) |
| `${uuid}` | Random 32-hex id |
| `${now}` | UTC `YYYYMMDD-HH:MM:SS.sss` (FIX SendingTime shape) |
| `${now_ms}` | Unix milliseconds |
| `${now_iso}` | UTC RFC 3339 `YYYY-MM-DDTHH:MM:SS.sssZ` |
| `${rand(a,b)}` | Random integer in `[a,b]` |
| `${randf(a,b[,dp])}` | Random float, `dp` decimals (default 2) |
| `${choice(x\|y\|z)}` | Random pick |

Unknown tokens are left verbatim. `send_base64` payloads are decoded once and
sent as-is — no token expansion.

### `std/ws-recv@v1`

Reads until a **stopping rule** is satisfied — not reaching it within
`timeout` fails the step:

| Parameter | Type | Default | Description |
|---|---|---|---|
| `id` | string | **required** | Connection id |
| `until_contains` | string | — | Stop when a message contains this substring (mutually exclusive with `until_json`) |
| `until_json` | object | — | Stop when a message JSON-subset-matches (pattern fields must equal; extra fields ignored) |
| `count` | integer | `1` | Without an `until_*` rule: stop after N data messages |
| `timeout` | integer (ms) | `10000` | Deadline for the stopping rule |

**Output**:

```json
{ "messages": ["…"], "body": "…", "count": 2, "matched": true, "duration_ms": 8.4,
  "metrics": { "ws_msgs_received": 2, "ws_msg_rtt": [7.9] } }
```

`matched` reports whether the stopping rule was reached (in `count` mode: the
count was reached). Text frames arrive as strings, binary frames as base64
strings; `body` is the newline-joined text form (so
`check: { body_contains: … }` works). Received messages count toward the
`ws_msgs_received` rate. When an `until_*` rule matches and a `ws-send`
preceded it on this connection, the send→match time is recorded as a
`ws_msg_rtt` histogram sample — the application-level message round trip.

Every message read along the way stays in `messages`, whatever the outcome.
If the peer closes or the transport dies before the rule is reached, the step
fails, the output gains an `error` field, and the connection is dropped; a
plain timeout fails the step too but leaves the connection usable for later
steps. Ping/pong frames arriving during the read are ignored as transport
noise.

The step's own `duration_ms` deliberately does **not** feed
`http_req_duration`: how long a server chooses to wait before pushing is not
target latency and would poison the shared percentiles.

### `std/ws-ping@v1`

Transport-level ping→pong round trip: `{ "pong": true, "duration_ms": 0.4 }`.
Takes `id` and `timeout`. The RTT is not aggregated into any histogram —
bound it with `check: { duration_ms_lt: … }` when needed. Data messages
arriving while waiting for the pong are buffered for the next `ws-recv`. No
pong within `timeout` (or a closed connection) fails the step and drops the
connection.

### `std/ws-close@v1`

Graceful close handshake. Takes `id`, `code` (default `1000`, normal
closure), `reason` (default empty) — both sent in the Close frame — and
`timeout`. Output: `{ "closed": true, "duration_ms": … }`, reported even when
the peer does not acknowledge within `timeout`: the socket is gone either way
and the id is released.

### `std/ws@v1` — one-shot session

Profile parameters as above, plus `messages` — a list where each entry is a
string (a `${…}` template to send) or an object:

| Entry field | Description |
|---|---|
| `send` / `send_base64` | Payload, as in `ws-send` |
| `repeat` / `interval_ms` | Stream expansion, as in `ws-send` |
| `until_contains` / `until_json` | Wait for a matching reply before the next entry; yields a `ws_msg_rtt` sample |

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
    outputs: feed
```

**Output**: `{ "connected": true, "sent": N, "received": M, "messages": […],
"body": "…", "subprotocol": …, "duration_ms": …, "metrics": { "ws_msgs_sent": N, "ws_msgs_received": M, "ws_msg_rtt": […] } }`.
The whole session is one `http_req_duration` sample; the step fails on
handshake/transport errors or any entry whose `until_*` rule did not match in
time. A mid-session failure still reports everything exchanged up to that
point, plus an `error` field naming the failing entry (`message[i]: …`); a
handshake failure yields `{ "connected": false, "error": "…", "duration_ms": … }`
and counts in `http_req_failed`.

### Limits

Inbound protocol limits come from the WebSocket library defaults: messages up
to 64 MiB, single frames up to 16 MiB — a larger inbound message errors the
connection (and therefore the step reading it). Timeout values, `repeat`
counts, and the `messages` list have no built-in caps.

## gRPC: `std/grpc@v1` and the `std/grpc-*@v1` family

> Concept-level walkthrough (schema sources, streams, assertions, metrics):
> [gRPC guide](grpc.md).

Two ways to load-test a gRPC endpoint:

- **One-shot call** — `std/grpc@v1` opens a channel, loads the schema, makes
  one unary call, and closes, all in one step. Simplest for occasional probes.
- **Live channel** — `std/grpc-connect@v1` opens an HTTP/2 channel that stays
  up across steps within the iteration; `grpc-call` (unary) and the
  `grpc-stream-*` family address it by the **id** the connect step returned.
  Use this for any serious load: the connection and the schema load are paid
  once per iteration, not per call.

Calls are **dynamic**: no protobuf codegen — the schema arrives at run time
and requests/responses are JSON (protobuf-JSON rules: field names accept both
the proto name and its camelCase `json_name`; 64-bit ints are strings; enums
are names).

A channel left open at the end of an iteration is dropped — Live Channels and
streams never survive into the next iteration, and `grpc-connect` inside
`before:` setup is not useful (the setup context is gone before VUs start).

### Channel profile

`std/grpc@v1` and `std/grpc-connect@v1` accept the same target parameters,
inline or bundled as a profile object under `connection` (inline fields win).
A profile defined in a config `before:` step travels as
`connection: "${{ config.<name> }}"`.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `url` | string | **required** | `grpc://` (plaintext) or `grpcs://` (TLS) target; a scheme-less host means `grpcs://` |
| `metadata` | object | — | Default call metadata (auth tokens etc.); per-call `metadata` overrides per key |
| `skipTLSVerify` | boolean | `false` | Accept any server certificate (self-signed staging only) |
| `descriptor_set` | string | one schema source | Base64 of a serialized `FileDescriptorSet` (mutually exclusive with `reflection`) |
| `reflection` | boolean | one schema source | `true`: fetch the schema via the server reflection service (v1) |
| `max_recv_size` | integer (bytes) | `16777216` | Inbound message cap (16 MiB) |
| `connection` | object \| string | — | A profile supplying defaults for any field above |

Exactly one schema source is required. `descriptor_set` is a base64
`FileDescriptorSet` — produce one with `protoc --descriptor_set_out`, or fetch
it over HTTP and reference `${{ fetch.body_base64 }}`. With `reflection: true`
the server must enable gRPC reflection; the fetched pool is cached per URL for
the rest of the run, so repeated connects to one server pay one round trip.

`timeout` (integer ms, default `10000`) is accepted inline by both steps but
is **not** part of the profile. For `std/grpc-connect@v1` it bounds connect +
schema load; for `std/grpc@v1` the whole step (connect → schema → call; the
call's `grpc-timeout` header is the remaining budget).

Methods are named `"package.Service/Method"`. A typo fails with a did-you-mean
suggestion when a known method is within edit distance 2.

### Payloads

Requests take `payload` (JSON → dynamic protobuf message) or `payload_base64`
(base64 of the serialized protobuf bytes) — mutually exclusive. String leaves
of `payload` may embed the single-brace `${…}` tokens documented under
[`std/ws-send@v1`](#stdws-sendv1), expanded per call/send (`${seq}` keeps
counting per channel/stream). Responses appear in `body` as JSON under the
same mapping rules.

### `std/grpc-connect@v1`

Opens a live channel and loads the schema. Output:

```json
{ "id": "grpc-1", "connected": true, "duration_ms": 4.8 }
```

Store the output (`outputs: conn`) and pass `id: "${{ conn.id }}"` to the
other `grpc-*` steps. Ids are minted per VU (`grpc-1`, `grpc-2`, …) and are
valid only inside that VU's current iteration. A failed connect or schema
load yields `{ "connected": false, "error": "…", "duration_ms": … }`.

### `std/grpc-call@v1`

One unary call on a live channel.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `id` | string | **required** | Channel id from `std/grpc-connect@v1` |
| `method` | string | **required** | `"package.Service/Method"` (unary methods only) |
| `payload` | object \| array \| string | one of payload/payload_base64 | JSON request message; `${…}` tokens expand per call |
| `payload_base64` | string | one of payload/payload_base64 | Serialized protobuf bytes |
| `metadata` | object | — | Per-call metadata (overrides channel defaults per key) |
| `expect_status` | integer | `0` | Expected gRPC status code — the step fails on any other |
| `timeout` | integer (ms) | `10000` | Sent as `grpc-timeout` and enforced locally |

**Output**:

```json
{ "status": 0, "body": { "message": "hello" }, "duration_ms": 2.4,
  "metrics": { "grpc_req_duration": [2.4], "grpc_msgs_sent": 1,
               "grpc_msgs_received": 1, "grpc_msg_rtt": [2.3], "grpc_req_failed": 0 } }
```

On a non-zero status the output carries `error` (the status message) instead
of `body`. `expect_status` makes error-path tests read naturally:
`expect_status: 5` passes when the server returns NOT_FOUND and fails on OK.
A failed RPC fails the step but the channel stays usable (HTTP/2 channels
recover; a dead WebSocket does not). Unary metrics: `grpc_req_duration` and
`grpc_msg_rtt` histograms, `grpc_msgs_sent` / `grpc_msgs_received` counters,
`grpc_req_failed` for RPCs that did not meet `expect_status`.

### `std/grpc@v1` — one-shot call

Channel-profile and call parameters in one step; connects, loads the schema,
calls, closes. Same output as `std/grpc-call@v1`.

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

### `std/grpc-stream-open@v1`

Starts a client-streaming, bidi, or server-streaming call on a live channel
and returns a **stream id** (`grpcs-1`, …).

| Parameter | Type | Default | Description |
|---|---|---|---|
| `id` | string | **required** | Channel id |
| `method` | string | **required** | `"package.Service/Method"` (streaming methods only) |
| `payload` | object | server-streaming: required | The single request message for server-streaming methods |
| `payload_base64` | string | — | Serialized form of the above |
| `metadata` | object | — | Per-call metadata |

**Output**: `{ "id": "grpcs-1", "kind": "server"|"client"|"bidi",
"open": true, "duration_ms": … }`.

For server-streaming, the one request goes out at open; for
client-streaming/bidi, messages go out via `std/grpc-stream-send@v1` (passing
`payload` at open is an error). Open returns immediately — the call runs in a
relay task, because a client-streaming server sends its initial metadata only
after the client half-closes. A server-side failure (UNIMPLEMENTED, auth,
…) therefore surfaces at the first recv/close, not at open.

### `std/grpc-stream-send@v1`

| Parameter | Type | Default | Description |
|---|---|---|---|
| `id` | string | **required** | Stream id |
| `payload` | object | one of payload/payload_base64 | JSON message; `${…}` tokens expand per send |
| `payload_base64` | string | one of payload/payload_base64 | Serialized protobuf bytes |
| `repeat` | integer | `1` | Emit N messages from the one template |
| `interval_ms` | integer | `0` | Gap between repeated sends |
| `timeout` | integer (ms) | `10000` | For the whole send loop |

**Output**: `{ "sent": N, "duration_ms": …, "metrics": { "grpc_msgs_sent": N } }`.
Sending on a server-streaming stream is a parameter error (the stream stays
usable). A send that fails because the peer ended the call fails the step and
drops the stream id.

### `std/grpc-stream-recv@v1`

Reads until a **stopping rule** is satisfied — not reaching it within
`timeout` fails the step:

| Parameter | Type | Default | Description |
|---|---|---|---|
| `id` | string | **required** | Stream id |
| `until_contains` | string | — | Stop when a message contains this substring (objects: compact-JSON form) |
| `until_json` | object | — | Stop when a message JSON-subset-matches (mutually exclusive with `until_contains`) |
| `count` | integer | `1` | Without an `until_*` rule: stop after N messages |
| `timeout` | integer (ms) | `10000` | Deadline for the stopping rule |

**Output**:

```json
{ "messages": [ { "message": "hello" } ], "count": 1, "matched": true,
  "duration_ms": 3.1,
  "metrics": { "grpc_msgs_received": 1, "grpc_msg_rtt": [2.9] } }
```

`grpc_msg_rtt` appears only when an `until_*` rule matched and a
`grpc-stream-send` preceded it on this stream — the send→match application
RTT. A plain timeout fails the step but leaves the stream usable; the stream
ending (cleanly or with a status) before the rule is reached fails the step
and drops the stream.

### `std/grpc-stream-close@v1`

Half-closes the request side (client-streaming/bidi: the server sees
end-of-input) and drains remaining server messages until the final status,
bounded by `timeout`.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `id` | string | **required** | Stream id |
| `expect_status` | integer | `0` | Expected final gRPC status code |
| `timeout` | integer (ms) | `10000` | Drain deadline |

**Output**: `{ "closed": true, "status": 0, "received": N, "messages": […],
"duration_ms": …, "metrics": { "grpc_msgs_received": N, "grpc_req_failed": 0|1 } }`.
For a client-streaming method the drained single message is the call's
response. The stream id is released either way.

```yaml
steps:
  - name: open channel
    use: std/grpc-connect@v1
    with:
      url: grpcs://api.example.com:443
      reflection: true
      metadata: { authorization: "Bearer ${{ vars.token }}" }
    outputs: conn

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
    outputs: got

  - name: close stream
    use: std/grpc-stream-close@v1
    with: { id: "${{ stream.id }}" }
```

### gRPC limits

Inbound messages are capped by `max_recv_size` (default 16 MiB); an oversized
message fails the call with RESOURCE_EXCEEDED. Binary (`-bin`) metadata keys
are not supported — metadata values are strings. Stream lifetimes span user
steps, so streams deliberately do not feed `grpc_req_duration` (only unary
calls do); `grpc-stream-close` is what turns a stream's final status into
`grpc_req_failed`. Timeouts, `repeat` counts, and drain lengths have no
built-in caps.

## Database: the `std/db-*@v1` family

SQL load tests against **PostgreSQL**, **MySQL/MariaDB**, and **SQLite**.
Two styles, mirroring the WebSocket family: a live connection held across
steps (`db-connect` → `db-query` / transactions → `db-close`), and a
`per-query` mode where every query pays connect + query (see
[Connection modes](#db-connection-modes)).

```yaml
steps:
  - name: open db
    use: std/db-connect@v1
    with:
      driver: postgres
      dsn: "${{ vars.db_dsn }}"      # keep the password out of the file
    outputs: db

  - name: begin tx
    use: std/db-tx-begin@v1
    with: { id: "${{ db.id }}" }

  - name: write
    use: std/db-query@v1
    with:
      id: "${{ db.id }}"
      query: INSERT INTO hits (path, status) VALUES ($1, $2)
      params: ["/api/checkout", 200]

  - name: commit
    use: std/db-tx-commit@v1
    with: { id: "${{ db.id }}" }

  - name: hang up
    use: std/db-close@v1
    with: { id: "${{ db.id }}" }
```

### `std/db-connect@v1`

Opens a connection pool (persistent mode) or stores the parsed connect
config (per-query mode), and returns the Connection ID.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `driver` | string | **required** | `postgres`, `mysql` (covers MariaDB), or `sqlite` |
| `dsn` | string | **required** | Driver-native connection string, e.g. `postgres://user:pass@host:5432/db`, `mysql://user:pass@host:3306/db`, `sqlite://data.db?mode=rwc`, `sqlite::memory:`. May use `${{ … }}` interpolation for secrets |
| `tls` | bool \| string | `true` | `true` verifies certificate + hostname; `false` is plaintext; `"skip-verify"` encrypts without verification. Ignored for sqlite |
| `mode` | string | `persistent` | `persistent` (pool held across steps) or `per-query` (config only — each query connects fresh) |
| `pool_size` | integer | `1` | Persistent-mode pool size (max 1024) |
| `timeout_ms` | integer | `30000` | Connect timeout |

**Output**: `{ "id": "db-1", "driver": "postgres", "mode": "persistent", "connected": true, "duration_ms": …, "metrics": … }`.
In per-query mode `connected` is `false` — nothing is opened yet. The DSN
and its password are never logged; log lines use a sanitized
`host:port/database` label.

### `std/db-query@v1`

Runs one parameterized statement. INSERT/UPDATE/DELETE/DDL are allowed by
default.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `id` | string | **required** | Connection ID from `std/db-connect@v1` (`id: "${{ db.id }}"`) |
| `query` | string | **required** | SQL with driver-native placeholders (`$1`, `$2`, … for postgres; `?` for mysql/sqlite). **Never interpolated** — a `${{` in the SQL reaches the database verbatim. 64 KiB hard limit (`max_query_bytes`) |
| `params` | array | `[]` | Positional bind values; each entry *is* interpolated. Strings → text, numbers → i64/f64, booleans → bool, null → typed NULL (text-typed on postgres — cast in SQL, e.g. `$1::int`). Arrays/objects cannot be bound |
| `max_rows` | integer | `10000` | Hard cap on rows read into memory; extra rows only set `truncated: true` |
| `timeout_ms` | integer | `30000` | Query timeout (per-query mode: connect + query) |

**Output**: `{ "rows": <u64>, "rows_affected": <u64>, "truncated": <bool>, "data": [ … ], "duration_ms": …, "metrics": … }`.
Row cells decode as booleans/integers/floats/text/JSON/base64 (binary);
unmapped types (NUMERIC, temporal, UUID, arrays) surface as `null` — the
row still counts. With a transaction open on the connection the query runs
inside it. A failed query fails the step but leaves the connection — and
any open transaction — parked.

### `std/db-tx-begin@v1`, `std/db-tx-commit@v1`, `std/db-tx-rollback@v1`

Transaction context on a persistent connection (`id` parameter;
`timeout_ms` applies). Each step is timed as one unit (`duration_ms` +
`db_query_duration`). One open transaction per connection — begin while one
is open, or commit/rollback with none, fails the step. Transactions are
rejected on per-query ids.

### `std/db-close@v1`

Closes the connection and releases its id (`id` parameter). An open
transaction rolls back as it drops. Connections left open are closed
implicitly at iteration end.

### DB connection modes

**persistent** (default): `db-connect` opens a pool (`pool_size`, default
1 — steps within a VU run strictly sequentially) held across the VU's
steps; `db_query_duration` measures the query alone.

**per-query**: the connect step only validates and stores the config; every
`db-query` opens a fresh connection, queries, and closes, so
`db_query_duration` measures connect + query as one unit — the shape of
serverless drivers and connect-time benchmarking. Transactions are
unavailable. `sqlite::memory:` is pointless here (each fresh connection is
an empty database) — use a file DSN; conversely an in-memory pool must
stay at `pool_size: 1`.

### DB metrics

| Name | Type | Emitted by | Meaning |
|---|---|---|---|
| `db_connect_duration` | histogram | `std/db-connect@v1` (success) | Connect + pool setup latency |
| `db_query_duration` | histogram | `std/db-query@v1`, tx steps | Query latency; includes the fresh connect in per-query mode |
| `db_rows` | counter | `std/db-query@v1` | Rows returned, or rows affected when the statement returned none |
| `db_errors` | counter | all DB steps (failure) | Failed DB steps, total |
| `db_errors_{connection,constraint,deadlock,timeout,other}` | counter | all DB steps (failure) | Same, split by class (SQLSTATE / errno / SQLite result code; also in the step output's `error_kind`) |

Metric labels carry the step name only — never raw SQL.

## `std/file-read@v1`

Read a file into the process-wide cache and expose its content to later
steps. The first access pays the disk read; every following iteration —
across all VUs — is served from RAM. The cache revalidates against the
file's `(mtime, size)` on each access, so a file edited between runs of a
long-lived agent is picked up automatically.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `path` | string | **required** | File to read |
| `encoding` | string | `text` | `text` (file must be valid UTF-8) or `base64` (binary content) |

**Output** (available via `outputs` / `__last__`):

```json
{ "content": "...", "size": 1024, "path": "./fixtures/payload.json" }
```

```yaml
steps:
  - name: load payload
    use: std/file-read@v1
    with: { path: ./fixtures/payload.json }
    outputs: payload

  - name: send it
    use: std/http@v1
    with:
      method: POST
      url: "https://api.example.com/items"
      headers: { content-type: application/json }
      body: "${{ payload.content }}"
    check:
      status: 201
```

Notes:

- A non-UTF-8 file with `encoding: text` fails the step — use
  `encoding: base64` for binary content.
- Emits no per-iteration log lines: cache hits are the hot path.
- Referencing `${{ payload.content }}` copies the content into the request —
  the cache saves disk reads, not the per-request copy. Keep fixtures small
  under high RPS.
- For file *uploads*, prefer `std/http@v1`'s [`multipart`](#multipart-uploads)
  parameter, which sends proper `multipart/form-data`.

## `std/file-write@v1`

Write content to a file — typically to persist a previous step's response.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `path` | string | **required** | File to write (parent directory must exist) |
| `content` | string | **required** | Data to write; `${{ ... }}` placeholders make `${{ resp.body }}` the typical payload |
| `encoding` | string | `text` | `text` writes the string as-is; `base64` decodes it first (the inverse of `file-read`'s base64) |
| `append` | boolean | `false` | Append instead of overwrite |

**Output**: `{ "path": <string>, "size": <bytes written> }`

```yaml
steps:
  - name: fetch report
    use: std/http@v1
    with: { url: "https://api.example.com/report" }
    outputs: resp

  - name: save it
    use: std/file-write@v1
    with:
      path: ./out/report.json
      content: "${{ resp.body }}"
```

Notes:

- Writing a path that `std/file-read@v1` has cached invalidates that cache
  entry automatically — the read cache revalidates by `(mtime, size)`.
- With `append: true` each call is a single `O_APPEND` write, so concurrent
  VUs do not interleave mid-content; ordering between VUs is unspecified.
- Emits no per-iteration log lines.

## `std/child_process@v1`

Spawn a local OS process and keep it alive across the run — a mock server, a
fixture database, a sidecar the system under test needs. Typical home is the
config's `before:` block (start) paired with `after:` (stop); usable in test
steps too. **Fail-closed**: the run must opt in with
`allow_process_actions: true` in the config.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `command` | string | **required** | Executable to spawn |
| `args` | array of strings | `[]` | Command arguments |
| `env` | object | — | Extra environment variables (`{ "NAME": "value" }`, string values only) |
| `cwd` | string | — | Working directory of the child |
| `port` | integer | — | Port the process answers on (echoed to outputs); `0` auto-assigns a free port and exports it to the child as the `PORT` env var |
| `restart` | string | `never` | Restart policy: `never`, `on-failure` (non-zero exit or signal), `always` |
| `max_restarts` | integer | `3` | Restart budget for the policy |
| `backoff_ms` | integer | `1000` | Delay before each restart |
| `buffer_kb` | integer | `64` | Captured stdout/stderr tail size per stream (KiB) |
| `waitUntil` | object \| string | — | Readiness gate — see below |

**Output** (available via `outputs` / `__last__`):

```json
{ "pid": 12345, "ppid": 12300, "pgid": 12345, "port": 8080,
  "stdout": "<tail>", "stderr": "<tail>", "restart_count": 0 }
```

`ppid` is the perfscale process itself; `pgid` is the child's process group —
every child leads its own group, so a tree kill can never hit perfscale.
`port` is present only when the step declared one. `pid`/`pgid`/
`restart_count` are a **snapshot**: they move on restarts, so stop processes
by registry name ([`std/kill_process@v1`](#stdkill_processv1)), not by a
captured pid.

The child keeps running after the step returns. Its output streams into the
run log with a `{step}: ` prefix (same shape as the k6 runner) and accumulates
in the bounded tail buffers behind `stdout`/`stderr`. A supervisor applies the
restart policy for the rest of the run and logs each restart. Whatever is
still alive when the run ends — normal finish, failed `before:`, or Ctrl-C —
is **stopped automatically** (SIGTERM, escalated to SIGKILL after a grace
period, whole process group), so a forgotten `kill_process` never leaks a
server.

### waitUntil — readiness gate

Blocks the step until the process is ready; every listed matcher must hold.
Object form:

| Field | Type | Description |
|---|---|---|
| `stdout_contains` | string | Substring in captured stdout |
| `stderr_contains` | string | Substring in captured stderr |
| `stdout_matches` | string (regex) | Regex matched against captured stdout |
| `stderr_matches` | string (regex) | Regex matched against captured stderr |
| `port_open` | integer | A TCP connect to `127.0.0.1:<port>` succeeds; `0` probes the step's own `port` |
| `timeout` | string | Duration like `30s`/`1m` (default `30s`) |
| `on_timeout` | string | `fail` (default) fails the step; `continue` logs the miss and goes on |

A process that exits before becoming ready fails the step with its exit code
and an stderr tail. The string form covers the common one-matcher case:
`waitUntil: 'contains(stdout, "Serving HTTP")'`, `'matches(stderr, "re")'`,
`'port_open(8080)'`.

```yaml
allow_process_actions: true

before:
  - name: web
    uses: std/child_process@v1
    with:
      command: python3
      args: ["-m", "http.server", "8080"]
      waitUntil:
        port_open: 8080
        timeout: 10s
      restart: on-failure
    outputs: web          # → ${{ config.web.pid }}, ${{ config.web.port }}
```

## `std/kill_process@v1`

Stop a process started with `std/child_process@v1` — the `after:` counterpart.
Same `allow_process_actions` gate.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `name` | string | one of name/pid | Registry name of a managed process (its step name or `outputs` name). Preferred: the lookup always targets the *current* pid, even across restarts |
| `pid` | integer | one of name/pid | Raw OS pid — best-effort fallback for processes outside the registry (unix only) |
| `signal` | string | `TERM` | `TERM`, `KILL`, `INT`, `HUP`, `QUIT`, `USR1`, `USR2` (a `SIG` prefix is accepted) |
| `grace_ms` | integer | `5000` | Wait this long for the process to die before escalating to SIGKILL |
| `tree` | boolean | `true` | Signal the whole process group, not just the leader |

**Output**: `{ "pid": 12345, "signal": "TERM", "exit_code": null, "waited_ms": 12 }`

`exit_code` is `null` when the process died to a signal, and always `null`
for raw pids (a non-child's exit status cannot be collected). Killing an
already-stopped process is a no-op, not an error; the run's end-of-run
auto-kill covers anything not stopped explicitly.

```yaml
after:
  - name: stop web
    uses: std/kill_process@v1
    with: { name: web, signal: TERM }
```

On non-unix platforms there are no POSIX signals or process groups:
`std/kill_process@v1` by `name` terminates the direct child only (`tree` has
no effect), and `pid:` is unsupported.

## `std/thresholds@v1`

Run-level SLO gates over the metrics a run collected — k6-style threshold
expressions evaluated **once**, after every VU has stopped. Its home is the
config's `after:` block:

```yaml
after:
  - name: slo gate
    use: std/thresholds@v1
    with:
      db_query_duration: ["p95<500", "max<2000"]
      db_query_failed: ["rate<0.05"]
      db_errors: ["count==0"]
      http_req_duration: ["avg<300"]
    severity: fail            # fail (default) | warn | info
    message: "checkout SLO"   # optional, interpolated
```

| Parameter | Type | Default | Description |
|---|---|---|---|
| `with.<metric>` | string or array of strings | **required (≥1)** | Threshold expressions for a run metric |
| `severity` | string | `fail` | Step-level field: what a violated gate becomes — `fail` (the run exits non-zero), `warn`, or `info` (both exit zero) |
| `message` | string | — | Step-level field: custom label appended to the violation summary; interpolated |

**Expressions** are `<agg><op><number>`: agg ∈ `avg`, `min`, `max`, `p50`,
`p90`, `p95`, `p99`, `count`, `rate`; op ∈ `<`, `<=`, `>`, `>=`, `==`, `!=`;
the number is a plain float (int or decimal, no units). Whitespace around the
parts is tolerated (`p95 < 500` parses).

**Which aggregates apply** depends on the metric kind:

- *Sample metrics* (`db_query_duration`, `http_req_duration`, `ws_msg_rtt`, …):
  `avg`/`min`/`max`/`p50`/`p90`/`p95`/`p99` are computed over **all** samples
  emitted during the run, from the same HDR histograms the end-of-run summary
  prints — gate numbers match the summary. `count` is the number of samples.
- *Counter metrics* (`db_errors`, `db_rows`, …): only `count`, the counter's
  final value.
- *Failure metrics* (`http_req_failed`, `db_query_failed`, …): `rate` is
  failed/total invocations in `0.0..=1.0`; `count` is the invocation count.
  The runner derives these automatically — every step invocation records a
  0/1 sample under `<family>_failed` for each duration metric it emitted
  (`db_query_duration` → `db_query_failed`). See
  [metrics.md](metrics.md#failure-rate-metrics).

**Errors are hard.** An unknown metric name fails the gate — and the run —
with an error listing the metrics that *are* present; so does an empty
`with:`, an unparseable expression, or an aggregate that doesn't apply to the
metric kind. A broken gate never silently passes CI.

**Output** (available via `outputs` / `__last__`):

```json
{ "status": "fail",
  "message": "db_query_duration p95=612ms ≥ 500ms; checkout SLO",
  "violations": [{ "metric": "db_query_duration", "expr": "p95<500", "actual": 612.0 }] }
```

`status` is `pass` when every expression holds, else the `severity` value.
`message` is the violation summary joined with `; ` plus the custom message,
truncated at 200 chars with `…`. The run summary JSON
(`perfscale run --summary-export`) gains a `thresholds` field with the same
shape (several gates combine: worst status wins).

## `std/check@v1`

Assert properties of a previous step's output. Usually written as a step's
inline `check:` block, which runs this action against that step's output;
standalone usage picks its target with `on`.

| Parameter | Type | Description |
|---|---|---|
| `on` | string | What to check (defaults to the last step's output). Dots descend into the value: `on: got.messages.0` addresses one message by position |
| `status` | integer | HTTP status must equal this value |
| `duration_ms_lt` | integer | `duration_ms` must be strictly less |
| `body_contains` | string | Response body must contain this substring |
| `message_contains` | string | Some message in the `messages` list contains this substring |
| `message_matches` | object | Some message JSON-subset-matches this object |
| `messages_count_gte` | integer | The `messages` list has at least N entries |

The `message_*` asserts work over the `messages` list that message-exchanging
actions expose (`std/ws@v1`, `std/ws-recv@v1`, `pro/fix@v1`) and use the
**any** quantifier — at least one message must match, because streams carry
noise (heartbeats, unrelated events). WS text frames (strings) are parsed as
JSON for `message_matches`; FIX frames (tag→value objects) match directly, so
`message_matches: { "35": "8", "150": "F" }` asserts a filled
ExecutionReport. For deterministic exchanges, address one message by index
via `on` (`on: got.messages.0`) — brittle on unordered streams.

Each assertion logs `PASS`/`FAIL`; failures go to stderr but do not stop the
run. Output: `{ "passed": true|false }`.

## `std/sleep@v1`

Pause the current VU.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `ms` | integer | `1000` | Milliseconds to sleep |
| `seconds` | number | — | Alternative to `ms` (fractions allowed) |

## `std/log@v1`

Emit a line to stdout — mostly useful with interpolation:

| Parameter | Type | Description |
|---|---|---|
| `message` | string | Text to emit; `${{ var.field }}` references are resolved first |

## Interpolation rules

All string leaves in `with`/`check` are interpolated before the action runs:

- `${{ name }}` — the stored value, stringified
- `${{ name.field }}` — one field of a stored JSON object
- unknown names resolve to `""`; an unterminated `${{` is left as-is

## Adding a new action (contributors)

1. Implement it in `crates/perfscale-core/src/step/actions.rs` and add a
   dispatch arm in `execute_action`.
2. Return an `ActionOutput`: stored `value`, log lines, `success`, and an
   `http_sample` if the action performs HTTP work that should count toward
   metrics.
3. Add unit tests next to the existing ones (wiremock is available for HTTP).
4. Document it here and, if it introduces new step fields, regenerate the
   schemas (`cargo run -p perfscale-core --example gen_schema`).

## Custom actions from downstream crates

Actions that shouldn't live in this OSS crate — higher-tier or proprietary
protocols such as the FIX action `pro/fix@v1` — plug in without a fork.
Implement the `perfscale_core::step::actions::ActionHandler` trait in your own
crate and call `register_action(Arc::new(MyHandler))` once at process start
(e.g. in the agent's `main`). Registered handlers are consulted only for action
IDs no built-in `std/*` action matches, so built-ins pay no lookup cost. Params
reach the handler with `${{ }}` interpolation already applied.
