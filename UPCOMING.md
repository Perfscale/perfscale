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

## Library sandbox hardening (security audit)

- **The artifact cache is verified at run time**: a cached library `.wasm`
  whose bytes no longer match the pinned digest (cache corruption or
  tampering) is now a hard error pointing at `perfscale install` — the pin
  was previously checked only at install time. Local `./path.wasm`
  libraries remain unverified by design (they carry no digest; treat them
  like the YAML itself).
- **Component size cap at load**: a `.wasm` over 64 MiB is rejected before
  compilation (compile-time memory scales with module size), matching the
  cap `perfscale install` already enforces on remote fetches.
