#!/usr/bin/env bash
# Run the official SPECjbb2015 benchmark (a separate commercial Java
# benchmark — not an HTTP target and not a load-generator competitor) under
# the same resource-measurement conventions as scripts/bench.sh, which calls
# this runner from its opt-in `specjbb` suite. Standalone use works too.
#
# The kit is NOT bundled or downloaded: supply it via SPECJBB_HOME (kit root)
# or SPECJBB_JAR (path to specjbb2015.jar). An explicitly selected run with an
# invalid path fails (exit 1) — it never reports a fake success.
#
# Env:
#   SPECJBB_HOME        kit root (specjbb2015.jar inside)
#   SPECJBB_JAR         direct path to specjbb2015.jar (wins over HOME)
#   SPECJBB_JAVA        java launcher (default: java)
#   SPECJBB_MODE        run mode (default: COMPOSITE; MULTI / DIST also exist)
#   SPECJBB_JAVA_ARGS   extra JVM flags, word-split (e.g. "-Xms4g -Xmx4g")
#   SPECJBB_ARGS        extra benchmark args, word-split
#   SPECJBB_RESULTS_DIR where console log + SPEC report tree are preserved
#                       (default: ./specjbb-artifacts-<utc-stamp>)
#
# The official run is never shortened: `java -jar specjbb2015.jar -m <MODE>`
# runs to its own completion. Console output goes to stdout (bench.sh's
# instrumented run captures and parses it) and is also kept in the artifacts
# dir next to the kit's result/ report tree. Cleanup kills the JVM — no java
# process is left behind.
set -euo pipefail

SPECJBB_JAVA="${SPECJBB_JAVA:-java}"
SPECJBB_MODE="${SPECJBB_MODE:-COMPOSITE}"
SPECJBB_JAVA_ARGS="${SPECJBB_JAVA_ARGS:-}"
SPECJBB_ARGS="${SPECJBB_ARGS:-}"

JAR="${SPECJBB_JAR:-}"
if [[ -z "$JAR" && -n "${SPECJBB_HOME:-}" ]]; then
  JAR="${SPECJBB_HOME%/}/specjbb2015.jar"
fi
if [[ -z "$JAR" ]]; then
  echo "specjbb2015: no kit supplied — set SPECJBB_HOME (kit root) or SPECJBB_JAR (jar path)" >&2
  exit 1
fi
if [[ ! -f "$JAR" ]]; then
  echo "specjbb2015: no jar at $JAR (check SPECJBB_HOME / SPECJBB_JAR)" >&2
  exit 1
fi
if ! command -v "$SPECJBB_JAVA" >/dev/null 2>&1; then
  echo "specjbb2015: java launcher '$SPECJBB_JAVA' not on PATH (set SPECJBB_JAVA)" >&2
  exit 1
fi

# The jar must run from the kit root — the benchmark resolves its config and
# writes its report tree relative to the working directory.
KIT_ROOT="$(cd "$(dirname "$JAR")" && pwd)"
JAR="$KIT_ROOT/$(basename "$JAR")"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ART_DIR="${SPECJBB_RESULTS_DIR:-$PWD/specjbb-artifacts-$STAMP}"
mkdir -p "$ART_DIR"

JAVA_PID=""
cleanup() {
  # Never leave a JVM behind (interrupt, failure, normal exit all land here).
  if [[ -n "$JAVA_PID" ]]; then
    kill "$JAVA_PID" 2>/dev/null || true
    wait "$JAVA_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

now() { python3 -c 'import time; print(time.time())'; }
t0="$(now)"

cd "$KIT_ROOT"
# Word-splitting of SPECJBB_JAVA_ARGS / SPECJBB_ARGS is intended (flag lists).
"$SPECJBB_JAVA" $SPECJBB_JAVA_ARGS -jar "$JAR" -m "$SPECJBB_MODE" $SPECJBB_ARGS \
  >"$ART_DIR/console-$SPECJBB_MODE.log" 2>&1 &
JAVA_PID=$!
rc=0
wait "$JAVA_PID" || rc=$?
JAVA_PID=""

# stdout is what bench.sh's measure() parses; the log itself is preserved.
cat "$ART_DIR/console-$SPECJBB_MODE.log"

# SPECjbb2015 writes its report tree next to the jar (result/, mode-named
# variants in multi/distributed runs) — copy it next to the console log so
# nothing lives only inside the kit directory.
for d in result result-*; do
  [[ -d "$d" ]] && cp -R "$d" "$ART_DIR/" || true
done

# Machine-readable trailer for the specjbb-text parser (bench_metrics.py):
# the console text is SPEC's proprietary format, so mode/duration come from
# here rather than being guessed out of it.
echo "specjbb_mode: $SPECJBB_MODE"
echo "specjbb_duration_s: $(python3 -c "print(round($(now) - $t0, 1))")"
echo "specjbb_artifacts: $ART_DIR"
exit "$rc"
