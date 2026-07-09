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

- New `std/file-read@v1` action: read a file once into a process-wide cache
  (revalidated by mtime/size) and reference its content from later steps via
  `${{ name.content }}` — `text` or `base64` encoding.
- New `std/file-write@v1` action: write (or append) content to a file —
  e.g. persist `${{ resp.body }}` to disk; base64 content is decoded before
  writing.
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
