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
- Arrival-rate runs keep their schedule at high rates: the dispatcher emits
  the permits due since its last ~1ms timer wake back-to-back, and a 1-deep
  permit slot read each burst as "every VU busy" — permits were dropped
  while most of the pool sat idle (10k it/s against 256 VUs served ~721/s,
  ~85% in `dropped_iterations`). Drops are now decided by the in-flight
  permit count, so they happen only when `max_vus` workers are genuinely
  busy; the same run now serves ~10k/s with ~0 drops. Under real saturation
  one permit still waits beyond the pool, so
  `engine_arrival_queue_wait_ms` keeps showing it.
- Benchmarks: new `sytester` suite compares the open model against
  [SyTester](https://gitverse.ru/sbertech/sytester) Community Edition — every
  row offers the same fixed arrival rate (`SYT_RATE`, default 10000/s) to
  the same endpoint: SyTester (REST-driven JVM, warm-up before the measured
  run), k6 `constant-arrival-rate`, and the perfscale `arrival:` engine.
  Reports delivered RPS, latency percentiles, dropped iterations, SyTester's
  boot time, CPU per request and peak RSS. Runs in the bench workflow
  (pinned v6.10.0 jar, sha256-checked); see `docs/benchmarks.md`.
- Hot-path allocation and correctness improvements: built-in payload expansion copies literal spans instead of individual bytes, token arguments are borrowed instead of allocated per call, and non-ASCII payloads are preserved correctly. GraphQL schemas are shared through `Arc` instead of deep-cloned per execution, each query is parsed once, and owned JSON variables expand in place. Rust WASM guests advertise `settings_once`, so run settings cross the host/guest boundary once per instance instead of being copied on every call; older components keep the full-settings behavior.
