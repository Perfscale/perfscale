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

- **Background processes in setup/teardown**: new `std/child_process@v1` action runs a long-lived process from the config's `before:` block — readiness gating via `waitUntil` (stdout/stderr substring or regex, TCP `port_open`, start timeout), optional `restart: on-failure|always` supervision, and `outputs` exposing `pid`/`pgid`/`port` plus captured `stdout`/`stderr` tails to test steps. A new `after:` config section runs teardown steps at the end of every run (also on setup failure and on Ctrl-C/SIGTERM), and the new `std/kill_process@v1` action stops a managed process gracefully (configurable signal, grace period, whole process tree). Any processes still alive at shutdown are terminated automatically. Process actions are fail-closed: they require `allow_process_actions: true` in the config.
