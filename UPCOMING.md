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

- Benchmarks: SyTester rows now sit in the throughput, saturation, grpc
  and db tables next to k6/locust/JMeter (instead of a separate table),
  offering SyTester's maximum fixed rate (`SYT_RATE`, 10000 TPS) since it
  has no VU model. The grpc row uses its gRPC generator (proto set uploaded
  over the REST API), the db row its PgJDBC transport.