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

- **`std/llm@v1`: metrics-returning observer seam.** `register_llm_metrics_observer` lets downstream (pro) builds merge extra per-request metrics (ITL/TPOT percentiles, cost) into the step's `metrics` object under their own keys; engine `llm_*` keys win collisions, observer panics stay contained.
- **GPU samples carry pro extras.** `GpuSample.extra` (flattened in JSON) lets collectors registered via `register_gpu_collector` attach their own numeric fields — clocks, throttle bitmasks, per-process aggregates — to the run summary's GPU timeseries.
