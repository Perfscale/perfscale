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

## `perfscale serve`: embedded docs

- The serve dashboard gains a **Docs tab**: the full OSS documentation
  (English and Russian, switchable) ships inside the binary and renders
  offline — no CDN, no build step. The embedded copy is drift-checked in CI.

## Performance: metrics contention, WASM instance reuse, lossy logging

- The run metrics store is now sharded per worker thread instead of one
  global mutex: at 16 writer threads the aggregate record rate is ~8x higher
  than before (~15M → ~120M ops/s in the contended bench). Summary, `[stats]`,
  snapshots, and thresholds read a merged view — output formats are
  unchanged.
- WASM library instances are now created once per VU instead of once per
  step: a step using `${alias.fn(...)}` tokens no longer pays
  store/instantiation/`init()` per message (up to ~20x on token-heavy
  steps). Instance state now lives for the VU's lifetime — see the new
  "Instance lifecycle" section in the libraries guide.
- Per-step log lines no longer stall VUs when the log consumer (a pipe, a
  slow terminal) is slow: they go through a lossy channel, and dropped lines
  are counted in `engine_log_lines_dropped` with a warning at run end.
  Summaries and errors are still delivered reliably.

## Correctness: response body cap, WS buffer cap

- Native HTTP/GraphQL steps now fail with a clear error when a response body
  exceeds `max_response_body_bytes` (default 16 MiB, per config) instead of
  buffering it whole; binary bodies no longer double-allocate. Step outputs
  are shared by reference — no more double deep-clone per step.
- `std/ws-recv@v1` / `std/ws@v1` until-rules cap the buffered messages at
  `with: { max_buffered: N }` (default 10 000) and fail with a diagnostic
  instead of growing memory unbounded against a chatty server.

## Engine self-metrics + arrival-profile honesty

- New optional `engine_metrics:` config block records the engine's own
  overhead: `engine_scheduler_lag_ms` (tokio scheduling delay) and
  `engine_log_channel_depth_max`. Off by default.
- The arrival-rate profile now measures what latency percentiles used to
  hide: `engine_arrival_queue_wait_ms` (scheduled → actual start) and
  `engine_arrival_iteration_ms` (end-to-end iteration from the scheduled
  start), plus the existing `dropped_iterations` counter. `http_req_*` stay
  pure server time. See "Arrival-profile latency and coordinated omission"
  in the metrics guide.

## Metrics registry + `perfscale metrics`

- Steps now declare their metrics in typed **families** (name, kind, unit,
  description) registered with the engine; core ships the built-in `http` and
  `llm` families, and pro modules register theirs the same way. New
  `perfscale metrics` command prints the full catalog.
- Emitting a registered metric with the wrong JSON shape (a scalar where a
  histogram was declared, or vice versa) now logs a warning naming the metric
  — the value still folds, so a buggy step degrades its own metric instead of
  failing the run. Output formats are unchanged.

## WASM libraries: the `net` capability

- A library can now be granted `capabilities: [net: [hosts]]` for outgoing
  HTTP via `wasi:http`. Every request's destination is checked host-side
  against the allowlist (`api.acme.com` = any port, `api.acme.com:8443` =
  pinned port, `*.internal` = suffix wildcard); anything else fails with an
  error naming the destination. Requests are bounded at 30 s and 16 MiB per
  body. Raw sockets (`wasi:sockets`) stay unsupported. A component importing
  `wasi:http` without a `net` grant is a hard load error.

## Pro extension point: connection registries + `webrtc:` config gate

- `perfscale-connection` gains `ExtensionRegistries`, a type-erased parking
  lot where downstream pro action families (the upcoming `pro/webrtc-*`,
  RFC 007, and future `pro/*` modules) park live connections per VU
  iteration with the same Connection-ID model and iteration-end drain as the
  built-in ws/grpc/db families (reached by pro actions via
  `Context::extensions()`).
- New optional `webrtc:` config block (ICE servers, peer-connection
  guardrail). It requires the pro webrtc module: on builds without it,
  declaring the block fails load/lint/run with a clear "pro module required"
  error instead of silently doing nothing.

## Breaking: `capabilities:` is required on every library entry

- Every `libraries:` entry must now declare `capabilities:` explicitly —
  `capabilities: []` means "no grants". Documents that omitted the key now
  fail at load, `lint`, and run with a targeted error. Migration: add
  `capabilities: []` to every grant-less entry.

## `${vu}` token + new pro extension seams

- New built-in generator token `${vu}` — the current VU id (1-based) —
  usable anywhere generator tokens expand.
- `Context::call_library` lets pro action families invoke an RFC 005 library
  mid-step with computed arguments (policy, masking, and metrics go through
  the same path as `${alias.fn(...)}` tokens); `Context::vu_id()` exposes the
  VU id for per-VU artifact naming.
- Shared-variable drivers gain ephemeral ops (`apply_ephemeral`) —
  undeclared, TTL-bounded keys for engine-internal coordination (used by the
  pro `pro/webrtc-call@v1` rendezvous; the redis driver namespaces them under
  `perfscale:ephemeral:`).
