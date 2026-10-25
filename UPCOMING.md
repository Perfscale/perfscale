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

- `std/http@v1` `pool: dedicated` — one HTTP client per VU, created on the
  VU's first request and held until the VU exits. The default shard pools
  cap idle connections at 64 per host per shard, so past
  ~`shards × 64` VUs a VU's keep-alive connection is evicted between its
  requests (connect churn, TIME_WAIT buildup); dedicated mode removes the
  ceiling for "N VUs = N standing connections" scenarios. Think time
  (`std/sleep@v1`) does not close the connection, clients are dropped at VU
  exit (fixed, staged, and arrival-rate runs alike), and the mode composes
  with `source_ips` (client key: `(vu_id, source_ip)`).
- Multi-source-IP HTTP load (`std/http@v1` `source_ips`): a step can bind
  outgoing connections to a list of local source addresses — bare IPs or
  CIDR prefixes (`source_ips: ["10.0.0.2", "10.0.0.16/28", "fd00::/120"]`,
  expansion capped at 4096 addresses). The ephemeral source-port space is
  per (source IP, dst IP, dst port), so multiple source addresses multiply
  the ~64k-concurrent-connections-per-endpoint budget of one machine. VU `i`
  is pinned to `ips[(i-1) % n]` (stable across iterations, keep-alive stays
  warm); clients are built lazily per (shard, address) pair, the first
  request logs how many addresses are active, and `perfscale lint` validates
  the list before the run.
- Typed run inputs (issue #11): config files declare an `inputs:` block
  (`number` with `min`/`max`, `string`, `bool` with 0/1/false/true coercion,
  GitHub-style `choice` with `options:`), steps interpolate
  `${{ inputs.<name> }}` everywhere `${{ ... }}` works, and the CLI overrides
  values at launch: `-i users=10 -i env=prod`, the comma form
  `-i users=10,env=prod` (`\,` escapes a literal comma), and `-e file.env`
  env files. Precedence: `-i` > env file > `default:`; unknown keys, type
  mismatches, out-of-range values and missing required inputs fail before
  the run starts, and `perfscale lint` prints the effective resolved values.
- Config extensions (issue #10): WASM libraries built against
  `perfscale:library@0.3.0` may export `config-schema`, claiming one
  top-level config key (`mylibrary: { ... }`) with a JSON Schema for its
  value. The engine validates the block at lint and run start and hands it
  to the guest via the run settings' `extensions` object. Unknown top-level
  keys stay a hard error (the typo guard moved from schema parse to this
  validation), and two libraries claiming one key is a hard error. Older
  (0.1/0.2) components keep loading unchanged.
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
