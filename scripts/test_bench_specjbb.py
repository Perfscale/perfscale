#!/usr/bin/env python3
"""Focused tests for the SPECjbb2015 bench integration.

Covers the specjbb-text parser (bench_metrics.py) and the runner
(scripts/bench_specjbb2015.sh) against a FAKE java/kit fixture — no real
SPECjbb2015 binary or proprietary output is used. A run against the real
commercial kit is impossible here.

Run: python3 -m unittest scripts/test_bench_specjbb.py -v
"""

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
RUNNER = SCRIPTS / "bench_specjbb2015.sh"

# Shape modeled on the public SPECjbb2015 console summary format (values are
# invented fixture data, not output from a real run).
FAKE_SUMMARY = """\
<some simulated preamble>
RUN RESULT: hbIR (max attempted) = 42844, hbIR (settled) = 40017, max-jOPS = 40017, critical-jOPS = 13842
"""


class ParseSpecjbbTextTest(unittest.TestCase):
    def test_parses_jops_and_trailer(self):
        content = FAKE_SUMMARY + (
            "specjbb_mode: COMPOSITE\n"
            "specjbb_duration_s: 240.5\n"
        )
        out, extra = bench_metrics.parse_specjbb_text(content)
        self.assertEqual(out["parse_ok"], 1)
        self.assertEqual(extra["max_jops"], 40017)
        self.assertEqual(extra["critical_jops"], 13842)
        self.assertEqual(extra["mode"], "COMPOSITE")
        self.assertEqual(extra["duration_s"], 240.5)
        # jOPS must never leak into the HTTP-request metric fields.
        self.assertEqual(out["requests"], 0)
        self.assertEqual(out["rps"], 0)
        self.assertEqual(out["p95_ms"], 0)

    def test_jops_without_trailer(self):
        out, extra = bench_metrics.parse_specjbb_text(FAKE_SUMMARY)
        self.assertEqual(out["parse_ok"], 1)
        self.assertEqual(extra["max_jops"], 40017)
        self.assertEqual(extra["mode"], "")
        self.assertEqual(extra["duration_s"], 0.0)

    def test_garbage_is_not_a_success(self):
        out, extra = bench_metrics.parse_specjbb_text("nothing to see here\n")
        self.assertEqual(out["parse_ok"], 0)
        self.assertEqual(extra["max_jops"], 0)

    def test_cli_parse_prints_extra_keys(self):
        with tempfile.NamedTemporaryFile(
            "w", suffix=".log", delete=False
        ) as f:
            f.write(FAKE_SUMMARY + "specjbb_mode: MULTI\nspecjbb_duration_s: 10.0\n")
            path = f.name
        try:
            proc = subprocess.run(
                [sys.executable, str(SCRIPTS / "bench_metrics.py"), "parse",
                 "specjbb-text", path],
                capture_output=True, text=True, check=True,
            )
            self.assertIn("max_jops=40017", proc.stdout)
            self.assertIn("critical_jops=13842", proc.stdout)
            self.assertIn("mode=MULTI", proc.stdout)
            self.assertIn("parse_ok=1", proc.stdout)
        finally:
            os.unlink(path)


class CompareSpecjbbModesTest(unittest.TestCase):
    def test_only_same_mode_compared(self):
        import bench_compare

        old = {"specjbb": [
            {"label": "specjbb2015 (COMPOSITE)", "mode": "COMPOSITE",
             "max_jops": 100, "critical_jops": 50},
        ]}
        new = {"specjbb": [
            {"label": "specjbb2015 (COMPOSITE)", "mode": "MULTI",
             "max_jops": 90, "critical_jops": 40},
        ]}
        rows = list(bench_compare.collect(old, new))
        self.assertEqual(rows, [])
        new["specjbb"][0]["mode"] = "COMPOSITE"
        rows = list(bench_compare.collect(old, new))
        self.assertEqual(len(rows), 2)
        self.assertTrue(all(r[2] < r[1] for r in rows))  # 90 < 100 etc.


class RunnerTest(unittest.TestCase):
    """Smoke the runner against a fake kit: an empty specjbb2015.jar and a
    fake `java` that emits the fixture summary (and optionally fails)."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.kit = root / "kit"
        self.kit.mkdir()
        (self.kit / "specjbb2015.jar").write_text("")  # never really executed
        self.artifacts = root / "artifacts"
        self.fake_java = root / "fake-java"
        self._write_fake_java(exit_code=0)

    def tearDown(self):
        self.tmp.cleanup()

    def _write_fake_java(self, exit_code):
        self.fake_java.write_text(
            "#!/usr/bin/env bash\n"
            "cat <<'SUMMARY'\n" + FAKE_SUMMARY + "SUMMARY\n"
            # Emulate the kit writing its report tree next to the jar.
            "mkdir -p result && echo report > result/report.txt\n"
            f"exit {exit_code}\n"
        )
        self.fake_java.chmod(self.fake_java.stat().st_mode | stat.S_IEXEC)

    def run_runner(self, **env_overrides):
        env = os.environ.copy()
        env.pop("SPECJBB_HOME", None)
        env.pop("SPECJBB_JAR", None)
        env.update({
            "SPECJBB_JAVA": str(self.fake_java),
            "SPECJBB_RESULTS_DIR": str(self.artifacts),
            "PATH": os.environ["PATH"],
        })
        env.update(env_overrides)
        return subprocess.run(
            ["bash", str(RUNNER)], capture_output=True, text=True, env=env,
        )

    def test_success_via_home(self):
        proc = self.run_runner(SPECJBB_HOME=str(self.kit))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("max-jOPS = 40017", proc.stdout)
        self.assertIn("specjbb_mode: COMPOSITE", proc.stdout)
        self.assertIn("specjbb_duration_s:", proc.stdout)
        # Artifacts: console log + the kit's report tree are preserved.
        self.assertTrue((self.artifacts / "console-COMPOSITE.log").is_file())
        self.assertTrue((self.artifacts / "result" / "report.txt").is_file())

    def test_success_via_jar_and_mode(self):
        proc = self.run_runner(
            SPECJBB_JAR=str(self.kit / "specjbb2015.jar"), SPECJBB_MODE="MULTI",
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("specjbb_mode: MULTI", proc.stdout)
        self.assertTrue((self.artifacts / "console-MULTI.log").is_file())

    def test_invalid_path_fails_clearly(self):
        proc = self.run_runner(SPECJBB_HOME="/nonexistent/kit")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("no jar at", proc.stderr)
        self.assertNotIn("max-jOPS", proc.stdout)  # no fake success

    def test_no_kit_configured_fails(self):
        proc = self.run_runner()
        self.assertEqual(proc.returncode, 1)
        self.assertIn("SPECJBB_HOME", proc.stderr)

    def test_benchmark_failure_propagates(self):
        self._write_fake_java(exit_code=3)
        proc = self.run_runner(SPECJBB_HOME=str(self.kit))
        self.assertEqual(proc.returncode, 3)
        # The console log is still preserved for diagnosis.
        self.assertTrue((self.artifacts / "console-COMPOSITE.log").is_file())


if __name__ == "__main__":
    unittest.main()
