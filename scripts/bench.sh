#!/usr/bin/env bash
set -euo pipefail

# Compares perfscale against bare k6/locust using hyperfine, across up to
# five scenarios: native k6, native locust, and perfscale wrapping each
# engine (plus perfscale's own YAML-driven native engine). Every scenario
# hits the same `perfscale serve` /health endpoint, so any wall-time gap
# between a `perfscale (*)` row and its native counterpart is perfscale's
# wrapping overhead, not the underlying tool.
#
# Scenarios whose engine isn't on PATH are skipped, not failed.

VUS="${VUS:-10}"
DURATION="${DURATION:-15s}"
WARMUP="${WARMUP:-1}"
RUNS="${RUNS:-5}"
PORT="${PORT:-18999}"
OUTPUT="${OUTPUT:-bench-report.md}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${PERFSCALE_BIN:-$ROOT/target/release/perfscale}"

if [[ ! -x "$BIN" ]]; then
  echo "building perfscale (release)..." >&2
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
fi

WORKDIR="$(mktemp -d)"
SERVE_PID=""
cleanup() {
  [[ -n "$SERVE_PID" ]] && kill "$SERVE_PID" 2>/dev/null || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

TARGET="http://127.0.0.1:${PORT}"

"$BIN" serve --port "$PORT" >"$WORKDIR/serve.log" 2>&1 &
SERVE_PID=$!

for _ in $(seq 1 50); do
  curl -fs "$TARGET/health" >/dev/null 2>&1 && break
  sleep 0.1
done
if ! curl -fs "$TARGET/health" >/dev/null 2>&1; then
  echo "perfscale serve never came up:" >&2
  cat "$WORKDIR/serve.log" >&2
  exit 1
fi

cat >"$WORKDIR/script.js" <<EOF
import http from 'k6/http';
export const options = { vus: ${VUS}, duration: '${DURATION}' };
export default function () {
  http.get('${TARGET}/health');
}
EOF

cat >"$WORKDIR/locustfile.py" <<'EOF'
from locust import HttpUser, task, constant

class HealthUser(HttpUser):
    wait_time = constant(0)

    @task
    def health(self):
        self.client.get("/health")
EOF

cat >"$WORKDIR/test.yaml" <<EOF
steps:
  - name: health check
    use: std/http@v1
    with:
      method: GET
      url: "${TARGET}/health"
EOF

cat >"$WORKDIR/config.yaml" <<EOF
vus: ${VUS}
duration: ${DURATION}
EOF

names=()
commands=()

if command -v locust >/dev/null 2>&1; then
  names+=("locust (native)")
  commands+=("locust -f $WORKDIR/locustfile.py --headless -u $VUS -r $VUS -t $DURATION --host $TARGET --only-summary")

  names+=("perfscale (locust)")
  commands+=("$BIN run --locust $WORKDIR/locustfile.py --host $TARGET -c $WORKDIR/config.yaml")
else
  echo "skipping locust (native) and perfscale (locust): locust not on PATH" >&2
fi

if command -v k6 >/dev/null 2>&1; then
  names+=("k6 (native)")
  commands+=("k6 run --quiet $WORKDIR/script.js")

  names+=("perfscale (k6)")
  commands+=("$BIN run --k6 $WORKDIR/script.js")
else
  echo "skipping k6 (native) and perfscale (k6): k6 not on PATH" >&2
fi

names+=("perfscale (yaml)")
commands+=("$BIN run -f $WORKDIR/test.yaml -c $WORKDIR/config.yaml")

hyperfine_args=(--warmup "$WARMUP" --runs "$RUNS" --export-markdown "$OUTPUT")
for i in "${!commands[@]}"; do
  hyperfine_args+=(--command-name "${names[$i]}" "${commands[$i]}")
done

hyperfine "${hyperfine_args[@]}"

echo "report written to $OUTPUT"
