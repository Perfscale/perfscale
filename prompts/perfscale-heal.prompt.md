---
description: Heal a failing perfscale test
---

Use the **perfscale-test-healer** agent.

Failing test: $ARGUMENTS

Context to hand the healer:

- Seed test: `${seedFile}`.
- Config: `${configFile}`.

The healer reproduces with a **small run (1 VU, a few seconds)**, diagnoses,
patches, and re-runs. It never edits `thresholds:` — if an SLO gate fails,
that is a finding to report, not something to loosen. If the target itself
is broken, the healer marks the test `skip:` with a reason instead of
contorting the test.
