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

- `std/http@v1` now transparently decodes gzip-compressed responses (`Content-Encoding: gzip`) — send `Accept-Encoding: gzip` via `headers` and the decoded body flows into `check:` and `${{ feed.body.posts }}` selections as plain JSON. The `max_response_body_bytes` cap applies to the decoded payload.
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
