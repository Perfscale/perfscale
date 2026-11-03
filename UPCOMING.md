# Upcoming release

<!--
Release notes for the next release, written as features land.

- Append short, user-facing entries below this comment as you merge changes
  (what changed and why a user cares — not commit messages).
- On a `v*` tag, the release workflow publishes everything below the comment
  as the release body (with the auto-generated changelog appended), then
  resets this file back to the template.
- If this file has no entries at tag time, the release falls back to
  auto-generated notes and the workflow prints a warning.
-->

- Streaming reporters (issue #12): `report.reporters` can now stream live,
  cumulative metrics during the run to pluggable sinks — `std/report/serve@v1`
  (the historical `report.url` path as a reporter), `std/report/prometheus-rw@v1`
  (Snappy + protobuf remote write; classic `_bucket{le}` histograms by default,
  native histograms opt-in via `with.histogram: native`) and
  `std/report/otel@v1` (OTLP/gRPC with cumulative exponential histograms).
  Engine-owned keys sit next to `use:`: `during_run`, `interval_ms`, `labels`
  and `max_series` (default 10000, deterministic drop counted in
  `perfscale_report_dropped_series_total{reporter}`); `max_samples` moved out
  of `with:` (the legacy key still reads). Labels merge step > reporter > run,
  interpolate `${{ inputs.* }}` / `${{ env.* }}` once before the run, and steps
  accept their own `labels:`; `run_id`/`worker` are engine-set and rejected in
  user config. No client-side quantile series in the new exporters — backends
  compute correct cross-worker quantiles from the buckets
  (`histogram_quantile(0.95, sum by (le) (rate(http_req_duration_bucket[1m])))`).
  Each reporter gets an independent bounded queue (`with.max_pending`), batched
  delivery (`with.batch_size`, never exceeded — oversized snapshots split in
  order), retry with exponential backoff (`with.retry`, `Retry-After`
  honored, other 4xx poisoned) and a synchronous final flush bounded by
  `with.final_timeout` (default 30s) that coalesces the backlog to the
  terminal cumulative snapshot; overflow drops the OLDEST snapshot (counted in
  `perfscale_report_dropped_snapshots_total{reporter}`), and a slow or dead
  backend can never block the VUs, another reporter, or fail the run. The
  delivery counters are per-run, registered as the `report` metric family and
  exported to each backend as `perfscale_report_*_total{reporter="…"}`. A `during_run: true` on a final-only reporter (json/junit/html/wasm) is
  now a lint/run error.

- Benchmarks: a `renaissance` suite (in the default suites and the daily CI
  run) executes the official Renaissance MIT v0.16.2 jar
  (`renaissance-mit-0.16.2.jar`, sha256-pinned and cached by CI under
  `target/renaissance/`) via `scripts/bench_renaissance.sh` — default
  workload `finagle-http` (`RENAISSANCE_BENCHMARKS`), bounded to
  `RENAISSANCE_RUN_SECONDS` (default 60) measured seconds per benchmark
  through the harness's own `--run-seconds`. The report and results JSON
  record benchmark operation throughput (ops/s, never HTTP RPS) and
  per-operation time plus CPU/RSS/duration; a missing/invalid jar, a Java
  failure or empty results fail the run rather than reporting fake numbers.
  Console log and native JSON are preserved under `renaissance-artifacts/`
  and uploaded with the bench artifact; `bench_compare.py` compares only
  same-benchmark rows.

- Benchmarks: new opt-in `specjbb` suite (`SUITES=specjbb ./scripts/bench.sh`)
  runs a caller-supplied commercial SPECjbb2015 kit (`SPECJBB_HOME` /
  `SPECJBB_JAR`, plus `SPECJBB_JAVA` / `SPECJBB_MODE`, default `COMPOSITE`)
  via the official `java -jar specjbb2015.jar -m <MODE>` — never bundled,
  never downloaded, never shortened — and reports native max-jOPS /
  critical-jOPS (Java operations/s, not HTTP RPS) with mode, duration, CPU
  and peak RSS in the report and results JSON. It is excluded from the
  default suite list and CI; with an invalid kit path the bench fails loudly
  instead of reporting a fake success. Console log and the SPEC report tree
  are preserved as artifacts.

- Pub/Sub drivers can report asynchronous delivery failures through
  `PubSubOutcome.errors`. The engine adds them to the synchronous failure flag
  in `pubsub_errors`, so Kafka delivery errors, MQTT ack timeouts and Redis
  pipeline error replies remain visible with `flush: none`; the counter can
  exceed 1 per exchange without failing the step.
