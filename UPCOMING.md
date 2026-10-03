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

## Pure libraries no longer need capability grants

- **Libraries that declare `"pure": true` in `info()` skip the
  `capabilities:` requirement for `fs`/`clock` imports.** Toolchains like
  jco/StarlingMonkey (TypeScript) link `wasi:filesystem/*` and
  `wasi:clocks/wall-clock` unconditionally, so every TS-authored library —
  even a pure calculator — forced users to write `capabilities: [fs]` plus
  `allow_library_capabilities: true`. With a pure marker the engine treats
  those imports as toolchain noise and connects them with zero preopens:
  the library loads with no grants at all, and any actual file access fails
  at runtime. The official TS SDK emits `pure: true` by default (v0.1.3+).
  The security boundary is unchanged: a filesystem preopen is still
  attached only when the YAML grants `fs`, `net` imports remain a hard
  error for everyone, and engines older than this release ignore the field
  and keep requiring the grant (fail-closed).

## Faster and finer-grained runs

- **Sub-second durations**: `duration:` and stage timings accept
  milliseconds and fractions (`500ms`, `0.5s`), with a 100 ms reporting
  floor — useful for smoke runs in CI.
- **Lower per-call WASM overhead**: the library dispatch path was trimmed
  (see the bench suite), measurably cheaper token expansion in
  library-heavy steps.

## Easier library installs

- **`sha256:` is now optional for `https:` libraries** — omitting the pin
  fetches and trusts the bytes (handy for iterating on your own library);
  pinned installs keep being verified exactly as before.
