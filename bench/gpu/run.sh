#!/usr/bin/env bash
set -euo pipefail

# GPU inference benchmark suite (local only — needs an NVIDIA GPU; the CI
# bench workflow has none, so this suite never runs there).
#
# Drives a local OpenAI-compatible model server (Ollama or vLLM) with
# std/llm@v1 under two load profiles while the engine samples the GPU (the
# `gpu:` config section, docs/core/gpu.md):
#
#   stages   – stepped ramping VUs: plateaus at 2/8/16 concurrent requests.
#              Shows where tokens/sec flattens and TTFT climbs as the GPU
#              saturates (compare plateaus via the periodic [stats] lines).
#   arrival  – arrival-rate (open model): the completion rate steps up every
#              minute. The rate where TTFT climbs and dropped_iterations
#              first grows is the server's practical capacity.
#
# Every scenario writes a --summary-export JSON plus the raw run log into
# bench/gpu/results/<timestamp>/, and the script ends with a compact table
# (requests, tok/s, TTFT p50/p95, GPU util/VRAM max, drops).
#
# Usage:
#   bench/gpu/run.sh                  # ollama, both profiles
#   bench/gpu/run.sh vllm             # vLLM only
#   bench/gpu/run.sh ollama vllm      # both servers
#
# Env:
#   PERFSCALE_BIN  binary under test (default target/release/perfscale,
#                  built with cargo build --release when missing)
#   PROFILES       profiles per server (default "stages arrival")
#   OLLAMA_MODEL   default llama3.2:3b
#   VLLM_MODEL     default meta-llama/Llama-3.2-3B-Instruct — must match the
#                  model the vLLM server was started with
#   GPU_SOURCE     nvidia-smi (default) | dcgm
#   DCGM_URL       dcgm-exporter endpoint for GPU_SOURCE=dcgm
#                  (default http://127.0.0.1:9400/metrics)

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIR="$ROOT/bench/gpu"
BIN="${PERFSCALE_BIN:-$ROOT/target/release/perfscale}"

PROFILES="${PROFILES:-stages arrival}"
OLLAMA_MODEL="${OLLAMA_MODEL:-llama3.2:3b}"
VLLM_MODEL="${VLLM_MODEL:-meta-llama/Llama-3.2-3B-Instruct}"
GPU_SOURCE="${GPU_SOURCE:-nvidia-smi}"
DCGM_URL="${DCGM_URL:-http://127.0.0.1:9400/metrics}"

SERVERS=("$@")
[[ ${#SERVERS[@]} -gt 0 ]] || SERVERS=(ollama)
for s in "${SERVERS[@]}"; do
  case "$s" in
    ollama | vllm) ;;
    *)
      echo "unknown server '$s' — use 'ollama' and/or 'vllm'" >&2
      exit 2
      ;;
  esac
done

server_url() { # $1 server → base URL probed for readiness
  case "$1" in
    ollama) echo "http://127.0.0.1:11434" ;;
    vllm) echo "http://127.0.0.1:8000" ;;
  esac
}
server_model() {
  case "$1" in
    ollama) echo "$OLLAMA_MODEL" ;;
    vllm) echo "$VLLM_MODEL" ;;
  esac
}
server_hint() {
  case "$1" in
    ollama) echo "start it with: ollama serve  (and: ollama pull $OLLAMA_MODEL)" ;;
    vllm) echo "start it with: docker run --gpus all -p 8000:8000 vllm/vllm-openai --model $VLLM_MODEL" ;;
  esac
}

# --- prerequisites ----------------------------------------------------------

if [[ ! -x "$BIN" ]]; then
  echo "building perfscale (release)..." >&2
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
fi

command -v python3 >/dev/null 2>&1 || {
  echo "python3 is required for the results summary" >&2
  exit 1
}

case "$GPU_SOURCE" in
  nvidia-smi)
    command -v nvidia-smi >/dev/null 2>&1 ||
      echo "warning: nvidia-smi not on PATH — runs proceed but GPU metrics will be absent" >&2
    ;;
  dcgm)
    curl -fsS --max-time 3 "$DCGM_URL" >/dev/null 2>&1 ||
      echo "warning: dcgm-exporter at $DCGM_URL unreachable — GPU metrics will be absent" >&2
    ;;
  *)
    echo "unknown GPU_SOURCE '$GPU_SOURCE' — use 'nvidia-smi' or 'dcgm'" >&2
    exit 2
    ;;
esac

for s in "${SERVERS[@]}"; do
  url="$(server_url "$s")"
  if ! curl -fsS --max-time 5 "$url/v1/models" >/dev/null 2>&1; then
    echo "error: $s server not reachable at $url" >&2
    echo "  $(server_hint "$s")" >&2
    exit 1
  fi
done

# --- run --------------------------------------------------------------------

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="$DIR/results/$STAMP"
mkdir -p "$OUT"

{
  echo "timestamp: $STAMP"
  echo "git: $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo "perfscale: $("$BIN" --version 2>/dev/null || echo unknown)"
  echo "gpu_source: $GPU_SOURCE"
  if [[ "$GPU_SOURCE" == nvidia-smi ]] && command -v nvidia-smi >/dev/null 2>&1; then
    nvidia-smi --query-gpu=index,name,memory.total --format=csv,noheader 2>/dev/null || true
  fi
} >"$OUT/meta.txt"

FAILED=()
for s in "${SERVERS[@]}"; do
  for p in $PROFILES; do
    [[ -f "$DIR/$p.yaml" ]] || {
      echo "skipping unknown profile '$p' (no $DIR/$p.yaml)" >&2
      continue
    }
    # Override config: inherits the profile via `import:` (local-path imports
    # deep-merge — objects merge, scalars here win) and pins the model and
    # GPU source for this server.
    cfg="$OUT/config-$s-$p.yaml"
    {
      echo "import: $DIR/$p.yaml"
      echo "variables:"
      echo "  model: \"$(server_model "$s")\""
      echo "gpu:"
      echo "  enabled: true"
      echo "  source: $GPU_SOURCE"
      [[ "$GPU_SOURCE" == dcgm ]] && echo "  dcgm_url: $DCGM_URL"
    } >"$cfg"

    echo "scenario: $s + $p (model $(server_model "$s"))" >&2
    if ! "$BIN" run -f "$DIR/$s.yaml" -c "$cfg" \
      --summary-export "$OUT/$s-$p.json" 2>&1 | tee "$OUT/$s-$p.log"; then
      echo "warning: scenario $s-$p failed — continuing" >&2
      FAILED+=("$s-$p")
    fi
  done
done

# --- summary -----------------------------------------------------------------

python3 - "$OUT" <<'EOF'
import json, re, sys
from pathlib import Path

out = Path(sys.argv[1])


def num(pattern, text):
    m = re.search(pattern, text, re.M)
    return m.group(1) if m else None


rows = []
for log in sorted(out.glob("*-*.log")):
    name = log.stem
    text = log.read_text(errors="replace")
    reqs = re.search(r"^http_reqs.*?: (\d+) ([\d.]+)/s", text, re.M)
    reqs_n, rps = (reqs.group(1), reqs.group(2)) if reqs else ("—", "—")
    tps = num(r"^llm_tokens_per_sec.*?: avg=([\d.]+)ms", text) or "—"
    ttft_p50 = num(r"^llm_ttft_ms.*?: .*?p\(50\)=([\d.]+)ms", text) or "—"
    ttft_p95 = num(r"^llm_ttft_ms.*?: .*?p\(95\)=([\d.]+)ms", text) or "—"
    dropped = num(r"^dropped_iterations.*?: (\d+)", text) or "0"

    util = vram = "—"
    export = log.with_suffix(".json")
    if export.exists():
        try:
            gpu = json.loads(export.read_text()).get("gpu") or {}
            devs = gpu.get("devices") or []
            utils = [d["max_utilization_pct"] for d in devs if d.get("max_utilization_pct") is not None]
            vrams = [d["max_memory_used_mib"] for d in devs if d.get("max_memory_used_mib") is not None]
            if utils:
                util = f"{max(utils):.0f}%"
            if vrams:
                vram = f"{max(vrams):.0f}"
        except (json.JSONDecodeError, KeyError):
            pass
    rows.append((name, reqs_n, rps, tps, ttft_p50, ttft_p95, util, vram, dropped))

header = ("scenario", "reqs", "req/s", "tok/s avg", "ttft p50 ms", "ttft p95 ms", "gpu util max", "vram max MiB", "dropped")
widths = [max(len(str(c)) for c in col) for col in zip(header, *rows)] if rows else [len(h) for h in header]
line = "  ".join(h.ljust(w) for h, w in zip(header, widths))
print()
print(line)
print("  ".join("-" * w for w in widths))
for r in rows:
    print("  ".join(str(c).ljust(w) for c, w in zip(r, widths)))
EOF

echo
echo "results written to $OUT"
if [[ ${#FAILED[@]} -gt 0 ]]; then
  echo "failed scenarios: ${FAILED[*]}" >&2
  exit 1
fi
