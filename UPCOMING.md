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
## Faster engine + `perfscale-connection` crate

Profiling-driven performance work (400 VUs against a local echo):

- The release build now uses `lto = "thin"` and `codegen-units = 1` —
  binaries are ~19% smaller.
- The HTTP client is sharded per virtual user instead of one
  process-global `reqwest` client — the hyper connection-pool mutex that
  dominated the CPU profile (12.5% of samples) is gone. Under tokio
  work-stealing, per-VU sharding won over thread-local sharding.
- Interpolation is single-pass now (−12% on the multi-placeholder
  benchmark).

New zero-dependency workspace crate **`perfscale-connection`**: a
`Connection` trait + `ConnectionRegistry` that the WebSocket, gRPC, and
database families now share for parking named connections (`outputs: conn`
→ `${{ conn.id }}`). Behavior, id formats, and error messages are
unchanged; adding the next protocol family is now a much smaller change.
See `docs/core/architecture.md` for the new pipeline overview.

