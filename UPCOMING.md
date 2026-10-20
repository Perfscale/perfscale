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

- `std/pubsub@v1`: new `pubsub_publish_ms` histogram (per-message publish
  latency; the `nats` flush folds into the last sample) and `pubsub_errors`
  counter (emitted on every exchange, 0 on success — the `db_errors`
  convention), so publish latency and error rates show up in run summaries
  and `std/thresholds@v1` gates. `${…}` generator tokens now also expand in
  `subject` and `subscribe.until_contains`, sharing one `${seq}` / `${vu}`
  with the payload — per-VU subjects and own-message matching are
  expressible.
- Benchmarks: new `pubsub` suite in `scripts/bench.sh` — `std/pubsub@v1`
  publish / pub-sub roundtrip (e2e latency) against the in-process `memory`
  bus and a NATS broker (`BENCH_NATS_URL`), side by side with k6 rows from a
  custom xk6-nats binary (`K6_NATS_BIN`, built by the bench workflow) in warm
  and cold connection postures.