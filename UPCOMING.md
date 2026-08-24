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

- **Shared variables: cross-VU mutable state.** New steps `std/set_shared_variable@v1` (`set`/`increment`/`append`) and `std/get_shared_variable@v1` (`get`/`pop`, blocking `wait_for` with `exists`/`equals`/`length_gte`, `extract` dotted paths) share atomic state across every VU in the run — producer/consumer queues, counters, barriers. Declare them at top-level `shared_variables:`; undeclared names and op/type mismatches fail validation before the run starts. Default `memory` driver is process-local; a driver seam (`register_shared_variable_driver`) lets pro builds add networked stores (e.g. Redis) for cross-agent sharing.
- **`std/llm@v1`: metrics-returning observer seam.** `register_llm_metrics_observer` lets downstream (pro) builds merge extra per-request metrics (ITL/TPOT percentiles, cost) into the step's `metrics` object under their own keys; engine `llm_*` keys win collisions, observer panics stay contained.
- **GPU samples carry pro extras.** `GpuSample.extra` (flattened in JSON) lets collectors registered via `register_gpu_collector` attach their own numeric fields — clocks, throttle bitmasks, per-process aggregates — to the run summary's GPU timeseries.
