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

## Library distribution (RFC 005 phase 3)

- **`perfscale install`** fetches remote value-generator libraries declared in `libraries:` and pins them: `https://…/lib.wasm` sources (with a required `sha256:`) and `git+<repo>@<ref>#<path>` sources are fetched once, digest-verified, stored in the content-addressed cache (`<cache>/libraries/<sha256>.wasm`), and recorded in a TOML **`perfscale.lock`** next to the declaring file (commit-pinned for git). `run`/`lint` afterwards resolve remote refs through lock + cache **fully offline** — a missing pin or cache artifact is a hard error that says to run `perfscale install`. `--refresh` re-resolves git refs. Cache honors `PERFSCALE_CACHE_DIR`.
