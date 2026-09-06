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

- GPU metrics: the best-effort "gpu metrics unavailable" warning now goes
  out as a system log line instead of stderr — on the agent → controlplane
  wire a stderr line becomes an `[err]` entry that fails the whole run, so
  a GPU-less machine could not pass any `gpu: enabled` test.
- During-run metrics streaming: `report.during_run: true` now streams
  cumulative metric snapshots (Prometheus-style quantile/`_count`/`_sum`
  and `_total` samples) to the report URL *while the run is in progress*,
  not just the summary at the end. Batching is CPU-gated — under host CPU
  pressure the shipper drops snapshots and holds batches instead of
  stealing cycles from the VUs — with bounded pending batches, exponential
  backoff retries, and a final drain before exit.
- Docs (core/gpu): game-style rendering load example — a glmark2 render
  farm plus an NVENC encode sidecar orchestrated via `std/child_process@v1`
  before/after blocks, with the sweep method for finding a card's
  session-density ceiling.