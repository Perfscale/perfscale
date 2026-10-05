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

## Pro extension point: connection registries + `webrtc:` config gate

- `perfscale-connection` gains `ExtensionRegistries`, a type-erased parking
  lot where downstream pro action families (the upcoming `pro/webrtc-*`,
  RFC 007, and future `pro/*` modules) park live connections per VU
  iteration with the same Connection-ID model and iteration-end drain as the
  built-in ws/grpc/db families (reached by pro actions via
  `Context::extensions()`).
- New optional `webrtc:` config block (ICE servers, peer-connection
  guardrail). It requires the pro webrtc module: on builds without it,
  declaring the block fails load/lint/run with a clear "pro module required"
  error instead of silently doing nothing.

## Breaking: `capabilities:` is required on every library entry

- Every `libraries:` entry must now declare `capabilities:` explicitly —
  `capabilities: []` means "no grants". Documents that omitted the key now
  fail at load, `lint`, and run with a targeted error. Migration: add
  `capabilities: []` to every grant-less entry.

## `${vu}` token + new pro extension seams

- New built-in generator token `${vu}` — the current VU id (1-based) —
  usable anywhere generator tokens expand.
- `Context::call_library` lets pro action families invoke an RFC 005 library
  mid-step with computed arguments (policy, masking, and metrics go through
  the same path as `${alias.fn(...)}` tokens); `Context::vu_id()` exposes the
  VU id for per-VU artifact naming.
- Shared-variable drivers gain ephemeral ops (`apply_ephemeral`) —
  undeclared, TTL-bounded keys for engine-internal coordination (used by the
  pro `pro/webrtc-call@v1` rendezvous; the redis driver namespaces them under
  `perfscale:ephemeral:`).
