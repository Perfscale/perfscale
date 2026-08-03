# Upcoming release

<!--
Release notes for the next release, written as features land.

- Append short, user-facing entries below this comment as you merge changes
  (what changed and why a user cares — not commit messages).
- On a `v*` tag, the release workflow publishes everything below the comment
  as the release body (with the auto-generated changelog appended), then
  resets this file back to the template.
- If this file has no entries at tag time, the release falls back to
  auto-generated notes and the workflow prints a warning.
-->
## Thresholds — SLO gates for every protocol

Declare pass/fail conditions for a run and let CI go red exactly when the
system under test breaks your SLO. The new `std/thresholds@v1` step lives in
the scenario's `after:` block and evaluates k6-style expressions
(`p95<500ms`, `rate<0.05`, `count==0`) over the run's aggregated metrics —
any family: `http_req_*`, `ws_*`, `grpc_*`, `db_*`.

- Aggregations: `avg`, `min`, `max`, `p50`–`p99`, `count`, `rate` (failed
  invocations per step, recorded automatically).
- `severity: fail` (default) exits the CLI with code 1; `warn`/`info` record
  without failing.
- The gate's status, message (truncated to 200 chars) and structured
  violations land in the run summary JSON; the platform shows a thresholds
  card and fires a `run.threshold_failed` webhook event.
- Gates over unknown metric names fail loudly (config error), so a typo
  can't silently pass your SLO.

See the new **Thresholds** page in the docs.

