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

- **Man page**: the full CLI reference now ships as a real man page. `man perfscale` works after a global npm install (npm links `man/perfscale.1` from the package) or via `perfscale man --install` for other install methods. On Windows, where there is no `man(1)`, the new `perfscale man` subcommand prints the same manual as plain text (`--raw` gives the roff source).
