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

- Shared variables: lists in the `memory` driver are stored as a deque, so
  `pop` (`std/get_shared_variable@v1` `op: pop`) is O(1) instead of
  shifting the whole list — a 100k-item work queue went from ~5k to ~777k
  pops/s on the new bench (CPU per iteration 203 µs → 10 µs). Behaviour and
  JSON shapes are unchanged.
- Benchmarks: new `sharedvars` suite — cross-VU shared state (append → pop
  → increment, and draining a prefilled queue) on perfscale
  `shared_variables`, k6 + xk6-kv, locust and JMeter, with correctness
  columns (lost increments, queue drift) next to iterations/s. The bench
  workflow now runs with `BENCH_STRICT=1`: a row whose metrics don't parse
  fails the job (after the report and artifact are written).

- `std/pick@v1` learns object sources and field renaming: `with.key` selects
  a value from a JSON object (`from`/`file` may now hold an object or a
  string with one), and `with.remap: { source: destination }` renames
  top-level fields of the picked value — or of the whole object when no
  `key`/`index` is given. Data-losing remaps (missing source, duplicate or
  colliding destinations) fail the step instead of silently dropping fields.
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
