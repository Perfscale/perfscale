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

- **`boundary` benchmark suite** — ramps 0→`BOUNDARY_MAX_VUS` (default 2000)
  VUs over `BOUNDARY_DURATION` (default 30s) and reports the VU level, time,
  and RPS where the cumulative error rate first reaches `BOUNDARY_ERR_PCT`
  (default 1%), side by side for k6, locust, JMeter, and the native engine.
