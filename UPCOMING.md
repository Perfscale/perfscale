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

- **GPU metrics for runs (`gpu:` config section)** — the native engine can
  now sample the host's GPUs for the whole run: utilization %, VRAM
  used/total, temperature, and power draw, once per `interval_ms` via
  `nvidia-smi` (default) or a dcgm-exporter endpoint (`source: dcgm`).
  Built to correlate `std/llm@v1` load with GPU state when the model server
  is local (Ollama, vLLM, …). The run summary gains a compact per-device
  block and a machine-readable `gpu: {...}` line (full timeseries), embedded
  into `--summary-export` under `gpu`. Collection is best-effort — no GPU or
  missing tooling logs one warning and never fails the run. A public
  collector seam (`register_gpu_collector`, `trait GpuCollector`) is where
  the pro build plugs in detailed NVML/rocm-smi/powermetrics metrics. Guide:
  `docs/core/gpu.md`.
