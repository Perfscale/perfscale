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

Built-in emitters:

| Name | Type | Emitted by | Meaning |
|---|---|---|---|
| `ws_msgs_sent` | counter | `std/ws@v1`, `std/ws-send@v1` | WS messages sent |
| `ws_msgs_received` | counter | `std/ws@v1`, `std/ws-recv@v1` | WS messages read |
| `ws_msg_rtt` | histogram | `std/ws@v1`, `std/ws-recv@v1` | Send → first matching reply (application-level RTT) |
| `pubsub_msgs_published` | counter | `std/pubsub@v1` | Messages accepted by the transport |
| `pubsub_msgs_received` | counter | `std/pubsub@v1` (with `subscribe`) | Messages counted toward `subscribe.count` |
| `pubsub_e2e_ms` | histogram | `std/pubsub@v1` (with `subscribe`) | Publish-phase start → message consumed, one sample per matched message |
| `grpc_req_duration` | histogram | `std/grpc@v1`, `std/grpc-call@v1` | Unary call latency |
| `graphql_req_duration` | histogram | `std/graphql@v1` | GraphQL operation round trip |
| `graphql_errors` | counter | `std/graphql@v1` | GraphQL-level errors, including partial-data responses that pass the step |
| `graphql_op_<operationName>_duration` | histogram | `std/graphql@v1` | Per-operation latency, only for named operations (bounded cardinality) |
| `grpc_msg_rtt` | histogram | `std/grpc-call@v1`, `std/grpc-stream-recv@v1` | Send → matching reply RTT |
| `grpc_msgs_sent` | counter | `std/grpc-call@v1`, `std/grpc-stream-send@v1` | gRPC messages sent |
| `grpc_msgs_received` | counter | `std/grpc-call@v1`, `std/grpc-stream-recv@v1`, `std/grpc-stream-close@v1` | gRPC messages read |
| `grpc_req_failed` | counter | `std/grpc-call@v1`, `std/grpc-stream-close@v1` | Calls that missed `expect_status` |
| `db_connect_duration` | histogram | `std/db-connect@v1` (success) | Connect + pool setup latency |
| `db_query_duration` | histogram | `std/db-query@v1`, `std/db-tx-*@v1` | Query latency; includes the fresh connect in per-query mode |
| `db_rows` | counter | `std/db-query@v1` | Rows returned, or rows affected when the statement returned none |
| `db_errors` | counter | `std/db-*@v1` (failure) | Failed DB steps, total. Successful DB steps emit `db_errors: 0`, so the counter exists (at 0) on fully healthy runs — gates like `db_errors: ["count==0"]` work either way |
| `db_errors_connection` / `_constraint` / `_deadlock` / `_timeout` / `_other` | counter | `std/db-*@v1` (failure) | Same, split by class (SQLSTATE / errno / SQLite result code) |

Downstream actions use the same channel — e.g. the proprietary FIX action
emits `fix_messages_sent`.

## Failure-rate metrics (`<family>_failed`)

Alongside the `metrics` payload, the runner derives per-invocation failure
samples generically: for every histogram (array-valued) metric an invocation
emits, it records one 0/1 sample — 1 when the step invocation failed, 0 when
it succeeded — under the metric's family name with a trailing
`_duration`/`_rtt` replaced by `_failed`:

| Duration metric | Derived failure metric |
|---|---|
| `http_req_duration` | `http_req_failed` (native to the HTTP path) |
| `db_query_duration` | `db_query_failed` |
| `db_connect_duration` | `db_connect_failed` |
| `grpc_req_duration` | `grpc_req_failed` |
| `graphql_req_duration` | `graphql_req_failed` |
| `ws_msg_rtt` | `ws_msg_failed` |
| `pubsub_e2e_ms` | `pubsub_e2e_ms_failed` |

These print as `<name>: <pct>%` (k6's `http_req_failed` shape). Because one
sample is recorded **per invocation** (not per duration sample),
`failed/total` over them is exactly the step family's failure rate — that is
what `std/thresholds@v1` evaluates with `rate`, e.g.
`db_query_failed: ["rate<0.05"]`. Note a failed step that emits no duration
sample (e.g. `db-connect` that never connected) records no sample either, so
its family rate covers completed invocations.

When a family already has a same-named counter (the gRPC actions emit a
`grpc_req_failed` counter for `expect_status` misses), the rate metric
shadows it in the summary and in threshold evaluation.

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
