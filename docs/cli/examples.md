# Recipes

Runnable sample files for every engine live in [`examples/`](../../examples/).

## Smoke-test an API before merging

```yaml
# smoke.test.yaml
steps:
  - name: health
    use: std/http@v1
    with: { url: "https://staging.example.com/health" }
    check: { status: 200, duration_ms_lt: 300 }
```

```yaml
# smoke.config.yaml
vus: 5
duration: 30s
```

```sh
perfscale run -f smoke.test.yaml -c smoke.config.yaml
```

## Login → authenticated request (chained steps)

```yaml
steps:
  - name: login
    use: std/http@v1
    with:
      method: POST
      url: https://api.example.com/login
      body: { user: demo, password: demo }
    check: { status: 200 }
    outputs: login

  - name: profile
    use: std/http@v1
    with:
      url: https://api.example.com/me
      headers:
        authorization: "Bearer ${{ login.body }}"
    check: { status: 200, body_contains: "demo" }

  - use: std/sleep@v1
    with: { ms: 500 }
```

## Load-test a WebSocket endpoint

[`examples/websocket.test.yaml`](../../examples/websocket.test.yaml) shows both
styles — a live connection held across steps and a one-shot session. The live
style addresses the connection by the id `std/ws-connect@v1` returned:

```yaml
steps:
  - name: open feed
    use: std/ws-connect@v1
    with: { url: ws://127.0.0.1:9222 }
    outputs: feed

  - name: subscribe
    use: std/ws-send@v1
    with:
      id: "${{ feed.id }}"
      send: '{"op":"subscribe","id":"sub-${seq}"}'

  - name: await echo
    use: std/ws-recv@v1
    with: { id: "${{ feed.id }}", until_contains: "sub-1", timeout: 5000 }
    check: { message_contains: "subscribe" }

  - name: hang up
    use: std/ws-close@v1
    with: { id: "${{ feed.id }}" }
```

Run it against any echo server (`npx wscat --listen 9222`). See the
[WebSocket guide](../core/websocket.md) for concepts and
[Actions → WebSocket](../core/actions.md#websocket-stdwsv1-and-the-stdws-v1-family)
for the full parameter and metrics reference.

## Load-test a gRPC endpoint

[`examples/grpc.test.yaml`](../../examples/grpc.test.yaml) drives unary calls
and a bidi stream over one live channel. No protobuf codegen: the schema is
loaded at run time via server reflection (or a base64 `descriptor_set`), and
payloads are plain JSON mapped by protobuf-JSON rules. The channel and its
schema are paid once per iteration — calls and streams ride the same HTTP/2
connection:

```yaml
steps:
  - name: open channel
    use: std/grpc-connect@v1
    with:
      url: grpc://127.0.0.1:50051
      reflection: true
    outputs: conn

  - name: unary echo
    use: std/grpc-call@v1
    with:
      id: "${{ conn.id }}"
      method: "perfscale.test.v1.Echo/Unary"
      payload: { message: "ping-${seq}" }
    check:
      duration_ms_lt: 250

  - name: open bidi stream
    use: std/grpc-stream-open@v1
    with:
      id: "${{ conn.id }}"
      method: "perfscale.test.v1.Echo/Bidi"
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

Run it against the bundled echo server
(`cargo run -p perfscale-core --example grpc_echo_server`). For occasional
probes there is also the one-shot `std/grpc@v1` (connect → schema → call →
close in one step). See the [gRPC guide](../core/grpc.md) for concepts and
[Actions → gRPC](../core/actions.md#grpc-stdgrpcv1-and-the-stdgrpc-v1-family)
for the full parameter and metrics reference.

## Reuse an existing k6 script

```sh
perfscale run --k6 load-tests/checkout.js
```

Load configuration (VUs, stages, thresholds) stays in the script's `options`
block — perfscale streams the output and returns k6's exit code semantics.

## Reuse an existing locustfile

```sh
perfscale run --locust locustfile.py --host https://target.example.com -c load.config.yaml
```

`vus`/`duration` from the config map to `--users`/`--spawn-rate`/`--run-time`.
After the run, locust's CSV stats are converted to the same summary block the
other engines print.

## Collect results from several terminals / machines

```sh
# terminal 1 — collector
perfscale serve --port 7999

# terminals 2..N — each run reports in
perfscale run -f test.yaml -c config.yaml --report http://collector-host:7999
```

Or bake the collector into the config so the flag isn't needed:

```yaml
# config.yaml
vus: 10
duration: 5m
report:
  url: http://collector-host:7999
```

## CI (GitHub Actions)

The [`Perfscale/github-action`](https://github.com/Perfscale/github-action)
installs perfscale, runs the test, renders the metric table into the job
summary, and writes a machine-readable JSON summary:

```yaml
- uses: Perfscale/github-action@v1
  id: loadtest
  with:
    file: smoke.test.yaml
    config: smoke.config.yaml

- name: Gate on error rate and p95
  run: |
    jq -e '.summary.error_rate < 0.01 and .summary.p95_ms < 500' \
      "${{ steps.loadtest.outputs.summary-json }}"
```

Without the action, the same gate works from any CI via `--summary-export`:

```sh
perfscale run -f smoke.test.yaml -c smoke.config.yaml --summary-export result.json
jq -e '.summary.error_rate < 0.01' result.json
```

The run itself exits `0` even when checks fail (see
[exit code semantics](commands.md#exit-code-semantics)) — gate on the exported
summary, as above, when you want failures to break the build.
