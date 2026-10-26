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
- Hot-path allocation and correctness improvements: built-in payload expansion copies literal spans instead of individual bytes, token arguments are borrowed instead of allocated per call, and non-ASCII payloads are preserved correctly. GraphQL schemas are shared through `Arc` instead of deep-cloned per execution, each query is parsed once, and owned JSON variables expand in place. Rust WASM guests advertise `settings_once`, so run settings cross the host/guest boundary once per instance instead of being copied on every call; older components keep the full-settings behavior.
