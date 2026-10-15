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
#   ws          – WebSocket echo: same-connection round-trips against the
#                 serve target's /ws endpoint — messages/sec and RTT (k6 +
#                 native engine; locust has no built-in WS support).
#   boundary    – ladder of VU ceilings (BOUNDARY_LADDER, default
#                 "5000 15000 30000"): every engine ramps 0→ceiling over
#                 BOUNDARY_DURATION at each rung until its cumulative error
#                 rate crosses BOUNDARY_ERR_PCT percent (default 1%) or the
#                 process dies (OOM counts — recorded as the boundary). An
#                 engine that crossed stops climbing; the rest continue.
#                 BOUNDARY_MAX_VUS, when set, replaces the ladder with a
#                 single rung (backwards-compatible override).
#   tls         – engines against `perfscale serve --tls` (self-signed HTTPS).
#   library     – native engine with a WASM value-generator library
#                 (`${hello.greet(...)}` from the SDK hello component):
#                 hyperfine at 1s shows per-run compile (cold cache) vs the
#                 burn cache (perfscale install), plus instrumented runs for
#                 per-call cost against a no-library and a `@std/random`
#                 builtin baseline.
#   grpc        – native `std/grpc@v1` unary calls against the repo's
#                 grpc_echo_server example (reflection on, plaintext).
#   graphql     – native `std/graphql@v1` query against the repo's
#                 graphql_server example (introspection on).
#   db          – SQL suite: native `std/db-*@v1` scenarios against PostgreSQL
#                 at BENCH_PG_DSN (cold connect+query per iteration, warm
#                 persistent pool, transactions) plus an in-process SQLite
#                 baseline, and the same warm SELECT 1 shape from locust
#                 (psycopg2), JMeter (JDBC) and k6 — the k6 row needs a
#                 custom binary with the xk6-sql extension (K6_SQL_BIN;
#                 built by the bench workflow), stock k6 has no SQL module.
#
# No llm/pubsub suites: the engine's LLM and pubsub (nats) drivers need
# backing services that don't fit a bench CI job cleanly (an LLM endpoint
# with meaningful semantics, a NATS/Redis broker); the pubsub `memory`
# driver measures no real transport. Add them when a self-contained target
# exists.
#
# JMeter joins only the hyperfine suites (overhead/startup): its non-GUI
# console summary has no percentiles, so it is not part of the throughput
# table.
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
SUITES="${SUITES:-overhead throughput startup scaling saturation yaml ws boundary tls library grpc graphql db}"

STARTUP_DURATION="${STARTUP_DURATION:-1s}"
STARTUP_RUNS="${STARTUP_RUNS:-5}"
SCALING_VUS="${SCALING_VUS:-10 50 200}"
SCALING_DURATION="${SCALING_DURATION:-10s}"
SAT_VUS="${SAT_VUS:-256}"
SAT_DURATION="${SAT_DURATION:-15s}"
YAML_DURATION="${YAML_DURATION:-10s}"
TLS_DURATION="${TLS_DURATION:-10s}"
WS_DURATION="${WS_DURATION:-10s}"
WS_ROUNDS="${WS_ROUNDS:-10}"
BOUNDARY_DURATION="${BOUNDARY_DURATION:-30s}"
# Ladder of ramp ceilings for the boundary suite; an explicit
# BOUNDARY_MAX_VUS replaces the ladder with a single rung (legacy override).
BOUNDARY_LADDER="${BOUNDARY_MAX_VUS:-${BOUNDARY_LADDER:-5000 15000 30000}}"
BOUNDARY_ERR_PCT="${BOUNDARY_ERR_PCT:-1}"
# JMeter heap for the high-VU boundary rungs (the default 1g OOMs well below
# 30k threads; ubuntu-latest runners have 16 GB).
BOUNDARY_JMETER_HEAP="${BOUNDARY_JMETER_HEAP:--Xms512m -Xmx4g}"
LIB_DURATION="${LIB_DURATION:-10s}"
LIB_STARTUP_DURATION="${LIB_STARTUP_DURATION:-1s}"
LIB_RUNS="${LIB_RUNS:-5}"
GRPC_DURATION="${GRPC_DURATION:-10s}"
GQL_DURATION="${GQL_DURATION:-10s}"
DB_DURATION="${DB_DURATION:-10s}"
# Warm db scenarios: queries per iteration after one persistent connect, and
# tx cycles (begin+query+commit) per iteration.
DB_WARM_QUERIES="${DB_WARM_QUERIES:-10}"
DB_TX_CYCLES="${DB_TX_CYCLES:-5}"
BENCH_PG_DSN="${BENCH_PG_DSN:-postgres://postgres:perfscale@127.0.0.1:5432/postgres}"
# Custom k6 binary with the xk6-sql extension + postgres driver, used ONLY by
# the db suite's `k6 (xk6-sql)` row — every other k6 scenario keeps using the
# stock binary. Built by the bench workflow (see bench.yml); local runs skip
# the row when the binary is absent. Resolved after ROOT below.
K6_SQL_BIN="${K6_SQL_BIN:-}"

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
GRPC_SERVE_PID=""
GQL_SERVE_PID=""
cleanup() {
  [[ -n "$SERVE_PID" ]] && kill "$SERVE_PID" 2>/dev/null || true
  [[ -n "$TLS_SERVE_PID" ]] && kill "$TLS_SERVE_PID" 2>/dev/null || true
  [[ -n "$GRPC_SERVE_PID" ]] && kill "$GRPC_SERVE_PID" 2>/dev/null || true
  [[ -n "$GQL_SERVE_PID" ]] && kill "$GQL_SERVE_PID" 2>/dev/null || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

TARGET="http://127.0.0.1:${PORT}"
TLS_TARGET="https://127.0.0.1:${TLS_PORT}"

has_suite() { case " $SUITES " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
HAS_K6=0
HAS_LOCUST=0
HAS_JMETER=0
HAS_HYPERFINE=0
command -v k6 >/dev/null 2>&1 && HAS_K6=1
command -v locust >/dev/null 2>&1 && HAS_LOCUST=1
command -v jmeter >/dev/null 2>&1 && HAS_JMETER=1
command -v hyperfine >/dev/null 2>&1 && HAS_HYPERFINE=1
[[ "$HAS_K6" == 1 ]] || echo "skipping k6 scenarios: k6 not on PATH" >&2
[[ "$HAS_LOCUST" == 1 ]] || echo "skipping locust scenarios: locust not on PATH" >&2
[[ "$HAS_JMETER" == 1 ]] || echo "skipping jmeter scenarios: jmeter not on PATH" >&2

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

HAS_WS=0
if has_suite ws; then
  if "$BIN" serve --help 2>/dev/null | grep -q -- '/ws'; then
    HAS_WS=1
  else
    echo "skipping ws suite: this perfscale binary's serve has no /ws endpoint" >&2
  fi
fi

# TCP port probe for the non-HTTP fixture servers (gRPC/GraphQL examples,
# PostgreSQL).
wait_for_port() { # $1 host, $2 port
  for _ in $(seq 1 50); do
    (echo >/dev/tcp/"$1"/"$2") >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

# library suite fixture: the SDK hello example compiled to a wasm32-wasip2
# component. Built on demand into the shared fixtures target dir (cached in
# CI by the rust cache); skipped when the target toolchain is unavailable.
LIB_WASM="$ROOT/target/wasm-libs-fixtures/wasm32-wasip2/release/perfscale_hello_library.wasm"
HAS_LIB=0
if has_suite library; then
  if [[ -f "$LIB_WASM" ]]; then
    HAS_LIB=1
  elif command -v rustup >/dev/null 2>&1 \
    && rustup target list --installed 2>/dev/null | grep -qx wasm32-wasip2; then
    echo "building the hello library fixture (wasm32-wasip2)..." >&2
    if cargo build --release --target wasm32-wasip2 \
      --manifest-path "$ROOT/crates/perfscale-library-sdk/examples/hello/Cargo.toml" \
      --target-dir "$ROOT/target/wasm-libs-fixtures" >&2; then
      HAS_LIB=1
    fi
  fi
  [[ "$HAS_LIB" == 1 ]] || echo "skipping library suite: hello fixture unavailable (rustup target add wasm32-wasip2)" >&2
fi

# grpc/graphql suite targets: the repo's example servers, built on demand.
# Both are dev tools under crates/perfscale-core/examples.
example_bin() { # $1 example name → path (building if missing)
  local path="$ROOT/target/release/examples/$1"
  if [[ ! -x "$path" ]]; then
    echo "building the $1 example (release)..." >&2
    cargo build --release -p perfscale-core --example "$1" \
      --manifest-path "$ROOT/Cargo.toml" >&2 || return 1
  fi
  echo "$path"
}

GRPC_ADDR="127.0.0.1:50051"
HAS_GRPC=0
if has_suite grpc; then
  if grpc_bin=$(example_bin grpc_echo_server); then
    "$grpc_bin" "$GRPC_ADDR" >"$WORKDIR/grpc-serve.log" 2>&1 &
    GRPC_SERVE_PID=$!
    if wait_for_port 127.0.0.1 "${GRPC_ADDR##*:}"; then
      HAS_GRPC=1
    else
      echo "skipping grpc suite: grpc_echo_server never came up" >&2
    fi
  else
    echo "skipping grpc suite: failed to build grpc_echo_server" >&2
  fi
fi

GQL_ADDR="127.0.0.1:4000"
HAS_GQL=0
if has_suite graphql; then
  if gql_bin=$(example_bin graphql_server); then
    "$gql_bin" >"$WORKDIR/gql-serve.log" 2>&1 &
    GQL_SERVE_PID=$!
    if wait_for_port 127.0.0.1 "${GQL_ADDR##*:}"; then
      HAS_GQL=1
    else
      echo "skipping graphql suite: graphql_server never came up" >&2
    fi
  else
    echo "skipping graphql suite: failed to build graphql_server" >&2
  fi
fi

# db suite target: a PostgreSQL the caller provides (CI service container;
# BENCH_PG_DSN). Probed by host:port from the DSN — no engine changes.
# The DSN parts are also parsed out for the competitor scenarios (locust's
# psycopg2 connect string, JMeter's JDBC URL/credentials).
DB_HOSTPORT="${BENCH_PG_DSN##*@}"   # strip userinfo
DB_HOSTPORT="${DB_HOSTPORT%%/*}"    # strip /database
DB_USERINFO="${BENCH_PG_DSN#*://}"; DB_USERINFO="${DB_USERINFO%%@*}"
DB_USER="${DB_USERINFO%%:*}"; DB_PASS="${DB_USERINFO#*:}"
DB_NAME="${BENCH_PG_DSN##*/}"
HAS_DB=0
HAS_LOCUST_DB=0
HAS_JMETER_DB=0
if has_suite db; then
  if wait_for_port "${DB_HOSTPORT%%:*}" "${DB_HOSTPORT##*:}"; then
    HAS_DB=1
  else
    echo "skipping db suite: no PostgreSQL at $DB_HOSTPORT (BENCH_PG_DSN)" >&2
  fi
fi

if [[ "$HAS_DB" == 1 && "$HAS_LOCUST" == 1 ]]; then
  # psycopg2 must be importable from locust's own interpreter (pipx inject
  # locust psycopg2-binary) — probe the shebang interpreter, then python3 as
  # a fallback for pip-installed locust.
  locust_bin="$(command -v locust)"
  locust_py="$(head -1 "$locust_bin" 2>/dev/null | sed -n 's/^#!//p' | awk '{print $1}')"
  if [[ -n "$locust_py" && -x "$locust_py" ]] \
    && "$locust_py" -c 'import psycopg2' >/dev/null 2>&1; then
    HAS_LOCUST_DB=1
  elif python3 -c 'import psycopg2' >/dev/null 2>&1; then
    HAS_LOCUST_DB=1
  else
    echo "skipping locust db scenario: psycopg2 not in locust's env (pipx inject locust psycopg2-binary)" >&2
  fi
fi

if [[ "$HAS_DB" == 1 && "$HAS_JMETER" == 1 ]]; then
  # JMeter's JDBC sampler needs the PostgreSQL JDBC driver on its lib path.
  PG_JDBC_JAR="${BENCH_PG_JDBC_JAR:-}"
  if [[ -z "$PG_JDBC_JAR" ]]; then
    jmeter_home="$(cd "$(dirname "$(command -v jmeter)")/.." 2>/dev/null && pwd)"
    PG_JDBC_JAR="$(ls "$jmeter_home"/lib/postgresql-*.jar 2>/dev/null | head -1 || true)"
  fi
  if [[ -n "$PG_JDBC_JAR" && -f "$PG_JDBC_JAR" ]]; then
    HAS_JMETER_DB=1
  else
    echo "skipping jmeter db scenario: no postgresql JDBC jar in JMeter's lib/ (BENCH_PG_JDBC_JAR)" >&2
  fi
fi

# The k6 db row needs a custom binary with the xk6-sql extension (stock k6
# has no SQL module). Default path matches what the bench workflow builds;
# K6_SQL_BIN overrides it. NOTE: this binary is deliberately NOT on the k6
# scenarios above — the stock k6 keeps serving every other suite, and the
# custom build's k6 core version may differ from the apt-installed one.
HAS_K6_SQL=0
K6_SQL_BIN="${K6_SQL_BIN:-$ROOT/target/xk6-sql/k6}"
if [[ "$HAS_DB" == 1 ]]; then
  if [[ -x "$K6_SQL_BIN" ]]; then
    HAS_K6_SQL=1
  else
    echo "skipping k6 (xk6-sql) db scenario: no custom binary at $K6_SQL_BIN (built by the bench workflow)" >&2
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

# WebSocket echo scenario (ws suite): WS_ROUNDS same-connection round-trips
# per session against the serve target's /ws endpoint. Both engines report
# the same two metrics — ws_msgs_sent (counter) and ws_msg_rtt (trend) — so
# one parser (bench_metrics.py ws-text) fits every scenario.
{
  echo "steps:"
  echo "  - name: echo session"
  echo "    use: std/ws@v1"
  echo "    with:"
  echo "      url: \"ws://127.0.0.1:${PORT}/ws\""
  echo "      messages:"
  for i in $(seq 1 "$WS_ROUNDS"); do
    printf '        - send: m-%s\n          until_contains: m-%s\n' "$i" "$i"
  done
} >"$WORKDIR/ws.yaml"

cat >"$WORKDIR/ws.js" <<'EOF'
import ws from 'k6/ws';
import { Trend } from 'k6/metrics';

// Same shape as the native ws scenario: BENCH_WS_ROUNDS echo round-trips on
// one connection. Message counts come from k6's built-in ws_msgs_sent
// counter (same name/shape as the native engine's); the RTT is a custom
// Trend in the same ws_msg_rtt shape.
const ROUNDS = Number(__ENV.BENCH_WS_ROUNDS || 10);
const msgRtt = new Trend('ws_msg_rtt', true);

export const options = {
  vus: Number(__ENV.BENCH_VUS || 1),
  duration: __ENV.BENCH_DURATION || '10s',
  summaryTrendStats: ['avg', 'min', 'med', 'max', 'p(50)', 'p(90)', 'p(95)', 'p(99)'],
};

export default function () {
  ws.connect(__ENV.BENCH_TARGET, {}, function (socket) {
    let seq = 0;
    let received = 0;
    let sentAt = 0;
    const pump = function () {
      sentAt = Date.now();
      socket.send('m-' + ++seq);
    };
    socket.on('open', pump);
    socket.on('message', function () {
      msgRtt.add(Date.now() - sentAt);
      if (++received >= ROUNDS) {
        socket.close();
        return;
      }
      pump();
    });
    socket.on('error', function () {
      socket.close();
    });
    // Safety net: never let a stalled server pin the VU past the duration.
    socket.setTimeout(function () {
      socket.close();
    }, 10000);
  });
}
EOF

# JMeter bench plan (overhead/startup suites): one thread group hammering
# GET /health. The (vus, duration) pair is baked into a per-shape .jmx by
# jmeter_plan() so the wrapped run (`perfscale run --jmeter`) needs no flags —
# the CLI passes no -J properties.
cat >"$WORKDIR/plan-template.jmx" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<jmeterTestPlan version="1.2" properties="5.0" jmeter="5.6.3">
  <hashTree>
    <TestPlan guiclass="TestPlanGui" testclass="TestPlan" testname="bench" enabled="true">
      <elementProp name="TestPlan.user_defined_variables" elementType="Arguments" guiclass="ArgumentsPanel" testclass="Arguments" testname="User Defined Variables" enabled="true">
        <collectionProp name="Arguments.arguments"/>
      </elementProp>
    </TestPlan>
    <hashTree>
      <ThreadGroup guiclass="ThreadGroupGui" testclass="ThreadGroup" testname="bench" enabled="true">
        <stringProp name="ThreadGroup.num_threads">@VUS@</stringProp>
        <stringProp name="ThreadGroup.ramp_time">1</stringProp>
        <boolProp name="ThreadGroup.scheduler">true</boolProp>
        <stringProp name="ThreadGroup.duration">@SECS@</stringProp>
        <elementProp name="ThreadGroup.main_controller" elementType="LoopController" guiclass="LoopControlPanel" testclass="LoopController" testname="Loop Controller" enabled="true">
          <boolProp name="LoopController.continue_forever">true</boolProp>
          <stringProp name="LoopController.loops">-1</stringProp>
        </elementProp>
      </ThreadGroup>
      <hashTree>
        <HTTPSamplerProxy guiclass="HttpTestSampleGui" testclass="HTTPSamplerProxy" testname="health" enabled="true">
          <stringProp name="HTTPSampler.domain">127.0.0.1</stringProp>
          <stringProp name="HTTPSampler.port">@PORT@</stringProp>
          <stringProp name="HTTPSampler.path">/health</stringProp>
          <stringProp name="HTTPSampler.method">GET</stringProp>
          <boolProp name="HTTPSampler.use_keepalive">true</boolProp>
        </HTTPSamplerProxy>
        <hashTree/>
      </hashTree>
    </hashTree>
  </hashTree>
</jmeterTestPlan>
EOF

# Materialize the JMeter plan for a (vus, duration) pair from the template.
# Prints the path. Durations are seconds-only for JMeter ("15s" → 15).
jmeter_plan() {
  local path="$WORKDIR/plan-$1-$2.jmx"
  if [[ ! -f "$path" ]]; then
    sed -e "s/@VUS@/$1/" -e "s/@SECS@/${2%s}/" -e "s/@PORT@/$PORT/" \
      "$WORKDIR/plan-template.jmx" >"$path"
  fi
  echo "$path"
}

# Load config for the native engine / wrapped locust, one file per (vus,
# duration) pair. Prints the path.
cfg() {
  local path="$WORKDIR/config-$1-$2.yaml"
  [[ -f "$path" ]] || printf 'vus: %s\nduration: %s\n' "$1" "$2" >"$path"
  echo "$path"
}

# Command builders: $1 vus, $2 duration, $3 target base URL, $4 insecure(0/1)
# Env vars go through `env`: hyperfine ≥1.22 refuses to run a command string
# that starts with a variable assignment.
cmd_k6_native() {
  echo "env BENCH_VUS=$1 BENCH_DURATION=$2 BENCH_TARGET=$3/health BENCH_INSECURE=$4 k6 run --quiet $WORKDIR/script.js"
}
cmd_k6_wrapped() {
  echo "env BENCH_VUS=$1 BENCH_DURATION=$2 BENCH_TARGET=$3/health BENCH_INSECURE=$4 $BIN run --k6 $WORKDIR/script.js"
}
cmd_locust_native() { # $5 csv prefix
  echo "env BENCH_INSECURE=$4 locust -f $WORKDIR/locustfile.py --headless -u $1 -r $1 -t $2 --host $3 --only-summary --csv $5"
}
cmd_locust_wrapped() {
  echo "env BENCH_INSECURE=$4 $BIN run --locust $WORKDIR/locustfile.py --host $3 -c $(cfg "$1" "$2")"
}
cmd_yaml() { # $1 vus, $2 duration, $3 test file, $4 extra flags (optional)
  echo "$BIN run -f $3 -c $(cfg "$1" "$2")${4:+ $4}"
}
# JMeter runs from $WORKDIR so its jmeter.log doesn't land in the repo root.
cmd_jmeter_native() { echo "cd $WORKDIR && jmeter -n -t $(jmeter_plan "$1" "$2")"; }
cmd_jmeter_wrapped() { echo "cd $WORKDIR && $BIN run --jmeter $(jmeter_plan "$1" "$2")"; }

# Error-boundary scenario (boundary suite): every engine ramps 0→ the current
# ladder rung over BOUNDARY_DURATION against GET /health while its periodic
# output is watched for the cumulative error rate crossing BOUNDARY_ERR_PCT
# percent. Detection differs per engine — k6 aborts itself via a threshold,
# the others are parsed from their periodic lines.
cat >"$WORKDIR/boundary.js" <<'EOF'
import http from 'k6/http';

// The threshold aborts the run the moment the cumulative error rate reaches
// BENCH_ERR_PCT percent — vus_max in the summary is then the boundary load.
const ERR_PCT = Number(__ENV.BENCH_ERR_PCT || 1);

export const options = {
  scenarios: {
    ramp: {
      executor: 'ramping-vus',
      startVUs: 0,
      stages: [{ duration: __ENV.BENCH_DURATION || '30s', target: Number(__ENV.BENCH_MAX_VUS || 5000) }],
      gracefulRampDown: '0s',
    },
  },
  thresholds: {
    // abortOnFail fires when the threshold FAILS, so the expression must be
    // the healthy direction: rate < err% holds until errors reach the limit.
    http_req_failed: [{ threshold: `rate<${ERR_PCT / 100}`, abortOnFail: true }],
  },
};

export default function () {
  http.get(__ENV.BENCH_TARGET);
}
EOF

cat >"$WORKDIR/boundary.yaml" <<EOF
steps:
  - name: health check
    use: std/http@v1
    with:
      method: GET
      url: "${TARGET}/health"
EOF

# Staged ramp config for the native engine, one file per ladder rung
# (generated lazily by boundary_native_cfg below).

cat >"$WORKDIR/boundary_locust.py" <<'EOF'
import os

from locust import HttpUser, LoadTestShape, task

MAX_VUS = int(os.environ.get("BENCH_MAX_VUS", "5000"))
DURATION_S = int(os.environ.get("BENCH_DURATION_S", "30"))


class BoundaryUser(HttpUser):
    @task
    def health(self):
        self.client.get("/health")


class RampToBoundary(LoadTestShape):
    """Linear 0→MAX_VUS over DURATION_S. Run with --csv-full-history: the
    Aggregated history rows are what the boundary parser reads."""

    def tick(self):
        run_time = self.get_run_time()
        if run_time >= DURATION_S + 5:
            return None
        vus = max(1, min(MAX_VUS, int(MAX_VUS * run_time / DURATION_S)))
        # Spawn rate scales with the rung: finish hatching by ramp end
        # (MAX_VUS // DURATION_S) instead of a fixed fraction, so high rungs
        # actually reach their target VU count.
        return (vus, max(10, MAX_VUS // DURATION_S))
EOF

# Boundary JMeter plan: threads ramp over the whole BENCH duration and the
# scheduler holds for a tail so the summariser reports the full ramp.
cat >"$WORKDIR/plan-boundary-template.jmx" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<jmeterTestPlan version="1.2" properties="5.0" jmeter="5.6.3">
  <hashTree>
    <TestPlan guiclass="TestPlanGui" testclass="TestPlan" testname="boundary" enabled="true">
      <elementProp name="TestPlan.user_defined_variables" elementType="Arguments" guiclass="ArgumentsPanel" testclass="Arguments" testname="User Defined Variables" enabled="true">
        <collectionProp name="Arguments.arguments"/>
      </elementProp>
    </TestPlan>
    <hashTree>
      <ThreadGroup guiclass="ThreadGroupGui" testclass="ThreadGroup" testname="boundary" enabled="true">
        <stringProp name="ThreadGroup.num_threads">@VUS@</stringProp>
        <stringProp name="ThreadGroup.ramp_time">@RAMP@</stringProp>
        <boolProp name="ThreadGroup.scheduler">true</boolProp>
        <stringProp name="ThreadGroup.duration">@SECS@</stringProp>
        <elementProp name="ThreadGroup.main_controller" elementType="LoopController" guiclass="LoopControlPanel" testclass="LoopController" testname="Loop Controller" enabled="true">
          <boolProp name="LoopController.continue_forever">true</boolProp>
          <stringProp name="LoopController.loops">-1</stringProp>
        </elementProp>
      </ThreadGroup>
      <hashTree>
        <HTTPSamplerProxy guiclass="HttpTestSampleGui" testclass="HTTPSamplerProxy" testname="health" enabled="true">
          <stringProp name="HTTPSampler.domain">127.0.0.1</stringProp>
          <stringProp name="HTTPSampler.port">@PORT@</stringProp>
          <stringProp name="HTTPSampler.path">/health</stringProp>
          <stringProp name="HTTPSampler.method">GET</stringProp>
          <boolProp name="HTTPSampler.use_keepalive">true</boolProp>
        </HTTPSamplerProxy>
        <hashTree/>
      </hashTree>
    </hashTree>
  </hashTree>
</jmeterTestPlan>
EOF

# library suite: the yaml-get scenario with one header value generated by a
# library call per request — WASM component (`${hello.greet(world)}`) vs the
# native builtin (`${random.ulid()}`), so the WASM-runtime overhead and the
# library-call overhead separate. The `libraries:` block lives in the config
# (lib_cfg); cold/burned differ only by PERFSCALE_CACHE_DIR.
if [[ "$HAS_LIB" == 1 ]]; then
  cat >"$WORKDIR/yaml-lib.yaml" <<EOF
steps:
  - name: health check with a library greeting
    use: std/http@v1
    with:
      method: GET
      url: "${TARGET}/health"
      headers:
        x-greeting: "\${hello.greet(world)}"
EOF
  cat >"$WORKDIR/yaml-lib-builtin.yaml" <<EOF
steps:
  - name: health check with a builtin id
    use: std/http@v1
    with:
      method: GET
      url: "${TARGET}/health"
      headers:
        x-id: "\${random.ulid()}"
EOF
  cat >"$WORKDIR/lib-hello.yaml" <<EOF
libraries:
  - use: $LIB_WASM
    as: hello
    capabilities: []
EOF
  cat >"$WORKDIR/lib-builtin.yaml" <<'EOF'
libraries:
  - use: '@std/random@v1'
    capabilities: []
EOF
fi

# Load config with a libraries: block: vus/duration from the arguments plus
# the contents of the fragment file. Prints the path.
lib_cfg() { # $1 vus, $2 duration, $3 libraries fragment
  local path="$WORKDIR/libcfg-$1-$2-$(basename "$3" .yaml).yaml"
  if [[ ! -f "$path" ]]; then
    { printf 'vus: %s\nduration: %s\n' "$1" "$2"; cat "$3"; } >"$path"
  fi
  echo "$path"
}

# grpc suite: one-shot unary call per iteration (connect → reflect → call →
# close) against the example echo server. Reflection is cached per URL, so
# the per-iteration cost is connect + call.
if [[ "$HAS_GRPC" == 1 ]]; then
  cat >"$WORKDIR/yaml-grpc.yaml" <<EOF
steps:
  - name: echo unary
    use: std/grpc@v1
    with:
      url: "grpc://${GRPC_ADDR}"
      reflection: true
      method: perfscale.test.v1.Echo/Unary
      payload:
        message: "bench-\${seq}"
EOF
fi

# graphql suite: one schema-validated query per iteration; introspection is
# cached per URL, so per-iteration cost is parse + validate + HTTP POST.
if [[ "$HAS_GQL" == 1 ]]; then
  cat >"$WORKDIR/yaml-gql.yaml" <<EOF
steps:
  - name: viewer query
    use: std/graphql@v1
    with:
      url: "http://${GQL_ADDR}/graphql"
      query: |
        query { viewer { id name } }
EOF
fi

# db suite, perfscale native scenarios:
#   db / db-params  – cold path: connect + query per iteration (the
#                     connection drops at iteration end), SELECT 1 vs a query
#                     with a ${}-expanded bind parameter.
#   db-warm         – warm path: one persistent-mode connect per iteration
#                     (a real pool, pool_size 1), then DB_WARM_QUERIES queries
#                     on the parked connection, then db-close.
#   db-warm-tx      – warm path with DB_TX_CYCLES × (tx-begin, query, commit).
#   db-sqlite       – cold SQLite: connect + SELECT 1 per iteration against
#                     :memory: (pool_size 1 — the default; a second pooled
#                     connection would be a different database).
#   db-sqlite-warm  – warm SQLite: the db-warm shape with no server at all —
#                     the pure in-process cost floor of the engine's db path.
if [[ "$HAS_DB" == 1 ]]; then
  cat >"$WORKDIR/yaml-db.yaml" <<EOF
steps:
  - name: connect
    use: std/db-connect@v1
    with:
      driver: postgres
      dsn: "$BENCH_PG_DSN"
      tls: false
    outputs: conn
  - name: select one
    use: std/db-query@v1
    with:
      id: "\${{ conn.id }}"
      query: "SELECT 1"
EOF
  cat >"$WORKDIR/yaml-db-params.yaml" <<EOF
steps:
  - name: connect
    use: std/db-connect@v1
    with:
      driver: postgres
      dsn: "$BENCH_PG_DSN"
      tls: false
    outputs: conn
  - name: select with a bind param
    use: std/db-query@v1
    with:
      id: "\${{ conn.id }}"
      query: "SELECT \$1::int AS n"
      params: [ "\${rand(1,1000)}" ]
EOF
  {
    cat <<EOF
steps:
  - name: connect (persistent pool)
    use: std/db-connect@v1
    with:
      driver: postgres
      dsn: "$BENCH_PG_DSN"
      tls: false
    outputs: conn
EOF
    for i in $(seq 1 "$DB_WARM_QUERIES"); do
      cat <<EOF
  - name: warm query $i
    use: std/db-query@v1
    with:
      id: "\${{ conn.id }}"
      query: "SELECT 1"
EOF
    done
    cat <<EOF
  - name: close
    use: std/db-close@v1
    with:
      id: "\${{ conn.id }}"
EOF
  } >"$WORKDIR/yaml-db-warm.yaml"
  {
    cat <<EOF
steps:
  - name: connect (persistent pool)
    use: std/db-connect@v1
    with:
      driver: postgres
      dsn: "$BENCH_PG_DSN"
      tls: false
    outputs: conn
EOF
    for i in $(seq 1 "$DB_TX_CYCLES"); do
      cat <<EOF
  - name: tx $i begin
    use: std/db-tx-begin@v1
    with:
      id: "\${{ conn.id }}"
  - name: tx $i query
    use: std/db-query@v1
    with:
      id: "\${{ conn.id }}"
      query: "SELECT 1"
  - name: tx $i commit
    use: std/db-tx-commit@v1
    with:
      id: "\${{ conn.id }}"
EOF
    done
    cat <<EOF
  - name: close
    use: std/db-close@v1
    with:
      id: "\${{ conn.id }}"
EOF
  } >"$WORKDIR/yaml-db-warm-tx.yaml"
fi

# SQLite scenarios need no server — they are generated whenever the db suite
# is selected, so the in-process cost floor is measurable even without a
# PostgreSQL at hand.
if has_suite db; then
  cat >"$WORKDIR/yaml-db-sqlite.yaml" <<EOF
steps:
  - name: connect
    use: std/db-connect@v1
    with:
      driver: sqlite
      dsn: "sqlite::memory:"
    outputs: conn
  - name: select one
    use: std/db-query@v1
    with:
      id: "\${{ conn.id }}"
      query: "SELECT 1"
EOF
  {
    cat <<EOF
steps:
  - name: connect (persistent pool)
    use: std/db-connect@v1
    with:
      driver: sqlite
      dsn: "sqlite::memory:"
    outputs: conn
EOF
    for i in $(seq 1 "$DB_WARM_QUERIES"); do
      cat <<EOF
  - name: warm query $i
    use: std/db-query@v1
    with:
      id: "\${{ conn.id }}"
      query: "SELECT 1"
EOF
    done
    cat <<EOF
  - name: close
    use: std/db-close@v1
    with:
      id: "\${{ conn.id }}"
EOF
  } >"$WORKDIR/yaml-db-sqlite-warm.yaml"
fi

# locust db scenario: one psycopg2 connection per simulated user (on_start),
# then SELECT 1 in a tight loop — the same warm shape as db-warm.
if [[ "$HAS_LOCUST_DB" == 1 ]]; then
  cat >"$WORKDIR/locustfile_db.py" <<'EOF'
import os
import time

import psycopg2
from locust import User, task, constant

DSN = os.environ["BENCH_PG_DSN"]


class PgUser(User):
    wait_time = constant(0)

    def on_start(self):
        self.pg = psycopg2.connect(DSN)
        self.pg.autocommit = True

    def on_stop(self):
        self.pg.close()

    @task
    def select_one(self):
        t0 = time.perf_counter()
        try:
            with self.pg.cursor() as cur:
                cur.execute("SELECT 1")
                cur.fetchone()
        except Exception as e:
            self.environment.events.request.fire(
                request_type="SQL",
                name="SELECT 1",
                response_time=(time.perf_counter() - t0) * 1000,
                response_length=0,
                exception=e,
            )
            # Reconnect once — a dropped connection must not pin the user.
            try:
                self.pg.close()
            except Exception:
                pass
            self.pg = psycopg2.connect(DSN)
            self.pg.autocommit = True
            return
        self.environment.events.request.fire(
            request_type="SQL",
            name="SELECT 1",
            response_time=(time.perf_counter() - t0) * 1000,
            response_length=0,
        )
EOF
fi

# JMeter db plan: one JDBC Connection Configuration (pool sized to the
# thread count) + one JDBC Request running SELECT 1 per loop. The DSN parts
# are baked in at materialization time.
if [[ "$HAS_JMETER_DB" == 1 ]]; then
  cat >"$WORKDIR/plan-db-template.jmx" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<jmeterTestPlan version="1.2" properties="5.0" jmeter="5.6.3">
  <hashTree>
    <TestPlan guiclass="TestPlanGui" testclass="TestPlan" testname="db" enabled="true">
      <elementProp name="TestPlan.user_defined_variables" elementType="Arguments" guiclass="ArgumentsPanel" testclass="Arguments" testname="User Defined Variables" enabled="true">
        <collectionProp name="Arguments.arguments"/>
      </elementProp>
    </TestPlan>
    <hashTree>
      <JDBCDataSource guiclass="TestBeanGUI" testclass="JDBCDataSource" testname="pg pool" enabled="true">
        <boolProp name="autocommit">true</boolProp>
        <stringProp name="checkQuery">SELECT 1</stringProp>
        <stringProp name="connectionAge">5000</stringProp>
        <stringProp name="connectionProperties"></stringProp>
        <stringProp name="dataSource">pg</stringProp>
        <stringProp name="dbUrl">jdbc:postgresql://@DBHOST@:@DBPORT@/@DBNAME@</stringProp>
        <stringProp name="driver">org.postgresql.Driver</stringProp>
        <stringProp name="initQuery"></stringProp>
        <boolProp name="keepAlive">true</boolProp>
        <stringProp name="password">@DBPASS@</stringProp>
        <stringProp name="poolMax">@VUS@</stringProp>
        <stringProp name="poolPreparedStatements"></stringProp>
        <boolProp name="preinit">false</boolProp>
        <stringProp name="timeout">10000</stringProp>
        <stringProp name="transactionIsolation">DEFAULT</stringProp>
        <!-- string, millis between idle-eviction runs; keepAlive=true parses
             it numerically — a missing/empty trimInterval kills the pool
             setup with NumberFormatException. -->
        <stringProp name="trimInterval">60000</stringProp>
        <stringProp name="username">@DBUSER@</stringProp>
      </JDBCDataSource>
      <hashTree/>
      <ThreadGroup guiclass="ThreadGroupGui" testclass="ThreadGroup" testname="db" enabled="true">
        <stringProp name="ThreadGroup.num_threads">@VUS@</stringProp>
        <stringProp name="ThreadGroup.ramp_time">1</stringProp>
        <boolProp name="ThreadGroup.scheduler">true</boolProp>
        <stringProp name="ThreadGroup.duration">@SECS@</stringProp>
        <elementProp name="ThreadGroup.main_controller" elementType="LoopController" guiclass="LoopControlPanel" testclass="LoopController" testname="Loop Controller" enabled="true">
          <boolProp name="LoopController.continue_forever">true</boolProp>
          <stringProp name="LoopController.loops">-1</stringProp>
        </elementProp>
      </ThreadGroup>
      <hashTree>
        <JDBCSampler guiclass="TestBeanGUI" testclass="JDBCSampler" testname="SELECT 1" enabled="true">
          <stringProp name="dataSource">pg</stringProp>
          <stringProp name="query">SELECT 1</stringProp>
          <stringProp name="queryType">Select Statement</stringProp>
          <stringProp name="resultVariable"></stringProp>
          <stringProp name="variableNames"></stringProp>
        </JDBCSampler>
        <hashTree/>
      </hashTree>
    </hashTree>
  </hashTree>
</jmeterTestPlan>
EOF
fi

# k6 db scenario (custom xk6-sql binary): one connection per VU opened in the
# init scope, then SELECT 1 per iteration — the same warm shape as the locust
# and JMeter rows. Metrics mirror the native engine's names
# (db_query_duration Trend, db_errors Counter) so bench_metrics.py's db-text
# parser reads it unchanged; k6's built-in `iterations` counter supplies the
# query rate (one query per iteration).
if [[ "$HAS_K6_SQL" == 1 ]]; then
  K6_SQL_DSN="$BENCH_PG_DSN"
  case "$K6_SQL_DSN" in
    *\?*) K6_SQL_DSN="$K6_SQL_DSN&sslmode=disable" ;;
    *) K6_SQL_DSN="$K6_SQL_DSN?sslmode=disable" ;;
  esac
  cat >"$WORKDIR/db-k6.js" <<'EOF'
import sql from 'k6/x/sql';
import driver from 'k6/x/sql/driver/postgres';
import { Counter, Trend } from 'k6/metrics';

// Trend in the native engine's db_query_duration shape (isTime, values are
// ms from Date.now()).
const queryDuration = new Trend('db_query_duration', true);
const errors = new Counter('db_errors');

export const options = {
  vus: Number(__ENV.BENCH_VUS || 1),
  duration: __ENV.BENCH_DURATION || '10s',
  summaryTrendStats: ['avg', 'min', 'med', 'max', 'p(50)', 'p(90)', 'p(95)', 'p(99)'],
};

// Init scope runs once per VU: one connection per VU, reused by every
// iteration below — the warm-pool shape.
const db = sql.open(driver, __ENV.BENCH_PG_SQL_DSN);

export default function () {
  const t0 = Date.now();
  try {
    db.query('SELECT 1');
  } catch (e) {
    errors.add(1);
  }
  queryDuration.add(Date.now() - t0);
}

export function teardown() {
  db.close();
}
EOF
fi

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

# The hyperfine suites (overhead/startup) get JMeter rows on top: JMeter's
# non-GUI console summary has no percentiles, so it stays out of the
# throughput table.
hf_names=("${scenario_names[@]}")
hf_builders=("${scenario_builders[@]}")
if [[ "$HAS_JMETER" == 1 ]]; then
  hf_names+=("jmeter (native)" "perfscale (jmeter)")
  hf_builders+=("cmd_jmeter_native" "cmd_jmeter_wrapped")
fi

build_cmd() { # $1 builder, $2 vus, $3 duration, $4 csv prefix (locust native)
  case "$1" in
    cmd_locust_native) cmd_locust_native "$2" "$3" "$TARGET" 0 "$4" ;;
    cmd_locust_wrapped) cmd_locust_wrapped "$2" "$3" "$TARGET" 0 ;;
    cmd_k6_native) cmd_k6_native "$2" "$3" "$TARGET" 0 ;;
    cmd_k6_wrapped) cmd_k6_wrapped "$2" "$3" "$TARGET" 0 ;;
    cmd_yaml_get) cmd_yaml_get "$2" "$3" ;;
    cmd_yaml_get_quiet) cmd_yaml_get_quiet "$2" "$3" ;;
    cmd_jmeter_native) cmd_jmeter_native "$2" "$3" ;;
    cmd_jmeter_wrapped) cmd_jmeter_wrapped "$2" "$3" ;;
  esac
}

run_hyperfine() { # $1 vus, $2 duration, $3 runs, $4 md out, $5 json out
  local args=(--warmup "$WARMUP" --runs "$3" --export-markdown "$4" --export-json "$5")
  local i
  for i in "${!hf_names[@]}"; do
    args+=(--command-name "${hf_names[$i]}" \
      "$(build_cmd "${hf_builders[$i]}" "$1" "$2" "$WORKDIR/loc-hf")")
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

# --- ws ---------------------------------------------------------------------

if has_suite ws && [[ "$HAS_WS" == 1 ]]; then
  echo "suite: ws" >&2
  section "WebSocket echo (${VUS} VUs, ${WS_DURATION}, ${WS_ROUNDS} round-trips/connection)"
  {
    echo "| Scenario | Messages | Msgs/s | RTT avg ms | RTT p50 ms | RTT p95 ms | RTT p99 ms |"
    echo "|---|---:|---:|---:|---:|---:|---:|"
  } >>"$OUTPUT"
  ws_target="ws://127.0.0.1:${PORT}/ws"
  # locust is not in this suite on purpose: it has no built-in WebSocket
  # support (the third-party locust-plugins WebSocketUser is out of scope).
  ws_names=()
  ws_cmds=()
  if [[ "$HAS_K6" == 1 ]]; then
    ws_names+=("k6 (native)" "perfscale (k6)")
    ws_cmds+=("BENCH_VUS=$VUS BENCH_DURATION=$WS_DURATION BENCH_WS_ROUNDS=$WS_ROUNDS BENCH_TARGET=$ws_target k6 run --quiet $WORKDIR/ws.js"
      "BENCH_VUS=$VUS BENCH_DURATION=$WS_DURATION BENCH_WS_ROUNDS=$WS_ROUNDS BENCH_TARGET=$ws_target $BIN run --k6 $WORKDIR/ws.js")
  fi
  ws_names+=("perfscale (yaml)")
  ws_cmds+=("$(cmd_yaml "$VUS" "$WS_DURATION" "$WORKDIR/ws.yaml")")
  for i in "${!ws_names[@]}"; do
    echo "  ${ws_names[$i]}" >&2
    measure "${ws_names[$i]}" "${ws_cmds[$i]}" ws-text
    json_row ws.json "${ws_names[$i]}"
    echo "| ${ws_names[$i]} | $requests | $rps | $avg_ms | $p50_ms | $p95_ms | $p99_ms |" >>"$OUTPUT"
  done
  {
    echo
    echo "_Each connection performs ${WS_ROUNDS} echo round-trips (send, wait for"
    echo "the echo, send the next) against \`serve\`'s \`/ws\` endpoint. Messages"
    echo "counts sent messages; RTT is send→echo latency. The k6 scenarios"
    echo "report the same two metrics via a custom Trend/Counter pair._"
  } >>"$OUTPUT"
fi

# --- boundary ---------------------------------------------------------------

if has_suite boundary; then
  echo "suite: boundary" >&2
  # High-VU ramps exhaust file descriptors fast (one socket per VU); lift the
  # soft limit to the hard cap where permitted. Best-effort: macOS and some
  # CI containers refuse, and a lower ceiling simply shows up as the
  # boundary — which is exactly what this suite measures.
  ulimit -n 1048576 2>/dev/null || ulimit -n "$(ulimit -Hn)" 2>/dev/null || true

  b_dur_s="${BOUNDARY_DURATION%s}"
  b_out="$WORKDIR/boundary-out.$$"
  b_last_rung="${BOUNDARY_LADDER##* }"
  # tr is byte-oriented and would mangle a multibyte separator — keep this
  # rendering ASCII via sed.
  b_ladder_pretty="$(echo "$BOUNDARY_LADDER" | sed 's/ /, /g')"
  section "1% error boundary (ladder: ${b_ladder_pretty} VUs, ${BOUNDARY_DURATION} per rung)"
  {
    echo "| Engine | Boundary | Time | RPS at boundary |"
    echo "|---|---:|---:|---:|"
  } >>"$OUTPUT"

  # Native-engine staged-ramp config for one ladder rung. Prints the path.
  boundary_native_cfg() {
    local path="$WORKDIR/boundary-cfg-$1.yaml"
    [[ -f "$path" ]] || printf 'stages:\n  - duration: "%s"\n    target: %s\n' \
      "$BOUNDARY_DURATION" "$1" >"$path"
    echo "$path"
  }

  # Command builders, one per engine row: $1 = ladder rung (ramp target).
  b_cmd_k6_native() {
    echo "BENCH_TARGET=$TARGET/health BENCH_MAX_VUS=$1 BENCH_DURATION=$BOUNDARY_DURATION BENCH_ERR_PCT=$BOUNDARY_ERR_PCT k6 run $WORKDIR/boundary.js"
  }
  b_cmd_k6_wrapped() {
    echo "BENCH_TARGET=$TARGET/health BENCH_MAX_VUS=$1 BENCH_DURATION=$BOUNDARY_DURATION BENCH_ERR_PCT=$BOUNDARY_ERR_PCT $BIN run --k6 $WORKDIR/boundary.js"
  }
  b_cmd_locust() {
    echo "BENCH_MAX_VUS=$1 BENCH_DURATION_S=$b_dur_s locust -f $WORKDIR/boundary_locust.py --headless --host $TARGET --run-time $((b_dur_s + 10))s --csv $WORKDIR/b-loc-$1 --csv-full-history"
  }
  b_cmd_jmeter() {
    sed -e "s/@VUS@/$1/" -e "s/@RAMP@/$b_dur_s/" \
      -e "s/@SECS@/$((b_dur_s + 10))/" -e "s/@PORT@/$PORT/" \
      "$WORKDIR/plan-boundary-template.jmx" >"$WORKDIR/plan-boundary-$1.jmx"
    echo "cd $WORKDIR && HEAP='$BOUNDARY_JMETER_HEAP' jmeter -n -t $WORKDIR/plan-boundary-$1.jmx -Jsummariser.interval=2"
  }
  b_cmd_native() {
    echo "$BIN run -f $WORKDIR/boundary.yaml -c $(boundary_native_cfg "$1")"
  }

  # boundary_ladder $1 label, $2 parse kind, $3 command builder
  # Climbs BOUNDARY_LADDER: at each rung the engine ramps 0→rung over
  # BOUNDARY_DURATION. A rung ends the climb when the cumulative error rate
  # crosses BOUNDARY_ERR_PCT percent OR the process dies (OOM, crash — a
  # non-zero exit without a detected crossing; that IS the boundary, recorded
  # with a note). Engines that survive a rung move on to the next one.
  #
  # Before each run the target must accept fresh connections: a previous
  # engine's high-VU rung leaves tens of thousands of loopback TIME_WAIT
  # sockets behind, and with an exhausted ephemeral port range the NEXT
  # engine's every connect fails instantly (EADDRNOTAVAIL) — a bogus "1 VU,
  # 100% errors" boundary that measures the machine's drain window, not the
  # engine. The probe is 50 sequential fresh curl connections; it waits for a
  # clean round for up to ~100s (a TIME_WAIT generation is ~60s).
  boundary_settle() {
    local try fails
    for try in $(seq 1 20); do
      fails=0
      for _ in $(seq 1 50); do
        curl -sf "$TARGET/health" >/dev/null || fails=$((fails + 1))
      done
      if [[ "$fails" == 0 ]]; then
        [[ "$try" == 1 ]] || echo "  (settled after $(( (try - 1) * 5 ))s of TIME_WAIT drain)" >&2
        return 0
      fi
      [[ "$try" == 1 ]] && echo "  target not accepting fresh connections ($fails/50 failed) — waiting for TIME_WAIT drain" >&2
      sleep 5
    done
    echo "warning: target still refusing fresh connections after 100s — the next row may measure the machine, not the engine" >&2
  }

  boundary_ladder() {
    local label="$1" kind="$2" builder="$3"
    local rung crossed boundary_vus boundary_s boundary_rps rc parse_file
    for rung in $BOUNDARY_LADDER; do
      echo "  $label @ $rung VUs" >&2
      boundary_settle
      rc=0
      bash -c "$("$builder" "$rung")" >"$b_out" 2>&1 || rc=$?
      parse_file="$b_out"
      [[ "$kind" == locust ]] && parse_file="$WORKDIR/b-loc-${rung}_stats_history.csv"
      eval "$($METRICS boundary "$kind" "$parse_file" "$rung" "$b_dur_s" "$BOUNDARY_ERR_PCT")"
      if [[ "$crossed" == 1 ]]; then
        $METRICS append "$RESULTS_D/boundary.json" "$label" \
          crossed=1 boundary_vus="$boundary_vus" boundary_s="$boundary_s" \
          boundary_rps="$boundary_rps" rung="$rung"
        echo "| $label | ${boundary_vus} VUs | ${boundary_s}s | $boundary_rps |" >>"$OUTPUT"
        return
      fi
      if [[ "$rc" != 0 ]]; then
        # Died without a detected crossing: OOM/crash at this rung counts as
        # the boundary (the engine could not finish the ramp).
        $METRICS append "$RESULTS_D/boundary.json" "$label" \
          crossed=1 boundary_vus="$rung" boundary_s=0 boundary_rps=0 \
          rung="$rung" note="process died (exit $rc)"
        echo "| $label | ≈${rung} VUs (process died, exit $rc) | — | — |" >>"$OUTPUT"
        return
      fi
    done
    $METRICS append "$RESULTS_D/boundary.json" "$label" \
      crossed=0 boundary_vus=0 boundary_s=0 boundary_rps=0 rung="$b_last_rung"
    echo "| $label | >${b_last_rung} VUs (never crossed) | — | — |" >>"$OUTPUT"
  }

  [[ "$HAS_K6" == 1 ]] && boundary_ladder "k6 (native)" k6 b_cmd_k6_native
  [[ "$HAS_K6" == 1 ]] && boundary_ladder "perfscale (k6)" k6 b_cmd_k6_wrapped
  [[ "$HAS_LOCUST" == 1 ]] && boundary_ladder "locust (native)" locust b_cmd_locust
  [[ "$HAS_JMETER" == 1 ]] && boundary_ladder "jmeter (native)" jmeter b_cmd_jmeter

  # The native run is NOT --quiet: the boundary is read from its periodic
  # [stats] stream, which quiet mode suppresses. Its boundary therefore
  # includes the per-request logging cost, unlike the yaml/throughput rows.
  boundary_ladder "perfscale (yaml)" native b_cmd_native

  {
    echo
    echo "_Every engine climbs the ladder ${BOUNDARY_LADDER} (VU ramp ceilings,"
    echo "${BOUNDARY_DURATION} per rung); the boundary is the load at which the"
    echo "cumulative error rate first reaches ${BOUNDARY_ERR_PCT}%. An engine that"
    echo "crosses stops climbing; the rest continue to the next rung. A process"
    echo "dying on the ramp (OOM, crash — non-zero exit without a detected"
    echo "crossing) counts as the boundary at that rung. Detection is"
    echo "engine-specific: k6 aborts itself via a threshold (\`vus\` at abort is"
    echo "the boundary), locust is read from \`--csv-full-history\`, jmeter from"
    echo "accumulated \`summary +\` deltas (heap: \`${BOUNDARY_JMETER_HEAP}\`), the"
    echo "native engine from its periodic \`[stats]\` lines. Wrapped locust/jmeter"
    echo "rows are omitted: the wrapper is a pass-through, the boundary is an"
    echo "engine property. The \`perfscale (yaml)\` row runs without \`--quiet\`"
    echo "(quiet suppresses \`[stats]\`), so it includes the per-request logging"
    echo "cost. Between runs the suite probes the target with 50 fresh"
    echo "connections and waits out any loopback TIME_WAIT drain left by the"
    echo "previous engine — a boundary must be the engine's, not the machine's"
    echo "exhausted ephemeral ports (the workflow also widens"
    echo "\`ip_local_port_range\` and enables \`tcp_tw_reuse\`)._"
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

# --- library ---------------------------------------------------------------

if has_suite library && [[ "$HAS_LIB" == 1 ]]; then
  echo "suite: library" >&2

  # Cache dirs: lib-cold stays empty (run never burns — only install does),
  # so every cold run pays full component compilation; lib-warm is burned by
  # `perfscale install` once and hit by every burned run.
  mkdir -p "$WORKDIR/lib-cold" "$WORKDIR/lib-warm"
  lib_cfg_cold="$WORKDIR/lib-cold"
  lib_cfg_warm="$WORKDIR/lib-warm"
  echo "  perfscale install (burn)" >&2
  if ! PERFSCALE_CACHE_DIR="$lib_cfg_warm" "$BIN" install \
    "$(lib_cfg "$VUS" "$LIB_DURATION" "$WORKDIR/lib-hello.yaml")" \
    >"$WORKDIR/lib-install.log" 2>&1; then
    echo "perfscale install failed (the burned rows need the burn cache):" >&2
    cat "$WORKDIR/lib-install.log" >&2
    exit 1
  fi

  lib_run() { # $1 test file, $2 config file, $3 cache env assignment
    echo "${3:+$3 }$BIN run -f $1 -c $2"
  }
  lib_base_cmd() { # $1 duration
    lib_run "$WORKDIR/yaml-get.yaml" "$(cfg "$VUS" "$1")" ""
  }
  lib_cold_cmd() {
    lib_run "$WORKDIR/yaml-lib.yaml" \
      "$(lib_cfg "$VUS" "$1" "$WORKDIR/lib-hello.yaml")" \
      "PERFSCALE_CACHE_DIR=$lib_cfg_cold"
  }
  lib_burned_cmd() {
    lib_run "$WORKDIR/yaml-lib.yaml" \
      "$(lib_cfg "$VUS" "$1" "$WORKDIR/lib-hello.yaml")" \
      "PERFSCALE_CACHE_DIR=$lib_cfg_warm"
  }
  lib_builtin_cmd() {
    lib_run "$WORKDIR/yaml-lib-builtin.yaml" \
      "$(lib_cfg "$VUS" "$1" "$WORKDIR/lib-builtin.yaml")" ""
  }

  # Per-run cost: hyperfine at a 1s duration, where compiling the component
  # once per run (cold) vs deserializing the burn artifact (burned) is a
  # visible fraction of wall time. Raw hyperfine JSON, same as `overhead`.
  if [[ "$HAS_HYPERFINE" == 1 ]]; then
    echo "  hyperfine (${LIB_STARTUP_DURATION} runs)" >&2
    hyperfine --warmup "$WARMUP" --runs "$LIB_RUNS" \
      --export-markdown "$WORKDIR/library-startup.md" \
      --export-json "$RESULTS_D/library-startup.json" \
      --command-name "yaml (no library)" "$(lib_base_cmd "$LIB_STARTUP_DURATION")" \
      --command-name "yaml+lib (cold cache)" "$(lib_cold_cmd "$LIB_STARTUP_DURATION")" \
      --command-name "yaml+lib (burned)" "$(lib_burned_cmd "$LIB_STARTUP_DURATION")" \
      --command-name "yaml+builtin (@std/random)" "$(lib_builtin_cmd "$LIB_STARTUP_DURATION")"
    section "WASM library: per-run cost (hyperfine, ${LIB_STARTUP_DURATION})"
    cat "$WORKDIR/library-startup.md" >>"$OUTPUT"
  else
    echo "skipping library hyperfine rows: hyperfine not on PATH" >&2
  fi

  # Per-call cost: one instrumented run per variant at the standard shape.
  section "WASM library: per-call cost (${VUS} VUs, ${LIB_DURATION})"
  {
    echo "| Scenario | Requests | RPS | p95 ms | Err | CPU per req | Peak RSS |"
    echo "|---|---:|---:|---:|---:|---:|---|"
  } >>"$OUTPUT"
  for variant in base cold burned builtin; do
    echo "  $variant" >&2
    measure "lib-$variant" "$(lib_${variant}_cmd "$LIB_DURATION")"
    json_row library.json "$variant"
    echo "| $variant | $requests | $rps | $p95_ms | $err_pct% | $(cpu_per_req "$requests") µs | $T_RSS |" >>"$OUTPUT"
  done
  {
    echo
    echo "_Every row is the same GET /health shape; the only difference is one"
    echo "generated header value per request. \`base\` has no library at all,"
    echo "\`cold\` compiles the WASM component on every run (empty cache),"
    echo "\`burned\` deserializes the \`.cwasm\` artifact \`perfscale install\`"
    echo "wrote, \`builtin\` uses the native \`@std/random@v1\`. The hyperfine"
    echo "table above prices per-run compile/deserialize; this table prices the"
    echo "per-call WASM cost (JSON marshaling, per-VU instance) — burn removes"
    echo "per-run compilation, not per-call cost._"
  } >>"$OUTPUT"
fi

# --- grpc -------------------------------------------------------------------

if has_suite grpc && [[ "$HAS_GRPC" == 1 ]]; then
  echo "suite: grpc" >&2
  section "gRPC unary echo (${VUS} VUs, ${GRPC_DURATION})"
  {
    echo "| Scenario | Messages | Msgs/s | p95 ms | Err | CPU per msg | Peak RSS |"
    echo "|---|---:|---:|---:|---:|---:|---|"
  } >>"$OUTPUT"
  echo "  perfscale (yaml)" >&2
  measure "grpc-unary" "$(cmd_yaml "$VUS" "$GRPC_DURATION" "$WORKDIR/yaml-grpc.yaml")" grpc-text
  json_row grpc.json "perfscale (yaml)"
  echo "| perfscale (yaml) | $requests | $rps | $p95_ms | $err_pct% | $(cpu_per_req "$requests") µs | $T_RSS |" >>"$OUTPUT"
  {
    echo
    echo "_One-shot \`std/grpc@v1\` unary call per iteration (connect → call →"
    echo "close) against the repo's \`grpc_echo_server\` example (plaintext,"
    echo "server reflection — fetched once per run, cached per URL). Compare"
    echo "against the throughput table for the HTTP/2 + protobuf tax over plain"
    echo "HTTP. No k6 row: k6's gRPC module would measure k6, not this engine._"
  } >>"$OUTPUT"
fi

# --- graphql -----------------------------------------------------------------

if has_suite graphql && [[ "$HAS_GQL" == 1 ]]; then
  echo "suite: graphql" >&2
  section "GraphQL query (${VUS} VUs, ${GQL_DURATION})"
  {
    echo "| Scenario | Requests | RPS | p95 ms | Err | CPU per req | Peak RSS |"
    echo "|---|---:|---:|---:|---:|---:|---|"
  } >>"$OUTPUT"
  echo "  perfscale (yaml)" >&2
  measure "gql-query" "$(cmd_yaml "$VUS" "$GQL_DURATION" "$WORKDIR/yaml-gql.yaml")"
  json_row graphql.json "perfscale (yaml)"
  echo "| perfscale (yaml) | $requests | $rps | $p95_ms | $err_pct% | $(cpu_per_req "$requests") µs | $T_RSS |" >>"$OUTPUT"
  {
    echo
    echo "_One \`std/graphql@v1\` query per iteration against the repo's"
    echo "\`graphql_server\` example: document parse + schema validation"
    echo "(introspection is fetched once per run, cached per URL) + HTTP POST."
    echo "Compare against the throughput table for the GraphQL tax over plain"
    echo "HTTP._"
  } >>"$OUTPUT"
fi

# --- db ----------------------------------------------------------------------

if has_suite db; then
  echo "suite: db" >&2
  section "SQL queries (${VUS} VUs, ${DB_DURATION} per scenario)"
  {
    echo "| Scenario | Queries | QPS | p95 ms | Err | CPU per query | Peak RSS |"
    echo "|---|---:|---:|---:|---:|---:|---|"
  } >>"$OUTPUT"

  # db_row $1 label, $2 command, $3 parse kind, $4 queries per unit (a warm
  # iteration runs several query steps; the summary's iteration rate is
  # scaled up to a query rate), $5 parse file (optional)
  db_row() {
    local label="$1" cmd="$2" kind="$3" mult="${4:-1}" parse_file="${5:-}"
    echo "  $label" >&2
    measure "$label" "$cmd" "$kind" "$parse_file"
    if [[ "$parse_ok" != 1 ]]; then
      echo "| $label | — | — | — | — | — | — |" >>"$OUTPUT"
      return
    fi
    if [[ "$mult" != 1 ]]; then
      requests=$((requests * mult))
      rps=$(awk "BEGIN{printf \"%.2f\", $rps*$mult}")
    fi
    json_row db.json "$label"
    echo "| $label | $requests | $rps | $p95_ms | $err_pct% | $(cpu_per_req "$requests") µs | $T_RSS |" >>"$OUTPUT"
  }

  # SQLite in-process baseline: no server needed, so these rows run even
  # without a PostgreSQL at BENCH_PG_DSN.
  db_row "db-sqlite" "$(cmd_yaml "$VUS" "$DB_DURATION" "$WORKDIR/yaml-db-sqlite.yaml")" db-text
  db_row "db-sqlite-warm (x$DB_WARM_QUERIES)" \
    "$(cmd_yaml "$VUS" "$DB_DURATION" "$WORKDIR/yaml-db-sqlite-warm.yaml")" \
    db-text "$DB_WARM_QUERIES"

  if [[ "$HAS_DB" == 1 ]]; then
    db_row "db" "$(cmd_yaml "$VUS" "$DB_DURATION" "$WORKDIR/yaml-db.yaml")" db-text
    db_row "db-params" "$(cmd_yaml "$VUS" "$DB_DURATION" "$WORKDIR/yaml-db-params.yaml")" db-text
    db_row "db-warm (x$DB_WARM_QUERIES)" \
      "$(cmd_yaml "$VUS" "$DB_DURATION" "$WORKDIR/yaml-db-warm.yaml")" \
      db-text "$DB_WARM_QUERIES"
    db_row "db-warm-tx (x$DB_TX_CYCLES)" \
      "$(cmd_yaml "$VUS" "$DB_DURATION" "$WORKDIR/yaml-db-warm-tx.yaml")" \
      db-text "$DB_TX_CYCLES"

    if [[ "$HAS_LOCUST_DB" == 1 ]]; then
      # Plain User (no HTTP client): --host is unused but locust requires it.
      db_row "locust (psycopg2, warm)" \
        "env BENCH_PG_DSN='$BENCH_PG_DSN' locust -f $WORKDIR/locustfile_db.py --headless -u $VUS -r $VUS -t $DB_DURATION --host http://127.0.0.1 --only-summary --csv $WORKDIR/loc-db" \
        locust-csv 1 "$WORKDIR/loc-db_stats.csv"
    fi

    if [[ "$HAS_K6_SQL" == 1 ]]; then
      # NOT the stock k6: a custom binary with the xk6-sql extension.
      db_row "k6 (xk6-sql)" \
        "env BENCH_VUS=$VUS BENCH_DURATION=$DB_DURATION BENCH_PG_SQL_DSN='$K6_SQL_DSN' $K6_SQL_BIN run --quiet $WORKDIR/db-k6.js" \
        db-text
    fi

    if [[ "$HAS_JMETER_DB" == 1 ]]; then
      db_secs="${DB_DURATION%s}"
      sed -e "s/@VUS@/$VUS/" -e "s/@SECS@/$db_secs/" \
        -e "s|@DBHOST@|${DB_HOSTPORT%%:*}|" -e "s/@DBPORT@/${DB_HOSTPORT##*:}/" \
        -e "s/@DBNAME@/$DB_NAME/" -e "s/@DBUSER@/$DB_USER/" -e "s/@DBPASS@/$DB_PASS/" \
        "$WORKDIR/plan-db-template.jmx" >"$WORKDIR/plan-db.jmx"
      db_row "jmeter (JDBC, warm pool)" \
        "cd $WORKDIR && jmeter -n -t $WORKDIR/plan-db.jmx" jmeter-summary
    fi
  fi

  {
    echo
    echo "_Targets: PostgreSQL at \`BENCH_PG_DSN\` (CI service container) and"
    echo "in-process SQLite (\`sqlite::memory:\`, pool_size 1 — the default; a"
    echo "second pooled connection would be a different database). The perfscale"
    echo "rows all use \`std/db-*@v1\`: **db** = cold connect + \`SELECT 1\` per"
    echo "iteration (the connection drops at iteration end — the serverless/"
    echo "short-lived shape); **db-params** = the same with a"
    echo "\`\${rand(1,1000)}\` bind parameter; **db-warm** = one persistent-mode"
    echo "connect per iteration, then ${DB_WARM_QUERIES} queries on the parked pool,"
    echo "then \`db-close\` (Queries/QPS count queries, not iterations);"
    echo "**db-warm-tx** = ${DB_TX_CYCLES} × \`tx-begin + SELECT 1 + commit\` per"
    echo "iteration (its p95 mixes begin/query/commit samples). The competitor rows"
    echo "repeat the warm shape: locust connects once per user with psycopg2 and"
    echo "loops \`SELECT 1\` (needs \`pipx inject locust psycopg2-binary\`); JMeter"
    echo "uses a JDBC connection pool + \`SELECT 1\` sampler (needs the PostgreSQL"
    echo "JDBC jar in JMeter's \`lib/\`; its console summary has no percentiles, so"
    echo "its p95 cell is empty and avg/min/max are console aggregates);"
    echo "**k6 (xk6-sql)** is NOT stock k6 — a custom binary built with the"
    echo "\`xk6-sql\` extension + postgres driver (one connection per VU opened in"
    echo "the init scope, \`SELECT 1\` per iteration; metrics reported in the"
    echo "native engine's \`db_query_duration\`/\`db_errors\` shapes). Its k6 core"
    echo "version may differ from the stock k6 — see the CI environment summary._"
  } >>"$OUTPUT"
fi

# ---------------------------------------------------------------------------

$METRICS merge "$RESULTS_D" "$RESULTS"
echo "report written to $OUTPUT"
echo "results written to $RESULTS"
