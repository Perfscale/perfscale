# Built-in actions

Actions are the units a native test's `steps` are made of. Each step names an
action in `use`, passes parameters in `with`, and may assert on the result in
`check`.

Full IDs carry a namespace and version (`std/http@v1`); the short aliases
(`http`, `check`, `sleep`, `log`, `file-read`, `file-write`) resolve to the same implementations.

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
{ "status": 200, "body": "...", "duration_ms": 42.37 }
```

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
| `on` | string | Variable name to check (defaults to the last step's output) |
| `status` | integer | HTTP status must equal this value |
| `duration_ms_lt` | integer | `duration_ms` must be strictly less |
| `body_contains` | string | Response body must contain this substring |

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
