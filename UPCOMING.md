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

- New `std/tcp@v1` and `std/udp@v1` actions: raw TCP/UDP probes — connect (TCP),
  send an optional payload (`send` / `send_base64`), optionally read a response
  and assert a substring (`expect`), all with per-request timing. Latency feeds
  the same metrics as HTTP, so percentiles are comparable across transports.
- Config files gain a `before:` block: setup steps that run **once** before the
  load (not per iteration). Each step's `outputs` is exposed to test steps under
  the `config` namespace — `${{ config.<name>.<field> }}` — for building a
  reusable value (e.g. a token or connection profile). A failing setup step
  aborts the run before any VU starts.
- Config files gain a `variables:` block: static values exposed to steps as
  `${{ vars.<key> }}`.
- Steps accept `uses:` as an alias for `use:`.
- New `std/file-read@v1` action: read a file once into a process-wide cache
  (revalidated by mtime/size) and reference its content from later steps via
  `${{ name.content }}` — `text` or `base64` encoding.
- New `std/file-write@v1` action: write (or append) content to a file —
  e.g. persist `${{ resp.body }}` to disk; base64 content is decoded before
  writing.
- `std/http@v1` output now includes response `headers` (lowercase names),
  and `${{ ... }}` paths descend nested objects — chain requests on response
  headers: `headers: { x-session: "${{ r1.headers.x-session }}" }`.
- `std/http@v1` can send `multipart/form-data`: a `multipart:` array of
  parts — text fields (`value`) and file uploads (`file`, with optional
  `filename`/`content_type`). Mutually exclusive with `body`; the boundary
  header is set automatically.

### Changed

- Release notes are now written by hand in `UPCOMING.md` instead of being a
  bare commit diff; the release workflow publishes and then resets the file.
- Steps without `${{ ... }}` placeholders skip the interpolation pass
  entirely — no per-iteration deep clone of the `with:` block on the hot
  path. The `${{ ... }}` variable syntax is now fully documented in the
  YAML reference.
- The native engine tracks request durations in a fixed-size HDR histogram
  (~tens of KB) instead of storing every sample: long soak runs no longer
  grow memory 8 bytes per request (a 30-hour run at 10k RPS previously
  needed ~26 GB at the final summary). Quantiles are now within ≤1% of the
  exact value — invisible at the precision the summary prints.
- The step engine exposes an `ActionHandler` registration seam
  (`register_action`) so downstream builds can add custom `pro/*` actions
  without forking; built-in `std/*` actions pay no lookup cost for it.
