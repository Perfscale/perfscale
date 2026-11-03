---
description: "Threshold gate: will the target hold a given load? (HOLD / NOT HOLD)"
---

Answer one question: **will the target hold the stated load within its
SLOs?** This is the only workflow that runs at full load.

## 1. Confirm before running

State the run you are about to make — test file, load shape, target load
(e.g. "500 rps for 10 min"), and expected impact on the target — and **ask
the user to confirm explicitly before starting**. Full-load runs generate
real traffic; never start one on your own initiative.

## 2. Inputs

- Test: $ARGUMENTS (plus the target load and the SLO thresholds; read the
  thresholds from the test's `after: [std/thresholds@v1]` gate or the plan
  in `specs/`).
- Seed test `${seedFile}` and config `${configFile}` for conventions.

## 3. Run

Use `run_test` with a stepped shape (`stages:` or `arrival:`) ramping up to
the target load, so the breaking point is visible if the target fails early.
This is a real full-load run against the target.

## 4. Verdict

Report **HOLD** or **NOT HOLD**:

- Per-threshold pass/fail with the observed values (from `parse_summary` /
  the summary's `thresholds` field).
- The load level where the first threshold broke, or "held up to target".
- The suspected limiting factor from the metrics: error spike, latency
  knee, or throughput plateau (`dropped_iterations` in arrival mode).

The verdict follows the threshold gate's exit code (`severity: fail` makes
a violated gate exit non-zero), so this workflow is usable in CI.
