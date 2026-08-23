# GPU benchmark suite

Local benchmark suite for **GPU inference**: perfscale's native engine drives
a local OpenAI-compatible model server (Ollama or vLLM) with
[`std/llm@v1`](../../docs/core/llm.md) while sampling the host GPU with the
[`gpu:` run config](../../docs/core/gpu.md). The GPU is the system under test
— the suite correlates TTFT / tokens-per-second with SM utilization and VRAM
pressure.

This suite is **local-only by design**: CI runners (`.github/workflows/bench.yml`)
have no GPU, so nothing here is wired into a workflow.

## Requirements

- NVIDIA GPU + `nvidia-smi` on PATH (from the driver), **or** a
  [dcgm-exporter](https://github.com/NVIDIA/dcgm-exporter) endpoint
  (`GPU_SOURCE=dcgm`).
- `python3` (results summary), `curl`.
- A perfscale binary — `run.sh` builds `target/release/perfscale` itself when
  missing (`cargo build --release`), or point `PERFSCALE_BIN` at one.
- A running model server (below). Without `nvidia-smi`/dcgm the runs still
  work, but GPU columns will be empty (the engine's GPU collection is
  best-effort).

## Model server: Ollama

```sh
ollama pull llama3.2:3b   # default $OLLAMA_MODEL
ollama serve              # usually already running as a service
```

Ollama exposes an OpenAI-compatible endpoint on `http://127.0.0.1:11434`.

## Model server: vLLM

Docker (needs the
[NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/install-guide.html)):

```sh
docker run --gpus all -p 8000:8000 \
  vllm/vllm-openai --model meta-llama/Llama-3.2-3B-Instruct
```

or pip: `pip install vllm && vllm serve meta-llama/Llama-3.2-3B-Instruct`.

vLLM serves the OpenAI API on `http://127.0.0.1:8000`. `$VLLM_MODEL` must
match the `--model` the server was started with (gated HF models also need
`HF_TOKEN`).

## Running

```sh
bench/gpu/run.sh                  # ollama, both profiles
bench/gpu/run.sh vllm             # vLLM only
bench/gpu/run.sh ollama vllm      # both servers

OLLAMA_MODEL=qwen3:4b bench/gpu/run.sh
PROFILES=stages bench/gpu/run.sh  # only the ramping-VU profile
GPU_SOURCE=dcgm DCGM_URL=http://127.0.0.1:9400/metrics bench/gpu/run.sh
```

`run.sh` checks the binary, GPU tooling, and server readiness, then runs each
profile per server. Per scenario it generates an override config
(`import:`-merged on top of the profile) pinning `variables.model` and the
GPU source — the checked-in YAML stays server-agnostic.

## Scenarios

| File | Kind | What it is |
|---|---|---|
| `ollama.yaml` | test definition | one `std/llm@v1` streaming chat completion vs Ollama (:11434); model from `${{ vars.model }}` |
| `vllm.yaml` | test definition | same against vLLM (:8000) |
| `stages.yaml` | config (profile) | stepped ramping VUs: plateaus at 2 / 8 / 16 concurrent requests, `gpu:` on |
| `arrival.yaml` | config (profile) | arrival-rate (open model): 1→16 completions/sec, `max_vus: 64`, `gpu:` on |

Both profiles exist to answer different questions:

- **`stages`** (closed model): how do tok/s and TTFT degrade as concurrency
  grows? Each plateau is a measurement point — read the periodic `[stats]`
  lines (they share a timeline with the GPU timeseries) to compare
  plateaus, not just the run-level aggregates.
- **`arrival`** (open model): what request rate can the server actually
  sustain? New requests arrive on schedule even when the server is behind;
  the rate where TTFT starts climbing and `dropped_iterations` first grows
  past 0 is the practical capacity for this model/prompt.

## Reading the results

Everything lands in `bench/gpu/results/<timestamp>/`:

- `<server>-<profile>.json` — `--summary-export` (run metadata, request
  summary, full GPU timeseries + per-device aggregates under `gpu`).
- `<server>-<profile>.log` — raw run output: per-request lines, periodic
  `[stats]`, the k6-style metric summary (`llm_ttft_ms`,
  `llm_tokens_per_sec`, `dropped_iterations`, …), the compact `gpu:` block.
- `config-<server>-<profile>.yaml` — the exact override config used.
- `meta.txt` — git SHA, perfscale version, GPU inventory.

The script ends with a table:

```text
scenario        reqs  req/s  tok/s avg  ttft p50 ms  ttft p95 ms  gpu util max  vram max MiB  dropped
--------------  ----  -----  ---------  -----------  -----------  ------------  ------------  -------
ollama-stages   152   0.42   38.71      212.40       890.15       100%          5104          0
ollama-arrival  210   0.63   31.05      340.72       2410.30      100%          5112          17
```

(numbers above are illustrative, from an imaginary mid-range GPU)

How to read it together with `docs/core/gpu.md`:

- `tok/s avg` flat across plateaus while `gpu util max` sits at ~100% → the
  GPU is the bottleneck (smaller model, quantization, more VRAM, or accept
  the rate).
- Utilization well below 100% with rising `ttft p95` → the server is the
  bottleneck (queueing, context/batch limits) — tune the server, not the GPU.
- `vram max` creeping to the card's total explains mid-run evictions/OOMs.
- `dropped` (arrival profile) > 0 means the arrival rate exceeded what
  `max_vus` workers could serve — the knee is just before that rate.

## Tuning

Edit the profiles in place — they are plain run configs: stage
durations/targets in `stages.yaml`, the rate ladder and `max_vus` in
`arrival.yaml`, sampling interval/source under `gpu:`. The prompt and
`max_tokens` live in the test definitions (`ollama.yaml` / `vllm.yaml`);
longer outputs shift the bottleneck from prefill to decode.
