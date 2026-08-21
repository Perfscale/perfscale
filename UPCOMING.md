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

- **`std/pubsub@v1` action** — one-shot pub/sub load step: publish messages
  to a subject and/or wait for `count` matching messages (optional
  `until_contains` matcher, `timeout_ms` bounded), reporting
  `pubsub_msgs_published` / `pubsub_msgs_received` counters and a
  `pubsub_e2e_ms` end-to-end latency histogram. Ships with two drivers:
  `memory` (process-global in-process bus shared by all VUs, no broker
  needed) and `nats` (core NATS via `async-nats`). The driver seam is public
  (`register_pubsub_driver`), and a new `options` object passes
  driver-specific tuning (QoS, consumer groups, auth, …) through verbatim —
  that is how the pro build's Kafka/Redis/MQTT drivers take their settings;
  an unknown `driver:` value fails with the list of registered ones.
- **`boundary` benchmark suite** — ramps 0→`BOUNDARY_MAX_VUS` (default 2000)
  VUs over `BOUNDARY_DURATION` (default 30s) and reports the VU level, time,
  and RPS where the cumulative error rate first reaches `BOUNDARY_ERR_PCT`
  (default 1%), side by side for k6, locust, JMeter, and the native engine.
