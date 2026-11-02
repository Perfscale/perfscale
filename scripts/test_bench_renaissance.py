#!/usr/bin/env python3
"""Focused tests for the Renaissance bench integration.

Covers the renaissance-text parser (bench_metrics.py), the runner
(scripts/bench_renaissance.sh) against a FAKE java/jar fixture, and
bench_compare.py. No real Renaissance JAR is executed here — the pinned
v0.16.2 MIT jar needs Java 11+ and ~440 MB; the JSON fixture below models
the actual harness output shape (format_version 6:
`data.<benchmark>.results[]` of `{"duration_ns", "uptime_ns"}` plus
`termination`, per renaissance-harness ResultWriter.scala/ExecutionDriver.java
at v0.16.2) with invented values.

Run: python3 -m unittest scripts/test_bench_renaissance.py -v
"""

import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench_metrics

SCRIPTS = Path(__file__).resolve().parent
RUNNER = SCRIPTS / "bench_renaissance.sh"

# The harness's real JSON shape (see module docstring); values invented.
FAKE_RESULTS = {
    "format_version": 6,
    "benchmarks": ["finagle-http"],
    "data": {
        "finagle-http": {
            "termination": "normal",
            "results": [
                {"duration_ns": 1_000_000, "uptime_ns": 1_200_000},
                {"duration_ns": 3_000_000, "uptime_ns": 3_200_000},
                {"duration_ns": 2_000_000, "uptime_ns": 2_200_000},
            ],
        }
    },
}

FAKE_CONSOLE = "Renaissance Benchmark Suite, version 0.16.2\n====== finagle-http (web) ======\n"


class ParseRenaissanceTextTest(unittest.TestCase):
    def _write_results(self, doc):
        f = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
        json.dump(doc, f)
        f.close()
        self.addCleanup(os.unlink, f.name)
        return f.name

    def test_parses_results_and_trailer(self):
        path = self._write_results(FAKE_RESULTS)
        content = FAKE_CONSOLE + (
            f"renaissance_benchmark: finagle-http\n"
            f"renaissance_json: {path}\n"
            f"renaissance_duration_s: 62.5\n"
        )
        out, extra = bench_metrics.parse_renaissance_text(content)
        self.assertEqual(out["parse_ok"], 1)
        self.assertEqual(extra["benchmark"], "finagle-http")
        # mean duration 2ms → 500 ops/s; latencies are per-operation ms.
        self.assertEqual(extra["ops_per_s"], 500.0)
        self.assertEqual(extra["avg_ms"], 2.0)
        self.assertEqual(extra["min_ms"], 1.0)
        self.assertEqual(extra["max_ms"], 3.0)
        self.assertEqual(extra["repetitions"], 3)
        self.assertEqual(extra["duration_s"], 62.5)
        self.assertEqual(extra["termination"], "normal")
        # Operation throughput must never leak into the HTTP fields.
        self.assertEqual(out["requests"], 0)
        self.assertEqual(out["rps"], 0)
        self.assertEqual(out["p95_ms"], 0)

    def test_missing_trailer_is_not_a_success(self):
        out, extra = bench_metrics.parse_renaissance_text(FAKE_CONSOLE)
        self.assertEqual(out["parse_ok"], 0)
        self.assertEqual(extra["ops_per_s"], 0.0)

    def test_failed_termination_is_not_a_success(self):
        doc = json.loads(json.dumps(FAKE_RESULTS))
        doc["data"]["finagle-http"]["termination"] = "failure"
        path = self._write_results(doc)
        content = (
            f"renaissance_benchmark: finagle-http\nrenaissance_json: {path}\n"
        )
        out, extra = bench_metrics.parse_renaissance_text(content)
        self.assertEqual(out["parse_ok"], 0)
        self.assertEqual(extra["termination"], "failure")

    def test_empty_results_is_not_a_success(self):
        doc = {"format_version": 6, "data": {"finagle-http": {"termination": "normal", "results": []}}}
        path = self._write_results(doc)
        content = (
            f"renaissance_benchmark: finagle-http\nrenaissance_json: {path}\n"
        )
        out, _ = bench_metrics.parse_renaissance_text(content)
        self.assertEqual(out["parse_ok"], 0)

    def test_cli_parse_prints_extra_keys(self):
        path = self._write_results(FAKE_RESULTS)
        with tempfile.NamedTemporaryFile("w", suffix=".log", delete=False) as f:
            f.write(
                FAKE_CONSOLE
                + f"renaissance_benchmark: finagle-http\nrenaissance_json: {path}\nrenaissance_duration_s: 62.5\n"
            )
            log = f.name
        self.addCleanup(os.unlink, log)
        proc = subprocess.run(
            [sys.executable, str(SCRIPTS / "bench_metrics.py"), "parse",
             "renaissance-text", log],
            capture_output=True, text=True, check=True,
        )
        self.assertIn("ops_per_s=500.0", proc.stdout)
        self.assertIn("avg_ms=2.0", proc.stdout)
        self.assertIn("repetitions=3", proc.stdout)
        self.assertIn("benchmark=finagle-http", proc.stdout)
        self.assertIn("parse_ok=1", proc.stdout)


class CompareRenaissanceTest(unittest.TestCase):
    def test_same_benchmark_compared_across_runs(self):
        import bench_compare

        old = {"renaissance": [
            {"label": "renaissance finagle-http", "benchmark": "finagle-http",
             "ops_per_s": 500.0, "avg_ms": 2.0},
        ]}
        new = {"renaissance": [
            {"label": "renaissance finagle-http", "benchmark": "finagle-http",
             "ops_per_s": 400.0, "avg_ms": 2.5},
        ]}
        rows = list(bench_compare.collect(old, new))
        self.assertEqual(len(rows), 2)
        # throughput down (bad), latency up (bad)
        self.assertTrue(all(r[2] < r[1] for r in rows if r[3]))
        self.assertTrue(all(r[2] > r[1] for r in rows if not r[3]))

    def test_different_benchmark_never_compared(self):
        import bench_compare

        old = {"renaissance": [
            {"label": "renaissance x", "benchmark": "finagle-http",
             "ops_per_s": 500.0, "avg_ms": 2.0},
        ]}
        new = {"renaissance": [
            {"label": "renaissance x", "benchmark": "akka-uct",
             "ops_per_s": 400.0, "avg_ms": 2.5},
        ]}
        self.assertEqual(list(bench_compare.collect(old, new)), [])


class RunnerTest(unittest.TestCase):
    """Smoke the runner against a fake jar: an empty jar file and a fake
    `java` that writes the fixture JSON (and optionally fails)."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.jar = root / "renaissance-mit-0.16.2.jar"
        self.jar.write_text("")  # never really executed
        self.artifacts = root / "artifacts"
        self.fake_java = root / "fake-java"
        self._write_fake_java(exit_code=0)

    def tearDown(self):
        self.tmp.cleanup()

    def _write_fake_java(self, exit_code):
        # Emulate the harness: parse --json/--scratch-base/--run-seconds from
        # argv, write the results JSON and a scratch dir.
        self.fake_java.write_text(
            "#!/usr/bin/env bash\n"
            "json=; bench=\n"
            "while [[ $# -gt 0 ]]; do\n"
            "  case \"$1\" in\n"
            "    --json) json=\"$2\"; shift 2;;\n"
            "    --scratch-base) mkdir -p \"$2/launcher-x\"; shift 2;;\n"
            "    --run-seconds) shift 2;;\n"
            "    *) bench=\"$1\"; shift;;\n"
            "  esac\n"
            "done\n"
            "mkdir -p \"$(dirname \"$json\")\"\n"
            "cat > \"$json\" <<'JSON'\n"
            + json.dumps(FAKE_RESULTS)
            + "\nJSON\n"
            'echo "====== finagle-http (web) ======"\n'
            f"exit {exit_code}\n"
        )
        self.fake_java.chmod(self.fake_java.stat().st_mode | stat.S_IEXEC)

    def run_runner(self, **env_overrides):
        env = os.environ.copy()
        env.pop("RENAISSANCE_JAR", None)
        env.update({
            "RENAISSANCE_JAVA": str(self.fake_java),
            "RENAISSANCE_JAR": str(self.jar),
            "RENAISSANCE_RESULTS_DIR": str(self.artifacts),
            "PATH": os.environ["PATH"],
        })
        env.update(env_overrides)
        return subprocess.run(
            ["bash", str(RUNNER)], capture_output=True, text=True, env=env,
        )

    def test_success_preserves_console_and_json(self):
        proc = self.run_runner()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("====== finagle-http (web) ======", proc.stdout)
        self.assertIn("renaissance_benchmark: finagle-http", proc.stdout)
        self.assertIn("renaissance_duration_s:", proc.stdout)
        # Artifacts: console log + native JSON preserved.
        self.assertTrue((self.artifacts / "console-finagle-http.log").is_file())
        results = self.artifacts / "results-finagle-http.json"
        self.assertTrue(results.is_file())
        doc = json.loads(results.read_text())
        self.assertEqual(doc["data"]["finagle-http"]["termination"], "normal")
        # The harness's scratch dirs are removed on exit.
        self.assertFalse((self.artifacts / "scratch").exists())

    def test_missing_jar_fails_clearly(self):
        self.jar.unlink()
        proc = self.run_runner()
        self.assertEqual(proc.returncode, 1)
        self.assertIn("no jar at", proc.stderr)
        self.assertNotIn("ops_per_s", proc.stdout)  # no fake success

    def test_benchmark_failure_propagates(self):
        self._write_fake_java(exit_code=3)
        proc = self.run_runner()
        self.assertEqual(proc.returncode, 3)
        self.assertIn("benchmark JVM failed", proc.stderr)
        self.assertTrue((self.artifacts / "console-finagle-http.log").is_file())

    def test_empty_results_json_fails(self):
        # Fake java succeeds but writes an empty results file.
        self.fake_java.write_text(
            "#!/usr/bin/env bash\n"
            "json=\n"
            "while [[ $# -gt 0 ]]; do\n"
            "  case \"$1\" in\n"
            "    --json) json=\"$2\"; shift 2;;\n"
            "    *) shift;;\n"
            "  esac\n"
            "done\n"
            "mkdir -p \"$(dirname \"$json\")\" && : > \"$json\"\n"
            "exit 0\n"
        )
        proc = self.run_runner()
        self.assertEqual(proc.returncode, 2)
        self.assertIn("no results JSON", proc.stderr)


if __name__ == "__main__":
    unittest.main()
