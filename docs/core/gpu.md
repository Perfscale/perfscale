# GPU metrics

The native engine can sample the host's GPUs for the whole run — utilization,
VRAM, temperature, and power — and land a `gpu` section in the run summary.
This exists primarily for **LLM load testing**: with [`std/llm@v1`](llm.md)
against a local server (Ollama, vLLM, …) the GPU *is* the system under test,
and correlating TTFT/tokens-per-second with SM utilization and memory
pressure is how you tell "model is saturated" apart from "server is
misconfigured".

Collection is **best-effort**: no GPU, a missing `nvidia-smi` binary, or an
unreachable exporter logs one warning at run start and the run continues
without GPU metrics — it never fails the run.

## Configuration

The `gpu:` block sits in the [run config](../yaml-reference.md#config--c-configyaml)
next to `vus`/`duration` (native engine only):

```yaml
vus: 10
duration: 5m
gpu:
  enabled: true
  interval_ms: 1000      # default 1000 (min 10)
  source: nvidia-smi     # nvidia-smi (default) | dcgm
  dcgm_url: http://127.0.0.1:9400/metrics  # for source: dcgm
  devices: [0, 1]        # optional; default — every GPU the source reports
```

| Field | Default | Description |
|---|---|---|
| `enabled` | `false` | Master switch; the section is ignored without it |
| `interval_ms` | `1000` | Sampling interval (one snapshot per GPU per tick) |
| `source` | `nvidia-smi` | `nvidia-smi` shells out to the binary; `dcgm` polls a dcgm-exporter HTTP endpoint |
| `dcgm_url` | `http://127.0.0.1:9400/metrics` | dcgm-exporter metrics endpoint (`source: dcgm` only) |
| `devices` | all | Restrict sampling to these GPU indices |

## Sources

**`nvidia-smi`** (default) — runs
`nvidia-smi --query-gpu=index,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw --format=csv,noheader,nounits`
once per tick. Works anywhere the NVIDIA driver is installed, no extra
daemon needed. Fields the driver reports as `N/A` (e.g. power draw on some
virtualized GPUs) are recorded as absent, not zero.

**`dcgm`** — HTTP GET on `dcgm_url` and parses the Prometheus text format of
[dcgm-exporter](https://github.com/NVIDIA/dcgm-exporter):
`DCGM_FI_DEV_GPU_UTIL`, `DCGM_FI_DEV_FB_USED`/`DCGM_FI_DEV_FB_FREE` (VRAM
total is derived as used+free), `DCGM_FI_DEV_GPU_TEMP`,
`DCGM_FI_DEV_POWER_USAGE`. Better for GPU servers and Kubernetes, where
dcgm-exporter is typically already running.

## Output

While the VUs run, the sampler takes one snapshot per GPU per tick (the
first immediately at run start). After the metric summary the run prints a
compact block:

```text
gpu: 1 device, 300 samples every 1000ms (nvidia-smi)
gpu0: util avg=64.3% max=100.0% vram max=41088/81559MiB temp max=71.0C power max=512.3W
```

…followed by one machine-readable `gpu: {...}` line with the full
timeseries, collected into `perfscale run --summary-export` under `gpu`
(same framing as the `thresholds: {...}` gate line):

```json
{
  "gpu": {
    "source": "nvidia-smi",
    "interval_ms": 1000,
    "devices": [
      {
        "index": 0,
        "samples": [
          { "ts_ms": 1720000000000, "index": 0, "utilization_pct": 64.0,
            "memory_used_mib": 41088.0, "memory_total_mib": 81559.0,
            "temperature_c": 71.0, "power_w": 512.3 }
        ],
        "avg_utilization_pct": 64.3,
        "max_utilization_pct": 100.0,
        "max_memory_used_mib": 41088.0,
        "memory_total_mib": 81559.0,
        "max_temperature_c": 71.0,
        "max_power_w": 512.3
      }
    ]
  }
}
```

Each sample carries `ts_ms` (epoch milliseconds) on the same timeline as the
[stats](metrics.md#live-stats-lines) lines, so throughput/latency and GPU
state can be charted together. Absent optional fields mean the source
reported `N/A` for that metric. Markdown exports (`--summary-export out.md`)
get one compact row per device per aggregate.

## Example: Ollama under load, GPU watch on

```yaml
# config.yaml
vus: 8
duration: 2m
gpu:
  enabled: true
```

```yaml
# test.yaml
steps:
  - name: llama completion
    use: std/llm@v1
    with:
      url: http://127.0.0.1:11434/v1/chat/completions
      model: llama3.1
      prompt: "Summarize the CAP theorem in two sentences."
      max_tokens: 128
    check:
      status: 200
```

```sh
perfscale run -f test.yaml -c config.yaml --summary-export gpu-run.json
```

Reading the result together: `llm_tokens_per_sec` flat while
`gpu0 util avg` sits at ~100% → the GPU is the bottleneck (add a card,
shard the model, or lower `vus`); util well below 100% with rising TTFT →
look at the server (queueing, context limits) instead. VRAM creeping to
`memory_total_mib` explains evictions/OOMs mid-run.

## Example: game-style rendering load

GPU load testing is not only about LLM servers. The other classic question
is **session density**: how many concurrent render sessions — game clients
on a cloud-gaming node, streaming viewports, digital-twin renderers — one
card carries before the frame rate collapses. The pattern is the same as
above, except the "system under test" is a set of renderer processes that
perfscale orchestrates with
[`std/child_process@v1`](actions.md#stdchild_processv1) while the `gpu:`
sampler records what the card is doing. A `before:` sidecar adds the second
ingredient of a real node — a heavy background GPU job (the encode stage)
competing with the sessions for the same card.

Any renderer that prints FPS works. This example uses
[glmark2](https://github.com/glmark2/glmark2) (OpenGL, `apt install
glmark2`) looping its 3D scenes as a stand-in for a game client; `vkmark`
is the Vulkan equivalent, and a headless Unity/Unreal build drops in
unchanged — only the `command` differs.

```yaml
# config.yaml
vus: 1                      # the renderers are the load; one VU just keeps
duration: 5m                # the run open (see test.yaml below)
allow_process_actions: true # required for child_process/kill_process

gpu:
  enabled: true
  interval_ms: 1000

before:
  # One "game session" = one renderer process. The farm spawns as a single
  # managed process group, so `after:` stops every session at once.
  - name: render-farm
    uses: std/child_process@v1
    with:
      command: sh
      args: ["-c", "for i in $(seq 4); do glmark2 --run-forever & done; wait"]
      waitUntil:
        stdout_contains: GL_RENDERER  # GL context is up
        on_timeout: continue
      restart: never                  # a crashed session must not respawn a 2nd farm

  # Sidecar: a heavy GPU job sharing the card with the sessions — the encode
  # stage of a game-streaming pipeline. Looped 1080p60 from a generated
  # source, NVENC-encoded, discarded to null.
  - name: encode-sidecar
    uses: std/child_process@v1
    with:
      command: ffmpeg
      args: ["-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=60",
             "-c:v", "h264_nvenc", "-f", "null", "-"]
      waitUntil:
        stderr_contains: "Press [q]"  # ffmpeg reports the running loop on stderr
        on_timeout: continue
      restart: on-failure             # a crashed encoder comes back

after:
  - name: stop the farm
    uses: std/kill_process@v1
    with: { name: render-farm }       # tree: true by default → every session
  - name: stop the sidecar
    uses: std/kill_process@v1
    with: { name: encode-sidecar }
```

```yaml
# test.yaml
steps:
  - name: keep the run open
    use: std/sleep@v1
    with: { seconds: 30 }
```

```sh
perfscale run -f test.yaml -c config.yaml --summary-export render.json
```

Headless nodes: glmark2 needs a GL context. On a GPU server without a
display use the DRM build (`glmark2-drm` / `glmark2-es2-drm` — renders via
GBM straight on the card) or wrap the command in `xvfb-run -a`.

### Reading the result

The renderer's FPS lines stream into the run log with a `render-farm: `
prefix; the `gpu:` summary records what the card did meanwhile. The method
is a sweep, not a single run — raise the session count (`seq 4` → 1, 2, 4,
8) between runs:

- per-scene FPS divided by ~N while `util max` pins at 100% → the GPU is
  saturated; that session count is the card's ceiling for this workload;
- FPS degrades while util stays below 100% → the limit is elsewhere (CPU,
  context switching) — cross-check `temp max` / `power max` for thermal or
  power throttling;
- `vram max` per session count answers the capacity question directly: how
  many sessions fit into `memory_total_mib` before the driver starts
  swapping;
- the sidecar's price is the **FPS delta** between a farm-only run (comment
  the `encode-sidecar` block out) and a farm+sidecar run at the same session
  count — that is what sharing the card with the encode pipeline costs a
  cloud-gaming node.

perfscale does not parse FPS into metrics — frame-rate numbers live in the
run log; the `gpu:` timeseries (each sample stamped `ts_ms` on the [stats
timeline](metrics.md#live-stats-lines)) is what you chart against them.

One caveat for runs like this: `utilization.gpu` reports the 3D/compute
engine — the NVENC encoder the sidecar burns is **not** counted there, so
judge the sidecar by VRAM, power draw, and the FPS it costs the sessions,
not by the util line.

## GPU benchmark suite

The repo ships a ready-made local suite in
[`bench/gpu/`](https://github.com/Perfscale/perfscale/tree/main/bench/gpu):
`std/llm@v1` scenarios against Ollama and vLLM with `gpu:` metrics on, a
stepped ramping-VU profile (concurrency vs tok/s / TTFT degradation) and an
arrival-rate profile (find the rate where TTFT and `dropped_iterations`
climb). It is local-only — CI runners have no GPU.

```sh
bench/gpu/run.sh ollama        # or: vllm, or both
```

…runs each profile, writes `--summary-export` JSONs and raw logs to
`bench/gpu/results/<timestamp>/`, and prints a compact table:

```text
scenario        reqs  req/s  tok/s avg  ttft p50 ms  ttft p95 ms  gpu util max  vram max MiB  dropped
--------------  ----  -----  ---------  -----------  -----------  ------------  ------------  -------
ollama-stages   152   0.42   38.71      212.40       890.15       100%          5104          0
ollama-arrival  210   0.63   31.05      340.72       2410.30      100%          5112          17
```

Setup (Ollama / vLLM), requirements, and how to read the numbers:
`bench/gpu/README.md`.

## Extension seam

`perfscale-core` exposes `gpu::GpuCollector` and
`gpu::register_gpu_collector` — the same pattern as
[`register_pubsub_driver`](pubsub.md#drivers): a downstream (proprietary)
build can register richer collectors (NVML-based per-process memory, SM
clock/throttle reasons, `rocm-smi` for AMD, `powermetrics` for Apple
silicon) and select them via `gpu.source`, or shadow the built-ins under
their own names. The basic metrics above are the OSS baseline every
collector reports.
