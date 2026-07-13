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

### Added

- **npm install**: `npm install -g @perfscale/exe` installs the standalone
  binary for your platform (esbuild-style optionalDependencies — npm fetches
  only the matching `@perfscale/<os>-<arch>` package; Linux builds are static
  musl and run on any distro). Published automatically from every release tag.
- Actions can emit **custom run metrics**: a `metrics` object in a step's output
  value (name → number) is summed across VUs/iterations by the native engine and
  reported in the summary as `<name>: <total> <rate>/s` — the same line shape the
  downstream parsers already read. Enables protocol-specific dashboards (e.g. FIX
  send/receive message rates) without changing the metrics pipeline.
