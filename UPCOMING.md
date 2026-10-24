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

- Branching and data-driven scenarios in YAML, enough to port k6 scripts
  like `if (Math.random() < 0.15) like(post)` one to one:
  - step-level `if:` — `==`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `||` over
    interpolated values and `${…}` tokens; skipped steps record nothing;
  - `${{ __vu__ }}` / `${{ __iter__ }}` (k6's `__VU` / `__ITER`);
  - paths index arrays and read into JSON bodies:
    `${{ feed.body.posts.0.id }}`;
  - `std/pick@v1` — a random or per-VU element from an array or a JSON
    file (parsed once per process);
  - `std/sleep@v1` expands tokens (`ms: "${rand(3000,7000)}"`) and rejects
    non-numeric strings instead of silently sleeping 1s.
- `std/pubsub@v1`: new `flush` parameter (`message` | `none`, default
  `message` — unchanged). `flush: message` flushes and awaits the broker ack
  after publishing, so `pubsub_publish_ms` is the full round trip;
  `flush: none` is fire-and-forget (samples measure the enqueue only) — the
  xk6-nats posture, for like-for-like warm throughput comparisons. Pairs
  with `scope: vu`; with `scope: exchange` one unsampled flush still runs
  before the connection drops so queued messages are not lost.
- WASM reporters compile once, not twice: a reporter component
  (`use: wasm:./reporter.wasm`) used to pay a full Cranelift compilation at
  both validate and delivery — and again on every `lint`. The compiled
  component is now shared within a run, and a write-through burn cache
  (`<cache>/reporters/<sha256>.cwasm`, same `PFSBURN1` format and
  invalidation rules as libraries) makes repeat `lint`/`run` deserialize
  native code instead of recompiling. `perfscale install` prewarms WASM
  reporters alongside libraries (the explicit warm-cache step), and the
  lint/validate path writes the artifact in the background — a cold lint no
  longer pays the serialize+write on its critical path.
- Built-in `html` reporter (`std/report/html@v1`, `--report html:report.html`):
  a single self-contained dark-themed page with the run identity, threshold
  verdicts, a sortable per-step table with samples sparklines, and metric
  families — all CSS/JS inlined, opens offline, archives as a CI artifact.
