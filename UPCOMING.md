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

- Faster startup and lint: the JSON schemas that validate `config.yaml` and test files are now compiled once per process instead of on every parse — `perfscale run` / `perfscale lint` validation is ~10–100× faster, and newly added config fields no longer make every parse slower.
