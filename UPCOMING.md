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

- `std/pubsub@v1`: new `scope` parameter (`exchange` | `vu`, default
  `exchange` — the historical connect-per-exchange posture, unchanged).
  With `scope: vu` a broker driver keeps one connection per VU and reuses it
  across exchanges (the `nats` driver pools clients per `(VU, url)`), and the
  new `pubsub_connect_ms` histogram carries one sample per exchange that
  actually opened a connection — every exchange under `scope: exchange`,
  only the first of each VU under `scope: vu` — so connect cost stays
  visible instead of hiding inside `pubsub_publish_ms`.
  **Breaking for driver authors:** `PubSubDriver::run` now takes a
  `&PubSubContext` (carries the VU key for `scope: vu` reuse) alongside the
  params, and `PubSubParams`/`PubSubOutcome` gained `scope` / `connect_ms`
  fields — downstream driver crates must update their implementations.
- Benchmarks: the `pubsub` suite gains `scope=vu` rows — `std/pubsub@v1`
  with one NATS connection per VU, the k6 (xk6-nats, warm) posture.
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