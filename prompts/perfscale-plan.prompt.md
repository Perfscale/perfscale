---
description: Plan a perfscale load-test scenario and write it to specs/
---

Use the **perfscale-test-planner** agent.

Request: $ARGUMENTS

Context to hand the planner:

- Seed test: `${seedFile}` (model conventions on it — base URL, auth, env
  imports).
- Config: `${configFile}`.
- Any OpenAPI / HAR / PRD files the user mentioned.

Expected output: `specs/<name>.md` with the step flow, load shape, test
data, and concrete SLO thresholds. The planner may run the seed test once
(1 VU) to confirm the target is reachable — nothing heavier.
