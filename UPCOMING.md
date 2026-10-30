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

- Benchmarks: new `sourceips` suite measures `std/http@v1` `source_ips`
  against the per-endpoint connection wall — standing keep-alive
  connections (`pool: dedicated`) under a narrowed ephemeral port range,
  from one source IP vs several loopback addresses (peak established
  connections, connect errors). Linux CI only (it narrows the port range
  via sysctl for the suite and restores it).
- Docker: every image flavor is now its own GHCR package —
  `ghcr.io/perfscale/perfscale` (perfscale only), `perfscale-k6`,
  `perfscale-jmeter`, `perfscale-locust`, and `perfscale-full` (all three
  runners), each tagged `X.Y.Z` / `X.Y` / `latest`. Pick exactly the engine
  you need (`docker pull ghcr.io/perfscale/perfscale-k6:latest`) instead of
  finding the right suffix tag in one package. The old suffix tags
  (`perfscale:latest-k6`, `perfscale:X.Y.Z-full`, …) keep being published
  for the same images, so existing pulls don't break.
- `std/http@v1` now transparently decodes gzip-compressed responses (`Content-Encoding: gzip`) — send `Accept-Encoding: gzip` via `headers` and the decoded body flows into `check:` and `${{ feed.body.posts }}` selections as plain JSON. The `max_response_body_bytes` cap applies to the decoded payload.
- Custom WASM steps (RFC 009): a WASM library component can now export steps dispatchable in `use:` as `lib/<alias>/<step>` — the component receives the interpolated `with:` params plus the step context (VU id, iteration, run settings) and returns the standard action result (success / output / tagged logs / metrics / optional `http_sample` and `secret_output_paths`), so it folds into `check:`, thresholds, and the run summary like any built-in. `lib/` is a reserved namespace (never shadows `std/*`/`pro/*`), `perfscale lint` validates `lib/` references against each component's probed manifest, and the run summary's `libraries:` line gains `step_calls` / `step_errors` / `step_p95_ms` next to the token-call counters. A new `process` capability (triple-gated: the YAML grant, `allow_library_capabilities: true`, and `allow_process_actions: true`) gives step components host-mediated one-shot process execution with bounded output, timeout, and process-tree cleanup — never raw WASI process imports. New ABI: `perfscale:step@0.1.0` (`wit/step/step.wit`), additive on top of the untouched `perfscale:library@0.3.0`.
- Benchmarks: SyTester is now a default competitor engine — whenever its
  jar and Java 21+ are available, its rows sit in the throughput,
  saturation, grpc and db tables next to k6/locust/JMeter,
  offering SyTester's maximum fixed rate (`SYT_RATE`, 10000 TPS) since it
  has no VU model. The grpc row uses its gRPC generator (proto set uploaded
  over the REST API), the db row its PgJDBC transport. A new `APPS` knob
  (bench workflow input `apps`, default `k6 jmeter locust sytester`) picks
  the competitor engines; perfscale's rows always run. Unknown names in
  `SUITES` or `APPS` now fail the run (exit 2) instead of being silently
  skipped.
