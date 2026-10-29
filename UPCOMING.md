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

- Benchmarks: the `sytester` suite now compares SyTester across five
  scenarios modelled on its own example plans — `get`, `post` (JSON POST
  with per-request dynamic fields), `steps` (GET → extract → POST), `grpc`
  (unary echo) and `db` (`SELECT 1` on a warm PostgreSQL connection) —
  each at a fixed arrival rate against perfscale's `arrival:` engine (and
  k6 on the HTTP scenarios). Select scenarios with `SYT_SCENARIOS`.
