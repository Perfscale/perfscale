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

- **Document imports** — test and config YAML gain a top-level `import:` that
  inherits a shared base with deep-merge overrides: a relative path, a raw
  HTTP(S) URL, or `{ git, ref, file }` against any git remote (SSH/HTTPS,
  self-hosted included; clones shell out to your system git, so existing
  credentials apply). Bases chain recursively; cycles are detected. See
  [docs/core/imports.md](docs/core/imports.md) and
  [examples/import/](examples/import/).
- **Fail-closed remote imports** — network imports run only under the new
  `--allow-remote-import` CLI flag (run + lint); a document can never grant
  itself network access. URL/git-origin documents are confined to their own
  origin and can't read the local filesystem.
- **Import caching** — git imports cache under `~/.cache/perfscale/imports`:
  tags and commit SHAs forever (immutable), branches revalidate via
  `git ls-remote` after a 5-minute TTL so `ref: main` follows the branch;
  `--refresh-imports` forces a refetch.
- `perfscale lint` now resolves imports and validates the merged document,
  reporting resolution problems (missing base, cycle, blocked remote) as
  regular findings.
