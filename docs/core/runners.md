# Runners

Three engines, one interface: every runner produces an
`mpsc::Receiver<LogLine>` that streams output live and closes when the run
ends. Pick one via `ExecutionPlan`.

## Native step engine (`step::runner`)

Pure Rust, no external binary. `run_steps(steps, config, tx)` runs the step
list under one of three load profiles, resolved from the config by
`step::schedule::Schedule`:

- **Fixed** — `vus` + `duration`: `config.vus` tokio tasks each loop over the
  step list until the duration expires.
- **Ramping VUs** — `stages:` (k6-style): a supervisor task recomputes the
  target VU count every ~100ms by linear interpolation between stage targets
  (the first stage ramps from 0, each next one from the previous target).
  Scaling up spawns fresh VU tasks; scaling down flags the newest VUs, which
  finish their in-flight step and exit at the next step boundary (graceful).
  The run length is the sum of the stage durations.
- **Arrival-rate** — `arrival:` (open model): a dispatcher computes iteration
  start instants by inverting the piecewise-linear rate integral and hands
  permits to a worker pool that starts at `pre_allocated_vus` (default 1) and
  grows lazily to `max_vus`. Unlike the closed VU-loop models, new iterations
  start on schedule even when the system under test slows down — a permit
  nobody can serve (pool saturated at `max_vus`) is dropped, counted in the
  `dropped_iterations` summary metric, and logged at most once per 5s.

For staged/arrival runs the summary's `vus` line reports the *observed*
concurrency — `vus....................: <last> min=<min> max=<max>` — and the
periodic `[stats]` line gains a trailing `vus=N` with the live count. Fixed
runs keep the historical formats unchanged.

- Per-VU `Context` — step outputs and `${{ }}` interpolation are isolated
  between VUs, persistent across iterations of the same VU
- HTTP timings from `std/http@v1` feed the shared metrics; other actions
  (WebSocket, gRPC, TCP/UDP) contribute counters and latency histograms
  (e.g. `ws_msg_rtt`, `grpc_req_duration`) through the same collector
- Ends with the k6-compatible summary block + `Done — Xs wall clock`
- `vus: 0` is clamped to 1; duration strings parse via `parse_duration_secs`
  (`"90"`, `"1m30s"`, `"1h"` — minimum 1s). Stage durations are validated
  strictly: unparseable or zero lengths fail the run (and `perfscale lint`)
  with a clear error, as do `stages` combined with `arrival`, and `arrival`
  without `max_vus >= 1`

## k6 (`runner::k6`)

Wraps an existing `k6` install:

1. the script is written to `$TMPDIR/perfscale-<uuid>.js`
2. `k6 run --no-color <script>` is spawned with piped stdio
3. stdout/stderr stream as `LogLine`s; the temp file is removed on exit

Two modes:

| Function | Returns | Use |
|---|---|---|
| `run_streaming(script)` | `Receiver<LogLine>` | live output |
| `run_oneshot(script)` | `RunResult { exit_code, success, stdout, stderr, script }` | collect-then-inspect |

Load configuration (VUs, stages, thresholds) belongs in the script's own
`options` block — perfscale does not inject k6 flags.

Missing binary → `k6 not found in PATH — install from https://k6.io/...`.

## locust (`runner::locust`)

Wraps an existing `locust` install in headless mode:

```text
locust -f <script> --headless -u <users> -r <spawn_rate> -t <duration> --csv <tmp-prefix> [--host <host>]
```

`LocustOpts { users, spawn_rate, duration, host }` maps from a generic
`RunConfig` via `LocustOpts::from_run_config` (vus → users and spawn_rate).

While running, locust's own stdout/stderr stream through. After exit, the
runner parses the `Aggregated` row of `<prefix>_stats.csv` and emits the same
summary block the other engines produce:

```text
http_req_duration......: avg=42.50ms p(50)=40ms p(90)=60ms p(95)=68ms p(99)=85ms min=10ms max=120ms
http_req_failed........: 2.00%
http_reqs..............: 100 10.50/s
```

Temp CSV files (`_stats`, `_stats_history`, `_failures`, `_exceptions`) are
cleaned up afterwards. A missing/short CSV yields a `system` line
(`failed to read locust stats: ...`) rather than an error — the process
output has already been streamed.

Missing binary → `locust not found in PATH — install with pip install locust`.

## Choosing an engine

| | native | k6 | locust |
|---|---|---|---|
| Install needed | none | k6 binary | python + locust |
| Scenario language | YAML steps | JavaScript | Python |
| Scripting power | low (4 actions) | high | high |
| Load model | fixed VUs, ramping `stages:`, or arrival-rate | stages/thresholds/scenarios | users/spawn-rate |
| Best for | smoke tests, CI gates, simple API flows | complex k6 suites you already have | python-centric teams |
