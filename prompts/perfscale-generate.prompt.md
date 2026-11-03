---
description: Generate a perfscale test from a plan in specs/
---

Use the **perfscale-test-generator** agent.

Plan: $ARGUMENTS (a file under `specs/`; if omitted, ask which plan)

Context to hand the generator:

- Seed test: `${seedFile}`.
- Config: `${configFile}`.

Expected output: `tests/<name>.yaml` (plus config changes if the plan
requires them), written through the MCP write tools with lint-on-write, and
verified with a **small run — 1 VU, a few seconds only**. Do not run full
load.
