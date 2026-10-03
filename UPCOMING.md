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

## Breaking: `capabilities:` is required on every library entry

- Every `libraries:` entry must now declare `capabilities:` explicitly —
  `capabilities: []` means "no grants". Documents that omitted the key now
  fail at load, `lint`, and run with a targeted error. Migration: add
  `capabilities: []` to every grant-less entry.
