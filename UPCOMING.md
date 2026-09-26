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

## Libraries: run settings, secrets, and policy rules (RFC 005 phase 3.5)

- **Run settings in every library call.** WASM and native libraries now
  receive a frozen JSON snapshot of the run — `vus`/`duration_ms` (fixed
  profile) or `stages`/`arrival`, plus `seed` and the config `variables:`
  with `${{ env.* }}` resolved — via `settings_json` in the call context
  (WIT `perfscale:library@0.2.0`). Components built against the 0.1 ABI
  keep working unchanged; they simply never see the settings.
- **`secret: true` in library metadata is now enforced**: results of
  functions the author marked secret are masked (`***`) in the run log,
  exactly like env secrets.
- **Entry-level policy rules** on `libraries:` — `secret: true` (mask every
  result), `allow:`/`deny:` (call whitelists/blacklists; `deny` wins, a
  blocked call fails the step and lint flags it), and `log:` (always mask
  the listed functions' results). Masking is additive — there is no unmask.

## Libraries: burn — AOT precompilation and standalone binaries

- **Burn cache.** `perfscale install` now precompiles every declared WASM
  library (remote and local `./path.wasm` alike) into
  `<cache>/libraries/<sha256>.cwasm`; `run`/`lint` deserialize that artifact
  instead of recompiling the component on every run — seconds of Cranelift
  compilation become milliseconds. The cache is advisory: any mismatch
  silently falls back to a full compile, and re-running `perfscale install`
  after a perfscale upgrade re-burns. Local libraries are burned too, still
  without a `perfscale.lock` entry.
- **`perfscale burn -f test.yaml [-c config.yaml] -o <output>`** builds a
  standalone binary: a copy of perfscale with the documents' libraries
  embedded. The derived binary needs no `.wasm` files, cache, or
  `perfscale.lock` — one file to ship to load generators. Burn artifacts
  are tied to the perfscale build (wasmtime version, target, engine
  config); re-run `install`/`burn` after upgrading. Per-call library
  overhead is unchanged — burn removes per-run compilation, not call cost.

## Fix: WASM libraries with the `fs` capability panicked during runs

- **Fix:** a WASM library granted the `fs` capability (or any WASI hostcall
  that blocks, e.g. clocks) crashed the run with "Cannot start a runtime
  from within a runtime" on the first file access — the guest call ran on a
  tokio worker thread, where wasmtime-wasi's sync hostcalls cannot block.
  Guest calls (`init`/`call`/`info` probing) now run on a blocking thread
  whenever the caller is inside a tokio runtime; sync callers (lint, unit
  tests, embedders) are unchanged. Present in v0.21/v0.22 — `fs`-capable
  libraries loaded and linted fine but panicked at run time.
