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

- **WASM libraries (RFC 005, phase 2)**: `libraries:` now accepts local `.wasm` components — `use: ./libs/fixer-ids.wasm` — so you can write your own `${alias.fn(...)}` generators in Rust (TS/JS and Go SDKs coming). Libraries run in a wasmtime sandbox: per-call fuel and memory limits, and a fail-closed capability model — a component's imports are checked against the `capabilities:` grant in YAML (`fs` = read-only preopens confined to `fs_root`, `clock` = wall clock; `net` is reserved and rejected until host-mediated HTTP lands). The `perfscale-library-sdk` crate gives authors a `Library` trait, per-message `memo()` for id reuse, a seeded PRNG, and a runtime-free test harness; see `crates/perfscale-library-sdk/README.md`.
