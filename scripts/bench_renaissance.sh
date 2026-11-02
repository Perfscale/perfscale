#!/usr/bin/env bash
# Run an official Renaissance benchmark (MIT distribution) under the same
# resource-measurement conventions as scripts/bench.sh, which calls this
# runner from its `renaissance` suite. Standalone use works too.
#
# The MIT suite JAR is NOT bundled: CI downloads + checksum-verifies the
# pinned release (see .github/workflows/bench.yml); locally supply it via
# RENAISSANCE_JAR (or place it at the default target/renaissance path). An
# explicitly selected run with a missing/invalid JAR, a Java failure, or an
# empty results file fails (non-zero exit) — it never reports a fake success.
#
# Env:
#   RENAISSANCE_JAR         path to renaissance-mit-0.16.2.jar
#                           (default: target/renaissance/renaissance-mit-0.16.2.jar
#                           relative to the repo root — the CI cache location)
#   RENAISSANCE_JAVA        java launcher (default: java; needs Java 11+,
#                           CI uses 21)
#   RENAISSANCE_BENCHMARKS  benchmark(s) to run (default: finagle-http)
#   RENAISSANCE_RUN_SECONDS measured time per benchmark via the harness's
#                           `--run-seconds` (default: 60)
#   RENAISSANCE_JAVA_ARGS   extra JVM flags, word-split (e.g. "-Xms2g -Xmx2g")
#   RENAISSANCE_ARGS        extra harness args, word-split
#   RENAISSANCE_RESULTS_DIR where console log + native JSON are preserved
#                           (default: ./renaissance-artifacts)
#
# The console log goes to stdout (bench.sh's instrumented run captures it)
# and is kept in the artifacts dir next to the harness's `--json` output.
# The harness's launcher-*/harness-* scratch directories are confined to the
# artifacts dir and removed on every exit path. Cleanup kills the JVM — no
# java process is left behind.
set -euo pipefail

RENAISSANCE_JAVA="${RENAISSANCE_JAVA:-java}"
RENAISSANCE_BENCHMARKS="${RENAISSANCE_BENCHMARKS:-finagle-http}"
RENAISSANCE_RUN_SECONDS="${RENAISSANCE_RUN_SECONDS:-60}"
RENAISSANCE_JAVA_ARGS="${RENAISSANCE_JAVA_ARGS:-}"
RENAISSANCE_ARGS="${RENAISSANCE_ARGS:-}"

JAR="${RENAISSANCE_JAR:-}"
if [[ -z "$JAR" ]]; then
  # Repo root = two levels up from this script (scripts/…).
  ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  JAR="$ROOT/target/renaissance/renaissance-mit-0.16.2.jar"
fi
if [[ ! -f "$JAR" ]]; then
  echo "renaissance: no jar at $JAR (set RENAISSANCE_JAR — CI caches the pinned, checksum-verified release at target/renaissance/)" >&2
  exit 1
fi
if ! command -v "$RENAISSANCE_JAVA" >/dev/null 2>&1; then
  echo "renaissance: java launcher '$RENAISSANCE_JAVA' not on PATH (set RENAISSANCE_JAVA)" >&2
  exit 1
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ART_DIR="${RENAISSANCE_RESULTS_DIR:-$PWD/renaissance-artifacts}"
SCRATCH="$ART_DIR/scratch"
mkdir -p "$SCRATCH"

# bench.sh invokes this runner once per benchmark; the native JSON lands at
# a known per-benchmark path so each row parses separately. A standalone
# multi-benchmark invocation runs them in one JVM but names/parses the
# results file after the FIRST benchmark in the list.
BENCH="${RENAISSANCE_BENCHMARKS%% *}"
JSON_OUT="$ART_DIR/results-$BENCH.json"
CONSOLE="$ART_DIR/console-$BENCH.log"

JAVA_PID=""
cleanup() {
  # Never leave a JVM behind (interrupt, failure, normal exit all land here).
  if [[ -n "$JAVA_PID" ]]; then
    kill "$JAVA_PID" 2>/dev/null || true
    wait "$JAVA_PID" 2>/dev/null || true
  fi
  # The harness's scratch dirs are ours to remove (we set --scratch-base).
  rm -rf "$SCRATCH"
}
trap cleanup EXIT INT TERM

now() { python3 -c 'import time; print(time.time())'; }
t0="$(now)"

# Word-splitting of RENAISSANCE_JAVA_ARGS / RENAISSANCE_ARGS is intended.
# --run-seconds bounds the measured phase (plus benchmark warmup) instead of
# the harness's fixed repetition count.
"$RENAISSANCE_JAVA" $RENAISSANCE_JAVA_ARGS -jar "$JAR" \
  --json "$JSON_OUT" --scratch-base "$SCRATCH" \
  --run-seconds "$RENAISSANCE_RUN_SECONDS" $RENAISSANCE_ARGS $RENAISSANCE_BENCHMARKS \
  >"$CONSOLE" 2>&1 &
JAVA_PID=$!
rc=0
wait "$JAVA_PID" || rc=$?
JAVA_PID=""

# stdout is what bench.sh's measure() parses; the log itself is preserved.
cat "$CONSOLE"

if [[ "$rc" != 0 ]]; then
  echo "renaissance: benchmark JVM failed (rc=$rc) — console preserved at $CONSOLE" >&2
  exit "$rc"
fi
if [[ ! -s "$JSON_OUT" ]]; then
  echo "renaissance: no results JSON at $JSON_OUT — failing rather than reporting a fake result" >&2
  exit 2
fi

# Machine-readable trailer for the renaissance-text parser
# (bench_metrics.py): the console text is narrative, so the results path and
# wall duration come from here rather than being guessed out of it.
echo "renaissance_benchmark: $BENCH"
echo "renaissance_json: $JSON_OUT"
echo "renaissance_duration_s: $(python3 -c "print(round($(now) - $t0, 1))")"
echo "renaissance_artifacts: $ART_DIR"
