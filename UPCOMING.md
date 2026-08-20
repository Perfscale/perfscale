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

- **JMeter engine** — `perfscale run --jmeter plan.jmx` runs a JMeter test
  plan headless (`jmeter -n -t`), streams its output live, and translates the
  final console `summary =` line into the k6-compatible summary block
  (avg/min/max and error rate — the console summary has no percentiles).
  JMeter's exit code is the CI gate; there is no thresholds integration and
  no perfscale config mapping.
- **WebSocket echo endpoint in `perfscale serve`** — `GET /ws` upgrades and
  echoes every text (and binary) message back, a loopback target for
  WebSocket load tests and the new `ws` benchmark suite (messages/sec and
  message RTT for the native engine and k6).
- **Embedding API for imports** — `import::resolve_value` resolves the
  `import:` chain of an API-submitted document (no file origin), and
  `ImportOptions::remote_guard` lets a server veto every network target in
  the chain (SSRF protection), including HTTP redirect hops.
- **HTTP plumbing is public for downstream actions** — `step::http` now
  exposes `ClientPool`, `client`, `timed_exchange`, `HttpOutcome`,
  `request_line`, `transport_error`, and `error_chain`, plus
  `Context::http_client_shard()`, so proprietary action families (e.g.
  `pro/soap`) pool, time, and report HTTP exchanges identically to
  `std/http@v1`.
- **k6-style load profiles in the native engine** — `stages:` ramps VUs
  linearly between targets (graceful scale-down at step boundaries) and
  `arrival:` holds an iterations/sec rate profile with a lazily growing
  worker pool (`max_vus`, `pre_allocated_vus`). For staged/arrival runs the
  summary's `vus` line reports the observed concurrency
  (`vus....................: <last> min=<min> max=<max>`), the periodic
  `[stats]` line gains a trailing `vus=N` field (**downstream parsers: the
  field appears only on staged/arrival runs; fixed-run lines are
  unchanged**), summary exports report `vus: null`, and arrival runs surface
  a `dropped_iterations` metric for permits the saturated pool couldn't
  serve. Both profiles are native-engine only — `--locust` rejects them with
  a clear error.
