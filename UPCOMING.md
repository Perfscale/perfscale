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
  backoff retries, and a final drain before exit. With `gpu.enabled` on,
  each snapshot also carries the GPU gauges taken since the previous one
  (`gpu_utilization_pct`, `gpu_memory_used_mib`, `gpu_memory_total_mib`,
  `gpu_temperature_c`, `gpu_power_w`, labelled per device) — live GPU state
  next to live latency in the controlplane UI.
- Docs (core/gpu): game-style rendering load example — a glmark2 render
  farm plus an NVENC encode sidecar orchestrated via `std/child_process@v1`
  before/after blocks, with the sweep method for finding a card's
  session-density ceiling.
- GPU metrics: new `powermetrics` source for Apple Silicon Macs — the
  integrated GPU gets a true utilization figure (active residency %) and
  rail power, and the Neural Engine (ANE/NPU) arrives as `ane_power_w`
  alongside `cpu_power_w`/`package_power_w` (power is the only ANE signal
  macOS exposes; there is no public NPU utilization API). Streams during-run
  like every other GPU gauge. Requires root: run the CLI under sudo or add a
  NOPASSWD sudoers rule for `/usr/bin/powermetrics` (see the gpu docs page).