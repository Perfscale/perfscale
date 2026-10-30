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

- Docker: every image flavor is now its own GHCR package —
  `ghcr.io/perfscale/perfscale` (perfscale only), `perfscale-k6`,
  `perfscale-jmeter`, `perfscale-locust`, and `perfscale-full` (all three
  runners), each tagged `X.Y.Z` / `X.Y` / `latest`. Pick exactly the engine
  you need (`docker pull ghcr.io/perfscale/perfscale-k6:latest`) instead of
  finding the right suffix tag in one package. The old suffix tags
  (`perfscale:latest-k6`, `perfscale:X.Y.Z-full`, …) keep being published
  for the same images, so existing pulls don't break.
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
