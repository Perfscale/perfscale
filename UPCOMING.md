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

## `perfscale self-update`: interactive by default

- `perfscale self-update` now asks for confirmation before downloading and
  replacing the binary (`Update perfscale 0.24.0 → 0.25.0? [Y/n]` — Enter
  means yes, `n` cancels with exit code 0). For scripts and CI pass
  `--non-interactive`; without it, a non-terminal stdin fails with a hint
  instead of hanging on a prompt nobody can answer. `--check` stays
  read-only and never asks.
