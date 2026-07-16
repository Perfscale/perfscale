# Built-in actions

Actions are the units a native test's `steps` are made of. Each step names an
action in `use`, passes parameters in `with`, and may assert on the result in
`check`.

Full IDs carry a namespace and version (`std/http@v1`); the short aliases
(`http`, `tcp`, `udp`, `ws`, `ws-connect`, `ws-send`, `ws-recv`, `ws-ping`,
`ws-close`, `check`, `sleep`, `log`, `file-read`, `file-write`) resolve to the
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
| `timeout` | integer (ms) | `10000` | Handshake timeout (for `std/ws@v1`: the whole session) |

### `std/ws-connect@v1`

Opens a live connection. Output:

```json
{ "id": "ws-1", "connected": true, "subprotocol": "graphql-ws", "duration_ms": 3.1 }
```

The handshake feeds `http_req_duration`. Store the output (`outputs: feed`)
and pass `id: "${{ feed.id }}"` to the other `ws-*` steps.

### `std/ws-send@v1`

| Parameter | Type | Default | Description |
|---|---|---|---|
| `id` | string | **required** | Connection id from `std/ws-connect@v1` |
| `send` | string | one of send/send_base64 | Text payload; `${…}` tokens expand per send (see below) |
| `send_base64` | string | one of send/send_base64 | Binary payload |
| `repeat` | integer | `1` | Emit N messages from the one template |
| `interval_ms` | integer | `0` | Gap between repeated sends |
| `timeout` | integer (ms) | `10000` | For the whole send loop |

**Output**: `{ "sent": N, "bytes": B, "duration_ms": … }`. Counts toward the
`ws_msgs_sent` rate.

Text payloads may embed single-brace `${…}` tokens, expanded anew per send —
distinct from the engine's `${{ … }}`, which resolves once before the action
runs:

| Token | Expands to |
|---|---|
| `${seq}` | Monotonic counter, unique per message |
| `${uuid}` | Random 32-hex id |
| `${now}` | UTC `YYYYMMDD-HH:MM:SS.sss` (FIX SendingTime shape) |
| `${now_ms}` | Unix milliseconds |
| `${now_iso}` | UTC RFC 3339 `YYYY-MM-DDTHH:MM:SS.sssZ` |
| `${rand(a,b)}` | Random integer in `[a,b]` |
| `${randf(a,b[,dp])}` | Random float, `dp` decimals (default 2) |
| `${choice(x\|y\|z)}` | Random pick |

### `std/ws-recv@v1`

Reads until a **stopping rule** is satisfied — not reaching it within
`timeout` fails the step:

| Parameter | Type | Default | Description |
|---|---|---|---|
| `id` | string | **required** | Connection id |
| `until_contains` | string | — | Stop when a message contains this substring |
| `until_json` | object | — | Stop when a message JSON-subset-matches (pattern fields must equal; extra fields ignored) |
| `count` | integer | `1` | Without an `until_*` rule: stop after N data messages |
| `timeout` | integer (ms) | `10000` | Deadline for the stopping rule |

**Output**:

```json
{ "messages": ["…"], "body": "…", "count": 2, "matched": true, "duration_ms": 8.4 }
```

Text frames arrive as strings, binary frames as base64 strings; `body` is the
newline-joined text form (so `check: { body_contains: … }` works). Received
messages count toward the `ws_msgs_received` rate. When an `until_*` rule
matches and a `ws-send` preceded it on this connection, the send→match time
is recorded as a `ws_msg_rtt` histogram sample — the application-level
message round trip.

The step's own `duration_ms` deliberately does **not** feed
`http_req_duration`: how long a server chooses to wait before pushing is not
target latency and would poison the shared percentiles.

### `std/ws-ping@v1`

Transport-level ping→pong round trip: `{ "pong": true, "duration_ms": 0.4 }`.
Takes `id` and `timeout`. The RTT is not aggregated into any histogram —
bound it with `check: { duration_ms_lt: … }` when needed. Data messages
arriving while waiting for the pong are buffered for the next `ws-recv`.

### `std/ws-close@v1`

Graceful close handshake. Takes `id`, `code` (default `1000`), `reason`,
`timeout`. Output: `{ "closed": true, "duration_ms": … }`.

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
"body": "…", "subprotocol": …, "duration_ms": … }`. The whole session is one
`http_req_duration` sample; the step fails on handshake/transport errors or
any entry whose `until_*` rule did not match in time.

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
