# Metrics

The native engine records every request-like step into a fixed-size HDR
histogram and folds custom counters/histograms from action outputs into the
same run aggregates. Two outputs: a machine-readable `[stats]` line every 5
seconds during the run, and a k6-compatible summary at the end.

## Request metrics (`http_req_*`)

Actions that perform a timed request return an `HttpSample`
(`duration_ms`, `status`, `failed`). Durations are recorded in
**microseconds** — sub-millisecond loopback calls stay distinguishable.

| Action | What feeds `http_req_duration` | What counts as failed |
|---|---|---|
| `std/http@v1` | Every request | Status ≥ 400, transport error, timeout (logged as `→ TIMEOUT after …ms`; `status` reported as 0) |
| `std/tcp@v1` | Connect + send/read exchange | Connect failure, timeout, `expect` mismatch |
| `std/udp@v1` | Send (+ optional reply wait) | Timeout, `expect` mismatch |
| `std/ws@v1` | The whole one-shot session (one sample) | Handshake/transport error, an `until_*` rule not met in time |
| `std/ws-connect@v1` | The handshake only | Handshake failure (`connected: false`) |

Deliberately **not** feeding `http_req_duration` — a step whose duration
says nothing about target latency would poison the shared percentiles:

| Action | Why |
|---|---|
| `std/ws-recv@v1` | How long a server waits before pushing is not target latency |
| `std/ws-ping@v1` | Transport RTT; bound it with `check: { duration_ms_lt: … }` instead |
| `std/ws-send@v1`, `std/ws-close@v1` | No meaningful request latency |
| `std/grpc*@v1` (whole family) | gRPC has its own histograms (`grpc_req_duration`, `grpc_msg_rtt`); stream lifetimes span user steps, so streams don't feed even those |
| `std/db-*@v1` (whole family) | DB steps have their own histograms (`db_connect_duration`, `db_query_duration`) and counters (`db_rows`, `db_errors`) |
| `std/child_process@v1`, `std/kill_process@v1` | Process lifecycle, not requests |
| `std/check@v1`, `std/sleep@v1`, `std/log@v1`, `std/file-*@v1` | No network I/O |

Note the asymmetry: gRPC assertion failures go to `grpc_req_failed` (a custom
counter, driven by `expect_status`), **not** to `http_req_failed`.

## Final summary

Printed at the end of every run, k6-compatible so downstream parsers
(dashboards, `perfscale serve`) treat all engines uniformly:

```text
vus....................: 10 min=1 max=10
iterations..............: 4521 150.23/s
http_req_duration......: avg=0.42ms p(50)=0.31ms p(90)=0.88ms p(95)=1.02ms p(99)=1.90ms min=0.09ms max=3.10ms
http_req_failed........: 0.00%
http_reqs..............: 4521 150.23/s
```

- `vus` / `iterations` (+ per-second rate) are always emitted, even for
  sleep-only runs.
- `dropped_iterations` is always emitted for arrival-rate runs — 0 when the
  pool kept up — so threshold gates (`dropped_iterations: ["count==0"]`)
  resolve instead of erroring on an unknown metric.
- The `http_req_*` block appears only when at least one sample was recorded.
- Percentiles come from a fixed-size HDR histogram (1 µs – 1 h range, two
  significant figures → ≤1% quantile error) — memory stays flat no matter how
  long the soak, at the cost of an error invisible at the printed precision.

## Live `[stats]` lines

Every 5 seconds while VUs run, one machine-readable line:

```text
[stats] ts=1720000000000 rps=246.80 err_pct=0.00 p50=1.20 p90=3.40 p95=4.10 p99=8.20 reqs=1234 iters=456
```

- `ts` — unix epoch milliseconds; `rps` — requests in the just-finished 5 s
  window; `err_pct` — cumulative failure percentage; `p50`…`p99` — cumulative
  percentiles in ms (the histogram is never reset, so they converge instead
  of jittering); `reqs` — cumulative requests; `iters` — cumulative
  iterations.
- With no requests yet the percentiles are omitted:
  `[stats] ts=… rps=0.00 reqs=0 iters=3`.
- These lines exist for streaming consumers (the controlplane parses them
  out of the log stream); see [`--quiet`](#quiet) for console behavior.

## Custom metrics (`value.metrics`)

Any action can attach a reserved `metrics` object to its step output; the
runner folds it into the run aggregates:

- A **number** becomes a counter, summed across VUs and iterations, reported
  as `<name>: <total> <rate>/s`.
- An **array of numbers** becomes HDR histogram samples (milliseconds),
  reported as
  `<name>: avg=…ms p(50)=… p(90)=… p(95)=… p(99)=… min=… max=… count=N`.

Built-in emitters, per metric (details on each family's page):

WebSocket — [websocket.md](websocket.md):

- `ws_msgs_sent` — counter; WS messages sent. Emitted by `std/ws@v1`
  (whole-session total) and `std/ws-send@v1` (per step), on success and on
  failure.
- `ws_msgs_received` — counter; WS messages read. Emitted by `std/ws@v1`
  and `std/ws-recv@v1`.
- `ws_msg_rtt` — HDR histogram (ms); send → first reply matching your
  `until_*` rule (application-level RTT). One sample per matched reply;
  emitted only when a rule matched after a send on the same connection —
  pure push waits record nothing (deliberately, see
  [websocket.md](websocket.md#metrics)).

Pub/Sub — [pubsub.md](pubsub.md):

- `pubsub_msgs_published` — counter; messages accepted by the transport.
  Emitted by every completed `std/pubsub@v1` exchange (also 0 for
  subscribe-only steps).
- `pubsub_msgs_received` — counter; messages counted toward
  `subscribe.count`. Only when the step has a `subscribe` block.
- `pubsub_e2e_ms` — HDR histogram (ms); publish-phase start → message
  consumed, one sample per matched message. Only with `subscribe`, and only
  when at least one message matched (a subscribe wait that matched nothing
  emits no samples). For subscribe-only steps this is the wait time.

Shared variables:

- `shared_variable_wait_ms` — HDR histogram (ms); time blocked in
  `wait_for` before the condition held, one sample per waiting read.
  Emitted by `std/get_shared_variable@v1` only with `wait_for`.

LLM — [llm.md](llm.md):

- `llm_ttft_ms` — HDR histogram (ms); request start → first content chunk
  (time to first token). Streamed requests only.
- `llm_tokens_per_sec` — HDR histogram; completion tokens / generation time
  (after the first token when streamed). Only when the server reports
  completion tokens.
- `llm_prompt_tokens` / `llm_completion_tokens` — counters, as reported by
  the server's `usage`; absent when the server does not report usage.
- `llm_chunks` — counter; SSE chunks received (0 for non-streamed
  requests).

gRPC — [grpc.md](grpc.md):

- `grpc_req_duration` — HDR histogram (ms); unary call latency, one sample
  per completed call. `std/grpc@v1` and `std/grpc-call@v1` only — stream
  lifetimes span user steps and deliberately feed nothing; a failed connect
  emits no metrics at all (no RPC was made).
- `grpc_msg_rtt` — HDR histogram (ms); send → matching reply RTT. On a
  unary call emitted only when the status is OK (then it equals
  `grpc_req_duration`); on `std/grpc-stream-recv@v1` only when an `until_*`
  rule matched and a `grpc-stream-send` preceded it on the same stream.
- `grpc_msgs_sent` — counter; messages sent, per `std/grpc-call@v1` call
  (1) and per `std/grpc-stream-send@v1` step.
- `grpc_msgs_received` — counter; messages read, per `std/grpc-call@v1`
  call, per `std/grpc-stream-recv@v1` and per `std/grpc-stream-close@v1`
  (drained at close).
- `grpc_req_failed` — counter, 0/1 per call; calls that missed
  `expect_status`. Emitted by `std/grpc-call@v1` and
  `std/grpc-stream-close@v1` (close turns the stream's final status into
  this counter). Note the derived same-named failure rate shadows this
  counter — see [below](#failure-rate-metrics-family_failed).

GraphQL — [graphql.md](graphql.md):

- `graphql_req_duration` — HDR histogram (ms); the operation's round trip,
  one sample per request that was actually sent (success or failure).
- `graphql_errors` — counter; GraphQL-level errors, including partial-data
  responses that pass the step. Always emitted (0 on clean responses), so
  `graphql_errors: ["count==0"]` gates resolve on healthy runs too.
- `graphql_op_<operationName>_duration` — HDR histogram (ms); per-operation
  latency, emitted only for named operations (explicit `operation` or a
  single named operation in the document), so cardinality stays bounded by
  the test definition.

Databases:

- `db_connect_duration` — HDR histogram (ms); connect + pool setup latency,
  `std/db-connect@v1` on success.
- `db_query_duration` — HDR histogram (ms); query latency for
  `std/db-query@v1` and `std/db-tx-*@v1`; includes the fresh connect in
  per-query mode.
- `db_rows` — counter; rows returned by `std/db-query@v1`, or rows affected
  when the statement returned none.
- `db_errors` — counter; failed DB steps, total, emitted by every
  `std/db-*@v1` step. Successful steps emit `db_errors: 0`, so the counter
  exists (at 0) on fully healthy runs — gates like
  `db_errors: ["count==0"]` work either way.
- `db_errors_connection` / `_constraint` / `_deadlock` / `_timeout` /
  `_other` — counters; same, split by class (SQLSTATE / errno / SQLite
  result code), emitted on failure.

Downstream actions use the same channel — e.g. the WebRTC plugin emits
`webrtc_*` series ([WebRTC (pro)](/docs/pro-features/webrtc#metrics)) and the proprietary FIX
action emits `fix_messages_sent`.

## Failure-rate metrics (`<family>_failed`)

**Why they exist.** `std/thresholds@v1` `rate` expressions are the natural
shape for an SLO gate — "fewer than 1% of calls may fail" — but a duration
histogram alone cannot answer it: it holds latencies, and nothing in a
latency sample says whether the invocation that produced it succeeded. So
alongside the `metrics` payload, the runner derives a per-invocation
failure signal generically.

**How they are derived.** For every array-valued (histogram) metric a step
invocation emits, the runner records one 0/1 sample — 1 when the step
invocation failed, 0 when it succeeded — under the metric's family name: a
trailing `_duration`/`_rtt` is replaced by `_failed`, and when there is no
such suffix the full name gets `_failed` appended:

| Duration metric | Derived failure metric |
|---|---|
| `http_req_duration` | `http_req_failed` (native to the HTTP path — see below) |
| `db_query_duration` | `db_query_failed` |
| `db_connect_duration` | `db_connect_failed` |
| `grpc_req_duration` | `grpc_req_failed` |
| `graphql_req_duration` | `graphql_req_failed` |
| `graphql_op_<name>_duration` | `graphql_op_<name>_failed` (per named operation) |
| `ws_msg_rtt` | `ws_msg_failed` |
| `pubsub_e2e_ms` | `pubsub_e2e_ms_failed` |
| `shared_variable_wait_ms` | `shared_variable_wait_ms_failed` |
| `llm_ttft_ms` | `llm_ttft_ms_failed` |
| `llm_tokens_per_sec` | `llm_tokens_per_sec_failed` |
| `webrtc_setup_ms`, `webrtc_ttff_ms`, `webrtc_call_duration_ms`, … | `<same name>_failed` — see [WebRTC (pro)](/docs/pro-features/webrtc#metrics) |

These print as `<name>: <pct>%` (k6's `http_req_failed` shape). Because one
sample is recorded **per invocation** (not per duration sample — an
invocation that matched three messages still records one failure sample),
`failed/total` over them is exactly the step family's invocation failure
rate — that is what `std/thresholds@v1` evaluates with `rate`:

```yaml
  - use: std/thresholds@v1
    with:
      graphql_req_failed:
        - "rate<0.01"             # fewer than 1% failed operations
      pubsub_e2e_ms_failed:
        - "rate<0.05"             # subscribe waits mostly kept up
```

Note a failed step that emits no duration sample (e.g. `db-connect` that
never connected, a subscribe wait that matched zero messages) records no
failure sample either, so its family rate covers invocations that got far
enough to produce a measurement.

**Native vs derived.** `http_req_failed` is native to the HTTP path: every
action returning an `HttpSample` marks it failed/succeeded itself (status ≥
400, transport error, timeout), and no derivation is involved. The gRPC
family is the asymmetric case: `std/grpc-call@v1` and
`std/grpc-stream-close@v1` emit a `grpc_req_failed` **counter** (0/1 per
call, driven by `expect_status`), and the runner *also* derives a
`grpc_req_failed` rate from `grpc_req_duration`. When both exist, the rate
metric shadows the counter in the summary and in threshold evaluation — so
`grpc_req_failed: ["rate<0.05"]` works, while `count` expressions against
that name do not see the counter. The WebRTC family has its own failure
counters (`webrtc_connect_errors`, `webrtc_call_errors_<stage>`) that
coexist with derived rates — see [WebRTC (pro)](/docs/pro-features/webrtc#metrics).

## Run-level gates (`std/thresholds@v1`)

A `std/thresholds@v1` step (typically in `after:`) evaluates k6-style
expressions against the run aggregates and prints one machine-readable line
after the metric summary:

```text
thresholds: {"status":"fail","message":"db_query_failed rate=1 ≥ 0.05; checkout SLO","violations":[{"metric":"db_query_failed","expr":"rate<0.05","actual":1.0}]}
```

Aggregates come from the same HDR histograms/counters as the text summary,
so gate numbers match what the summary prints. The line is collected into
`perfscale run --summary-export` output under `thresholds`
(`{status, message, violations}`), and a `fail` status makes the CLI exit
non-zero. See [actions.md](actions.md#stdthresholdsv1).

## GPU metrics (`gpu:`)

With `gpu.enabled: true` in the [run config](../yaml-reference.md#config--c-configyaml),
the native engine samples every GPU on the host — utilization %, VRAM
used/total, temperature, power draw — once per `interval_ms` for the whole
VU phase (via `nvidia-smi` or a dcgm-exporter endpoint). After the metric
summary the run prints a compact per-device block plus one machine-readable
`gpu: {...}` line with the full timeseries, which `--summary-export` embeds
under `gpu`:

```text
gpu: 1 device, 300 samples every 1000ms (nvidia-smi)
gpu0: util avg=64.3% max=100.0% vram max=41088/81559MiB temp max=71.0C power max=512.3W
gpu: {"source":"nvidia-smi","interval_ms":1000,"devices":[{"index":0,"samples":[…],…}]}
```

Collection is best-effort: no GPU / missing tooling logs one warning and the
run continues without GPU metrics. Sample timestamps share the epoch-ms
timeline with the `[stats]` lines, so load and GPU state correlate directly —
the primary use case is [`std/llm@v1`](llm.md) runs against local model
servers. Full guide: [gpu.md](gpu.md).

## `--quiet`

Two independent layers:

- **At the source** (native engine): per-iteration success output — request
  lines, sleep markers, passing checks — is not even formatted or sent.
  Errors, failing checks, `[stats]` lines, and the final summary are always
  emitted into the stream.
- **At the CLI printer**: under `--quiet`, stdout lines print only if they
  are k6-shaped summary lines (`vus`, `iterations`, `http_req_*`); stderr and
  system lines always print. Custom metric lines and `[stats]` stay in the
  stream for log consumers but are hidden from the console.

## Forwarding the summary (`report`)

Point the run at a `perfscale serve` instance and the summary lines are
forwarded when the run finishes:

```yaml
# config.yaml
report:
  url: http://localhost:7999
```

```sh
perfscale run -f test.yaml -c config.yaml --report http://localhost:7999
```

The CLI flag wins over the config block. After the run, the CLI POSTs the
collected summary lines (only the k6-shaped ones) as
`{"lines": […]}` to `<url>/api/v1/metrics` with a 5 s timeout; delivery
problems are logged as `[report] …` on stderr and never fail the run itself.

## During-run metrics (`report.during_run`)

The same `report:` block can also stream metrics **while the run is in
progress** — cumulative snapshots of every metric family, batched and POSTed
to the same `<url>/api/v1/metrics` endpoint:

```yaml
report:
  url: http://localhost:7999
  during_run: true        # default false — only the end-of-run summary
  interval_ms: 5000       # snapshot/flush interval, min 100
  batch_size: 500         # flush a batch once it holds this many samples
  max_cpu_percent: 90     # CPU gate; 0 disables it
  max_pending: 24         # soft warn cap — batches are never dropped mid-run
```

The engine emits one snapshot per `interval_ms` (independent of the 5 s
`[stats]` reporter) and a shipper batches them: a batch is POSTed once it
reaches `batch_size` samples, and whatever is open flushes on every interval
tick. Each POST body is `{samples, seq}` (`taskId` too when the caller sets
one), where `seq` is a monotonically increasing batch number reused across
retries — receivers can deduplicate redeliveries.

**Sample naming** follows the Prometheus summary/counter conventions, with
durations converted from the engine's milliseconds to **seconds**:

| Engine metric | Shipped samples |
|---|---|
| histogram (`http_req_duration`, `db_query_duration`, …) | `<name>{quantile="0.5"/"0.9"/"0.95"/"0.99"}` (seconds), `<name>_count`, `<name>_sum` (seconds) |
| counter (`db_rows`, …) | `<name>_total` |
| failure rate (`http_req_failed`, …) | `<name>_total` (invocations), `<name>_failed_total` (failures) |

All values are **cumulative since run start** — the HDR histograms and
counters never reset during a run. To derive per-window numbers consumer-side,
diff successive snapshots: `rate(x_count[window])`-style for throughput,
`Δ_sum / Δ_count` for windowed average latency.

**GPU gauges stream too** when the run also has `gpu.enabled: true` (see
[GPU metrics](gpu.md)): each snapshot carries the GPU samples taken since the
previous one, as point-in-time gauges with the collector's own timestamps —
chart them as-is, no diffing:

| Shipped sample | Labels | Unit |
|---|---|---|
| `gpu_utilization_pct` | `gpu="<index>"` | percent, 0–100 |
| `gpu_memory_used_mib` | `gpu="<index>"` | MiB |
| `gpu_memory_total_mib` | `gpu="<index>"` | MiB |
| `gpu_temperature_c` | `gpu="<index>"` | °C |
| `gpu_power_w` | `gpu="<index>"` | watts |

Fields the source reported as `N/A` are skipped (never shipped as zeros), and
pro-collector extras ride under their own names. Without `gpu.enabled` the
snapshots carry no GPU data at all.

**Protective strategies** (the VU loop always wins; streaming yields first):

- *CPU gate*: while the host's busy CPU% (from `/proc/stat`) is at or above
  `max_cpu_percent`, pending batches are held — no POSTs — and incoming
  snapshots are deferred (queued, every 10th logs a warning), then flushed
  in arrival order once the gate opens. Off-Linux there is no CPU reading,
  so the gate is inert. `max_cpu_percent: 0` disables it.
- *Unbounded backlog, soft cap*: sealed-but-undelivered batches are never
  dropped while the run is alive; growing past `max_pending` only logs a
  rate-limited warning. Delivery resumes in order when the target responds.
- *Retries*: network errors, 5xx and 429 retry the same batch with
  exponential backoff ×2 from 1 s, capped at 60 s. Any other 4xx is treated
  as poison — the batch is dropped, never retried.
- *Shutdown*: when the run ends, everything still pending — including
  snapshots deferred by the CPU gate — is flushed with short bounded
  retries (1/2/5/10 s), then the shipper exits. The CLI waits for this
  final drain (bounded) so the last batch lands before the process exits.

The engine never blocks on streaming: snapshots go over a bounded channel
with `try_send`, and a full channel drops the snapshot rather than
backpressuring VUs — the only place a point can be shed mid-run. With
`during_run` absent or `false` there is no extra task and no channel
traffic at all.

Auth note: the CLI ships unauthenticated (same as the end-of-run report).
The authenticated path is the agent's — it runs the same shipper with a
Keycloak token provider and a `taskId` (also stamped as a `task_id` label on
every sample).

**On the platform** (perfscale.su or a self-hosted controlplane) the stream
lights the run page up live — latency quantiles, throughput, failure rate
and GPU curves drawn as the samples arrive. The agent wires everything
itself: `report.during_run: true` in the run's configuration is the only
user action — it authenticates the stream with its machine token and stamps
every sample with `task_id` and `machine_id`. The same samples feed the
Metrics dashboard, the Prometheus-compatible query API (point Grafana at
`/api/v1/integrations/prometheus`) and Remote Write into your own
Prometheus/Mimir.
