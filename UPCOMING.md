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

- Faster payload generation and interpolation: the `${` / `${{` token scans
  run on a SIMD substring search (`memchr::memmem` — SSE2/AVX2/NEON) instead
  of a scalar one, and built-in tokens (`${seq}`, `${uuid}`, `${rand(…)}`,
  `${choice(…)}`, …) write straight into the output buffer instead of
  allocating a string each. Micro-benchmarks: a 4 KiB body with trailing
  tokens −71%, a 4 KiB interpolated string −60%, built-in-heavy payloads
  −41% (7 → 2 allocations per message). HTTP responses move a valid UTF-8
  body into the step output without copying it, the reason phrase is no
  longer allocated per response, and under `--quiet` successful
  `std/http@v1` / `std/graphql@v1` requests skip formatting their log line.
