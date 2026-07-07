#!/usr/bin/env bash
set -euo pipefail

# Engine benchmark suites. Every scenario hits the same `perfscale serve`
# endpoints, so gaps between a `perfscale (*)` row and its native counterpart
# are perfscale's wrapping overhead, not the underlying tool.
#
# Suites (select with SUITES="..."; default runs all):
#   overhead    – wall-clock at the configured duration (hyperfine). With a
#                 fixed test duration this mostly proves the wrapper adds no
#                 wall time; throughput differences live in `throughput`.
#   throughput  – one instrumented run per scenario: requests, RPS, latency
#                 percentiles, CPU, CPU-per-request, peak RSS, IO ops.
#   startup     – hyperfine at a 1s duration, where wrapper startup cost is
#                 visible instead of drowned by the test duration.
#   scaling     – VU sweep per engine: RPS / p95 / RSS / CPU as VUs grow.
#   saturation  – high-VU short run per engine: approximate max RPS.
#   yaml        – native engine step scenarios: GET, GET+check, POST JSON,
#                 multi-step with interpolation.
#   tls         – engines against `perfscale serve --tls` (self-signed HTTPS).
#
# Scenarios whose engine isn't on PATH are skipped, not failed. hyperfine is
# required only for the overhead/startup suites.
#
# Outputs: $OUTPUT (markdown report) and $RESULTS (machine-readable JSON,
# consumed by scripts/bench_compare.py for regression tracking).

VUS="${VUS:-10}"
DURATION="${DURATION:-15s}"
WARMUP="${WARMUP:-1}"
RUNS="${RUNS:-5}"
PORT="${PORT:-18999}"
TLS_PORT="${TLS_PORT:-18998}"
OUTPUT="${OUTPUT:-bench-report.md}"
RESULTS="${RESULTS:-bench-results.json}"
SUITES="${SUITES:-overhead throughput startup scaling saturation yaml tls}"

STARTUP_DURATION="${STARTUP_DURATION:-1s}"
STARTUP_RUNS="${STARTUP_RUNS:-5}"
SCALING_VUS="${SCALING_VUS:-10 50 200}"
SCALING_DURATION="${SCALING_DURATION:-10s}"
SAT_VUS="${SAT_VUS:-256}"
SAT_DURATION="${SAT_DURATION:-15s}"
YAML_DURATION="${YAML_DURATION:-10s}"
TLS_DURATION="${TLS_DURATION:-10s}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${PERFSCALE_BIN:-$ROOT/target/release/perfscale}"
METRICS="python3 $ROOT/scripts/bench_metrics.py"

if [[ ! -x "$BIN" ]]; then
  echo "building perfscale (release)..." >&2
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
fi

WORKDIR="$(mktemp -d)"
RESULTS_D="$WORKDIR/results"
mkdir -p "$RESULTS_D"
SERVE_PID=""
TLS_SERVE_PID=""
cleanup() {
  [[ -n "$SERVE_PID" ]] && kill "$SERVE_PID" 2>/dev/null || true
  [[ -n "$TLS_SERVE_PID" ]] && kill "$TLS_SERVE_PID" 2>/dev/null || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

TARGET="http://127.0.0.1:${PORT}"
TLS_TARGET="https://127.0.0.1:${TLS_PORT}"

has_suite() { case " $SUITES " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
HAS_K6=0
HAS_LOCUST=0
HAS_HYPERFINE=0
command -v k6 >/dev/null 2>&1 && HAS_K6=1
command -v locust >/dev/null 2>&1 && HAS_LOCUST=1
command -v hyperfine >/dev/null 2>&1 && HAS_HYPERFINE=1
[[ "$HAS_K6" == 1 ]] || echo "skipping k6 scenarios: k6 not on PATH" >&2
[[ "$HAS_LOCUST" == 1 ]] || echo "skipping locust scenarios: locust not on PATH" >&2

# ---------------------------------------------------------------------------
# Servers
# ---------------------------------------------------------------------------

wait_for() { # $1 url, $2 extra curl flags
  for _ in $(seq 1 50); do
    curl -fs ${2:-} "$1" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

"$BIN" serve --port "$PORT" >"$WORKDIR/serve.log" 2>&1 &
SERVE_PID=$!
if ! wait_for "$TARGET/health"; then
  echo "perfscale serve never came up:" >&2
  cat "$WORKDIR/serve.log" >&2
  exit 1
fi

HAS_TLS=0
if has_suite tls; then
  if "$BIN" serve --help 2>/dev/null | grep -q -- '--tls'; then
    "$BIN" serve --port "$TLS_PORT" --tls >"$WORKDIR/serve-tls.log" 2>&1 &
    TLS_SERVE_PID=$!
    if wait_for "$TLS_TARGET/health" "-k"; then
      HAS_TLS=1
    else
      echo "skipping tls suite: serve --tls never came up" >&2
    fi
  else
    echo "skipping tls suite: this perfscale binary has no 'serve --tls'" >&2
  fi
fi

# ---------------------------------------------------------------------------
# Workloads
# ---------------------------------------------------------------------------

# One k6 script for every suite — load shape and target come from BENCH_* env
# vars so hyperfine/scaling/tls runs share it. summaryTrendStats adds the
# p(50)/p(99) columns the report parses.
cat >"$WORKDIR/script.js" <<'EOF'
import http from 'k6/http';
export const options = {
  vus: Number(__ENV.BENCH_VUS || 1),
  duration: __ENV.BENCH_DURATION || '15s',
  insecureSkipTLSVerify: __ENV.BENCH_INSECURE === '1',
  summaryTrendStats: ['avg', 'min', 'med', 'max', 'p(50)', 'p(90)', 'p(95)', 'p(99)'],
};
export default function () {
  http.get(__ENV.BENCH_TARGET);
}
EOF

cat >"$WORKDIR/locustfile.py" <<'EOF'
import os

from locust import HttpUser, task, constant


class HealthUser(HttpUser):
    wait_time = constant(0)

    def on_start(self):
        if os.environ.get("BENCH_INSECURE") == "1":
            self.client.verify = False

    @task
    def health(self):
        self.client.get("/health")
EOF

cat >"$WORKDIR/yaml-get.yaml" <<EOF
steps:
  - name: health check
    use: std/http@v1
    with:
      method: GET
      url: "${TARGET}/health"
EOF

cat >"$WORKDIR/yaml-check.yaml" <<EOF
steps:
  - name: health check
    use: std/http@v1
    with:
      method: GET
      url: "${TARGET}/health"
    check:
      status: 200
EOF

cat >"$WORKDIR/yaml-post.yaml" <<EOF
steps:
  - name: post metrics
    use: std/http@v1
    with:
      method: POST
      url: "${TARGET}/api/v1/metrics"
      body:
        lines: []
EOF

cat >"$WORKDIR/yaml-multi.yaml" <<EOF
steps:
  - name: fetch
    use: std/http@v1
    with:
      method: GET
      url: "${TARGET}/health"
    outputs: resp
  - name: follow-up
    use: std/http@v1
    with:
      method: GET
      url: "${TARGET}/health"
      headers:
        x-prev-status: "\${{ resp.status }}"
    check:
      status: 200
EOF

cat >"$WORKDIR/yaml-tls.yaml" <<EOF
steps:
  - name: tls health check
    use: std/http@v1
    with:
      method: GET
      url: "${TLS_TARGET}/health"
      insecure: true
EOF

# Load config for the native engine / wrapped locust, one file per (vus,
# duration) pair. Prints the path.
cfg() {
  local path="$WORKDIR/config-$1-$2.yaml"
  [[ -f "$path" ]] || printf 'vus: %s\nduration: %s\n' "$1" "$2" >"$path"
  echo "$path"
}

# Command builders: $1 vus, $2 duration, $3 target base URL, $4 insecure(0/1)
cmd_k6_native() {
  echo "BENCH_VUS=$1 BENCH_DURATION=$2 BENCH_TARGET=$3/health BENCH_INSECURE=$4 k6 run --quiet $WORKDIR/script.js"
}
cmd_k6_wrapped() {
  echo "BENCH_VUS=$1 BENCH_DURATION=$2 BENCH_TARGET=$3/health BENCH_INSECURE=$4 $BIN run --k6 $WORKDIR/script.js"
}
cmd_locust_native() { # $5 csv prefix
  echo "BENCH_INSECURE=$4 locust -f $WORKDIR/locustfile.py --headless -u $1 -r $1 -t $2 --host $3 --only-summary --csv $5"
}
cmd_locust_wrapped() {
  echo "BENCH_INSECURE=$4 $BIN run --locust $WORKDIR/locustfile.py --host $3 -c $(cfg "$1" "$2")"
}
cmd_yaml() { # $1 vus, $2 duration, $3 test file, $4 extra flags (optional)
  echo "$BIN run -f $3 -c $(cfg "$1" "$2")${4:+ $4}"
}

# ---------------------------------------------------------------------------
# /usr/bin/time instrumentation
# ---------------------------------------------------------------------------

if /usr/bin/time -v true >/dev/null 2>&1; then
  TIME_STYLE="gnu"
elif /usr/bin/time -l true >/dev/null 2>&1; then
  TIME_STYLE="bsd"
else
  TIME_STYLE=""
  echo "resource columns unavailable: /usr/bin/time not found" >&2
fi

# Run $3 (a shell command string) with stdout to $1 and `/usr/bin/time`
# stats parsed into T_* globals.
T_WALL="?" T_USER="?" T_SYS="?" T_USER_S=0 T_SYS_S=0 T_RSS="?" T_RSS_MIB=0 T_IO="?"
run_timed() {
  local out="$1" tf="$WORKDIR/time.$$" cmd="$2"
  # `grep | awk || true`: with pipefail a missing stat line would otherwise
  # abort the whole bench via set -e; a "?" cell is better than no report.
  if [[ "$TIME_STYLE" == "gnu" ]]; then
    /usr/bin/time -v bash -c "$cmd" >"$out" 2>"$tf" || true
    T_WALL=$(grep 'Elapsed (wall clock)' "$tf" | awk -F': ' '{print $2}' || true)
    T_USER_S=$(grep -m1 'User time' "$tf" | awk -F': ' '{print $2}' || true)
    T_SYS_S=$(grep -m1 'System time' "$tf" | awk -F': ' '{print $2}' || true)
    local rss_kb io_in io_out
    rss_kb=$(grep 'Maximum resident set size' "$tf" | awk -F': ' '{print $2}' || true)
    T_RSS_MIB=$(awk "BEGIN{printf \"%.1f\", ${rss_kb:-0}/1024}")
    io_in=$(grep 'File system inputs' "$tf" | awk -F': ' '{print $2}' || true)
    io_out=$(grep 'File system outputs' "$tf" | awk -F': ' '{print $2}' || true)
    T_IO="${io_in:-0} in / ${io_out:-0} out"
  elif [[ "$TIME_STYLE" == "bsd" ]]; then
    /usr/bin/time -l bash -c "$cmd" >"$out" 2>"$tf" || true
    T_WALL=$(grep ' real' "$tf" | awk '{print $1"s"}' || true)
    T_USER_S=$(grep ' real' "$tf" | awk '{print $3}' || true)
    T_SYS_S=$(grep ' real' "$tf" | awk '{print $5}' || true)
    local rss_bytes io_in io_out
    rss_bytes=$(grep 'maximum resident set size' "$tf" | awk '{print $1}' || true)
    T_RSS_MIB=$(awk "BEGIN{printf \"%.1f\", ${rss_bytes:-0}/1048576}")
    io_in=$(grep 'block input operations' "$tf" | awk '{print $1}' || true)
    io_out=$(grep 'block output operations' "$tf" | awk '{print $1}' || true)
    T_IO="${io_in:-0} in / ${io_out:-0} out"
  else
    bash -c "$cmd" >"$out" 2>/dev/null || true
    T_USER_S=0 T_SYS_S=0 T_RSS_MIB=0 T_WALL="?" T_IO="?"
  fi
  T_USER="${T_USER_S:-0}s"
  T_SYS="${T_SYS_S:-0}s"
  T_RSS="${T_RSS_MIB} MiB"
}

cpu_per_req() { # $1 requests → µs of CPU per request, or —
  awk "BEGIN{ r=$1; if (r > 0) printf \"%.1f\", (${T_USER_S:-0}+${T_SYS_S:-0})*1000000/r; else printf \"—\" }"
}

# Instrumented run + metric parse into shell vars. $1 label, $2 cmd,
# $3 parse kind (text|locust-csv), $4 file to parse (defaults to stdout log).
requests=0 rps=0 avg_ms=0 p50_ms=0 p90_ms=0 p95_ms=0 p99_ms=0 min_ms=0 max_ms=0 err_pct=0 parse_ok=0
measure() {
  local label="$1" cmd="$2" kind="${3:-text}" parse_file="${4:-}"
  local out="$WORKDIR/out.$$"
  run_timed "$out" "$cmd"
  eval "$($METRICS parse "$kind" "${parse_file:-$out}")"
  if [[ "$parse_ok" != 1 ]]; then
    echo "warning: no metrics parsed for '$label'" >&2
  fi
}

json_row() { # $1 suite file, $2 label — records last measure() + T_* values
  $METRICS append "$RESULTS_D/$1" "$2" \
    requests="$requests" rps="$rps" avg_ms="$avg_ms" p50_ms="$p50_ms" \
    p90_ms="$p90_ms" p95_ms="$p95_ms" p99_ms="$p99_ms" err_pct="$err_pct" \
    user_s="${T_USER_S:-0}" sys_s="${T_SYS_S:-0}" rss_mib="$T_RSS_MIB"
}

# ---------------------------------------------------------------------------
# Scenario list shared by overhead/throughput/startup: name|builder|kind
# ---------------------------------------------------------------------------

scenario_names=()
scenario_builders=()
if [[ "$HAS_LOCUST" == 1 ]]; then
  scenario_names+=("locust (native)" "perfscale (locust)")
  scenario_builders+=("cmd_locust_native" "cmd_locust_wrapped")
fi
if [[ "$HAS_K6" == 1 ]]; then
  scenario_names+=("k6 (native)" "perfscale (k6)")
  scenario_builders+=("cmd_k6_native" "cmd_k6_wrapped")
fi
scenario_names+=("perfscale (yaml)")
scenario_builders+=("cmd_yaml_get")
cmd_yaml_get() { cmd_yaml "$1" "$2" "$WORKDIR/yaml-get.yaml"; }
cmd_yaml_get_quiet() { cmd_yaml "$1" "$2" "$WORKDIR/yaml-get.yaml" "--quiet"; }

# The yaml engine logs one line per request by default, which costs real CPU
# and syscalls under load — the quiet row shows the engine's price without
# that logging, side by side with the logged one so the comparison is honest.
HAS_QUIET=0
if "$BIN" run --help 2>/dev/null | grep -q -- '--quiet'; then
  HAS_QUIET=1
  scenario_names+=("perfscale (yaml quiet)")
  scenario_builders+=("cmd_yaml_get_quiet")
else
  echo "skipping quiet scenarios: this perfscale binary has no 'run --quiet'" >&2
fi

build_cmd() { # $1 builder, $2 vus, $3 duration, $4 csv prefix (locust native)
  case "$1" in
    cmd_locust_native) cmd_locust_native "$2" "$3" "$TARGET" 0 "$4" ;;
    cmd_locust_wrapped) cmd_locust_wrapped "$2" "$3" "$TARGET" 0 ;;
    cmd_k6_native) cmd_k6_native "$2" "$3" "$TARGET" 0 ;;
    cmd_k6_wrapped) cmd_k6_wrapped "$2" "$3" "$TARGET" 0 ;;
    cmd_yaml_get) cmd_yaml_get "$2" "$3" ;;
    cmd_yaml_get_quiet) cmd_yaml_get_quiet "$2" "$3" ;;
  esac
}

run_hyperfine() { # $1 vus, $2 duration, $3 runs, $4 md out, $5 json out
  local args=(--warmup "$WARMUP" --runs "$3" --export-markdown "$4" --export-json "$5")
  local i
  for i in "${!scenario_names[@]}"; do
    args+=(--command-name "${scenario_names[$i]}" \
      "$(build_cmd "${scenario_builders[$i]}" "$1" "$2" "$WORKDIR/loc-hf")")
  done
  hyperfine "${args[@]}"
}

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

: >"$OUTPUT"
section() { printf '\n## %s\n\n' "$1" >>"$OUTPUT"; }

$METRICS setobj "$RESULTS_D/meta.json" \
  vus="$VUS" duration="$DURATION" runs="$RUNS" \
  git="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)" \
  timestamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# --- overhead ---------------------------------------------------------------

if has_suite overhead; then
  if [[ "$HAS_HYPERFINE" == 1 ]]; then
    echo "suite: overhead" >&2
    run_hyperfine "$VUS" "$DURATION" "$RUNS" "$WORKDIR/overhead.md" "$RESULTS_D/overhead.json"
    section "Wall-clock at fixed duration (hyperfine, ${DURATION})"
    cat "$WORKDIR/overhead.md" >>"$OUTPUT"
    printf '\n_All scenarios run for a fixed %s, so wall time mostly measures the\nduration itself — near-1.00 relative numbers mean the wrapper adds no wall\ntime. Startup cost is isolated in the startup suite below._\n' "$DURATION" >>"$OUTPUT"
  else
    echo "skipping overhead suite: hyperfine not on PATH" >&2
  fi
fi

# --- throughput --------------------------------------------------------------

if has_suite throughput; then
  echo "suite: throughput" >&2
  tputs_rows=""
  res_rows=""
  for i in "${!scenario_names[@]}"; do
    name="${scenario_names[$i]}"
    echo "  $name" >&2
    csv_prefix="$WORKDIR/loc-tput"
    kind="text"
    [[ "${scenario_builders[$i]}" == "cmd_locust_native" ]] && kind="locust-csv"
    measure "$name" \
      "$(build_cmd "${scenario_builders[$i]}" "$VUS" "$DURATION" "$csv_prefix")" \
      "$kind" "$([[ "$kind" == locust-csv ]] && echo "${csv_prefix}_stats.csv")"
    json_row throughput.json "$name"
    tputs_rows="$tputs_rows| $name | $requests | $rps | $avg_ms | $p50_ms | $p95_ms | $p99_ms | $err_pct% |
"
    res_rows="$res_rows| $name | $T_WALL | $T_USER | $T_SYS | $(cpu_per_req "$requests") µs | $T_RSS | $T_IO |
"
  done

  section "Throughput & latency (${VUS} VUs, ${DURATION})"
  {
    echo "| Scenario | Requests | RPS | avg ms | p50 ms | p95 ms | p99 ms | Err |"
    echo "|---|---:|---:|---:|---:|---:|---:|---:|"
    printf '%s' "$tputs_rows"
    echo
    echo "_Same fixed duration for every scenario — compare RPS, not wall time._"
  } >>"$OUTPUT"

  section "Resources (same runs as throughput)"
  {
    echo "| Scenario | Wall | User | Sys | CPU per req | Peak RSS | IO ops |"
    echo "|---|---|---|---|---:|---|---|"
    printf '%s' "$res_rows"
    echo
    echo "_IO ops \`N in / M out\`: filesystem read (\`in\`) / write (\`out\`) operation counts"
    echo "from \`/usr/bin/time\` — GNU fs-block inputs/outputs on Linux, BSD block"
    echo "input/output operations on macOS. \`0 in\` usually means a warm page cache."
    echo "Units differ by OS; compare within this report only._"
  } >>"$OUTPUT"
fi

# --- startup -----------------------------------------------------------------

if has_suite startup; then
  if [[ "$HAS_HYPERFINE" == 1 ]]; then
    echo "suite: startup" >&2
    run_hyperfine "$VUS" "$STARTUP_DURATION" "$STARTUP_RUNS" \
      "$WORKDIR/startup.md" "$WORKDIR/startup-hf.json"
    section "Startup overhead (${STARTUP_DURATION} runs)"
    {
      echo "| Scenario | Mean [s] | Overhead vs native | Overhead vs ideal |"
      echo "|---|---:|---:|---:|"
      $METRICS startup "$WORKDIR/startup-hf.json" "$RESULTS_D/startup.json" "$STARTUP_DURATION"
      echo
      echo "_At a ${STARTUP_DURATION} test duration the wrapper's startup cost is a visible"
      echo "fraction of wall time. 'vs native' subtracts the bare engine; 'vs ideal'"
      echo "subtracts the test duration itself (startup + teardown of the whole stack)._"
    } >>"$OUTPUT"
  else
    echo "skipping startup suite: hyperfine not on PATH" >&2
  fi
fi

# --- scaling -----------------------------------------------------------------

if has_suite scaling; then
  echo "suite: scaling" >&2
  section "VU scaling (${SCALING_DURATION} per point)"
  {
    echo "| Engine | VUs | Requests | RPS | p95 ms | Err | CPU (u+s) | Peak RSS |"
    echo "|---|---:|---:|---:|---:|---:|---:|---|"
  } >>"$OUTPUT"
  for engine in k6 locust yaml yaml-quiet; do
    [[ "$engine" == k6 && "$HAS_K6" != 1 ]] && continue
    [[ "$engine" == locust && "$HAS_LOCUST" != 1 ]] && continue
    [[ "$engine" == yaml-quiet && "$HAS_QUIET" != 1 ]] && continue
    for v in $SCALING_VUS; do
      echo "  $engine @ $v VUs" >&2
      case "$engine" in
        k6) measure "k6@$v" "$(cmd_k6_native "$v" "$SCALING_DURATION" "$TARGET" 0)" ;;
        locust)
          measure "locust@$v" \
            "$(cmd_locust_native "$v" "$SCALING_DURATION" "$TARGET" 0 "$WORKDIR/loc-scale")" \
            locust-csv "$WORKDIR/loc-scale_stats.csv" ;;
        yaml) measure "yaml@$v" "$(cmd_yaml "$v" "$SCALING_DURATION" "$WORKDIR/yaml-get.yaml")" ;;
        yaml-quiet) measure "yaml-quiet@$v" "$(cmd_yaml "$v" "$SCALING_DURATION" "$WORKDIR/yaml-get.yaml" --quiet)" ;;
      esac
      json_row scaling.json "$engine@$v"
      cpu_total=$(awk "BEGIN{printf \"%.1fs\", ${T_USER_S:-0}+${T_SYS_S:-0}}")
      echo "| $engine | $v | $requests | $rps | $p95_ms | $err_pct% | $cpu_total | $T_RSS |" >>"$OUTPUT"
    done
  done
fi

# --- saturation ---------------------------------------------------------------

if has_suite saturation; then
  echo "suite: saturation" >&2
  section "Saturation (max RPS at ${SAT_VUS} VUs, ${SAT_DURATION})"
  {
    echo "| Engine | Requests | RPS | p95 ms | p99 ms | Err | CPU (u+s) |"
    echo "|---|---:|---:|---:|---:|---:|---:|"
  } >>"$OUTPUT"
  for engine in k6 locust yaml yaml-quiet; do
    [[ "$engine" == k6 && "$HAS_K6" != 1 ]] && continue
    [[ "$engine" == locust && "$HAS_LOCUST" != 1 ]] && continue
    [[ "$engine" == yaml-quiet && "$HAS_QUIET" != 1 ]] && continue
    echo "  $engine" >&2
    case "$engine" in
      k6) measure "k6-sat" "$(cmd_k6_native "$SAT_VUS" "$SAT_DURATION" "$TARGET" 0)" ;;
      locust)
        measure "locust-sat" \
          "$(cmd_locust_native "$SAT_VUS" "$SAT_DURATION" "$TARGET" 0 "$WORKDIR/loc-sat")" \
          locust-csv "$WORKDIR/loc-sat_stats.csv" ;;
      yaml) measure "yaml-sat" "$(cmd_yaml "$SAT_VUS" "$SAT_DURATION" "$WORKDIR/yaml-get.yaml")" ;;
      yaml-quiet) measure "yaml-quiet-sat" "$(cmd_yaml "$SAT_VUS" "$SAT_DURATION" "$WORKDIR/yaml-get.yaml" --quiet)" ;;
    esac
    json_row saturation.json "$engine"
    cpu_total=$(awk "BEGIN{printf \"%.1fs\", ${T_USER_S:-0}+${T_SYS_S:-0}}")
    echo "| $engine | $requests | $rps | $p95_ms | $p99_ms | $err_pct% | $cpu_total |" >>"$OUTPUT"
  done
  {
    echo
    echo "_Load generator and \`perfscale serve\` share this machine's CPU, so these"
    echo "ceilings include the target's cost. If two engines plateau at a similar"
    echo "RPS, the serve target (or the CPU) is likely the bottleneck, not the engine._"
  } >>"$OUTPUT"
fi

# --- yaml ---------------------------------------------------------------------

if has_suite yaml; then
  echo "suite: yaml" >&2
  section "Native YAML engine scenarios (${VUS} VUs, ${YAML_DURATION})"
  {
    echo "| Scenario | Requests | RPS | p95 ms | Err | CPU per req | Peak RSS |"
    echo "|---|---:|---:|---:|---:|---:|---|"
  } >>"$OUTPUT"
  yaml_scenarios="get get-quiet check post multi"
  for sc in $yaml_scenarios; do
    file="$WORKDIR/yaml-$sc.yaml"
    flags=""
    if [[ "$sc" == "get-quiet" ]]; then
      [[ "$HAS_QUIET" == 1 ]] || continue
      file="$WORKDIR/yaml-get.yaml"
      flags="--quiet"
    fi
    echo "  $sc" >&2
    measure "yaml-$sc" "$(cmd_yaml "$VUS" "$YAML_DURATION" "$file" "$flags")"
    json_row yaml.json "$sc"
    echo "| $sc | $requests | $rps | $p95_ms | $err_pct% | $(cpu_per_req "$requests") µs | $T_RSS |" >>"$OUTPUT"
  done
  {
    echo
    echo "_get = single GET; get-quiet = the same GET with \`--quiet\` (per-request"
    echo "logging suppressed — the delta against \`get\` is the logging cost);"
    echo "check = GET + inline \`status: 200\` check; post = POST"
    echo "with a JSON body (the serve target logs each metrics batch, so this row"
    echo "includes some target-side cost); multi = two steps with \`outputs\` +"
    echo "\`\${{ ... }}\` interpolation + check. Deltas against \`get\` price each"
    echo "feature of the step engine._"
  } >>"$OUTPUT"
fi

# --- tls ----------------------------------------------------------------------

if has_suite tls && [[ "$HAS_TLS" == 1 ]]; then
  echo "suite: tls" >&2
  section "TLS (HTTPS via \`serve --tls\`, ${VUS} VUs, ${TLS_DURATION})"
  {
    echo "| Scenario | Requests | RPS | p95 ms | Err | CPU (u+s) | Peak RSS |"
    echo "|---|---:|---:|---:|---:|---:|---|"
  } >>"$OUTPUT"
  if [[ "$HAS_K6" == 1 ]]; then
    measure "k6-tls" "$(cmd_k6_native "$VUS" "$TLS_DURATION" "$TLS_TARGET" 1)"
    json_row tls.json "k6"
    cpu_total=$(awk "BEGIN{printf \"%.1fs\", ${T_USER_S:-0}+${T_SYS_S:-0}}")
    echo "| k6 (native) | $requests | $rps | $p95_ms | $err_pct% | $cpu_total | $T_RSS |" >>"$OUTPUT"
  fi
  if [[ "$HAS_LOCUST" == 1 ]]; then
    measure "locust-tls" \
      "$(cmd_locust_native "$VUS" "$TLS_DURATION" "$TLS_TARGET" 1 "$WORKDIR/loc-tls")" \
      locust-csv "$WORKDIR/loc-tls_stats.csv"
    json_row tls.json "locust"
    cpu_total=$(awk "BEGIN{printf \"%.1fs\", ${T_USER_S:-0}+${T_SYS_S:-0}}")
    echo "| locust (native) | $requests | $rps | $p95_ms | $err_pct% | $cpu_total | $T_RSS |" >>"$OUTPUT"
  fi
  measure "yaml-tls" "$(cmd_yaml "$VUS" "$TLS_DURATION" "$WORKDIR/yaml-tls.yaml")"
  json_row tls.json "yaml"
  cpu_total=$(awk "BEGIN{printf \"%.1fs\", ${T_USER_S:-0}+${T_SYS_S:-0}}")
  echo "| perfscale (yaml) | $requests | $rps | $p95_ms | $err_pct% | $cpu_total | $T_RSS |" >>"$OUTPUT"
  {
    echo
    echo "_Self-signed certificate; all clients skip verification (k6"
    echo "\`insecureSkipTLSVerify\`, locust \`verify=False\`, native \`insecure: true\`)."
    echo "Compare against the plain-HTTP throughput table for the TLS tax._"
  } >>"$OUTPUT"
fi

# ---------------------------------------------------------------------------

$METRICS merge "$RESULTS_D" "$RESULTS"
echo "report written to $OUTPUT"
echo "results written to $RESULTS"
