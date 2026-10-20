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

## SQL steps: up to 9× more queries per second

- The `std/db-*@v1` hot path was rebuilt around how load scenarios actually
  behave. `std/db-connect@v1` gains `scope: "vu"` — the connection outlives
  its iteration and a repeated connect with the same config reuses it
  (~0 ms), giving the k6/locust "one warm connection per VU" shape.
  `prepared: true` enables the statement cache for one network round trip
  per repeat query (opt-in: incompatible with transaction-mode poolers like
  PgBouncer). `std/db-query@v1` gains `store_rows: false` to count rows
  without materializing them as JSON.
- The default path is faster with no config changes too: the pool connection
  is held across autocommit queries instead of re-acquired per query (that
  bookkeeping alone was ~half of per-query wall time), placeholder-free
  params are no longer cloned, and quiet runs skip log formatting entirely.
- Measured on the bench db suite (10 VUs, `SELECT 1`, PostgreSQL): cold
  303 → 965 QPS, warm pool 870 → ~20 300 QPS — ahead of k6 with the xk6-sql
  extension (~18 100 QPS) on the same machine. SQLite warm: 67k → 213k QPS.

## `perfscale serve --ui`: dashboard rebuilt on React + shadcn/ui

- The local dashboard is now a proper React app (shadcn/ui components,
  icons, cross-links into the docs) instead of a hand-rolled static page,
  still embedded into a single self-contained HTML — no server-side
  frontend needed. The bundle is verified in CI to always match its
  sources.

## Development builds carry a real version

- Builds between releases report `--version` as `0.25.0-next.N+g<sha>`
  (distance from the last tag + commit) instead of a bare `next`, so test
  rigs and bug reports can tell exactly which build is installed. Tagged
  releases still report the plain version.

## Benchmark suite: boundary ladder, SQL rows, run-to-run compare

- The boundary suite escalates through a VU ladder (5k/15k/30k, duration
  configurable per run) and reports the achieved load per rung instead of a
  bare "never crossed". The db suite gained warm-pool/transaction/SQLite
  rows plus competitor rows (locust/psycopg2, JMeter/JDBC, k6 with a
  purpose-built xk6-sql binary), and every run now diffs against the
  previous one with a ±15% flag table.

## `perfscale self-update`: reliable downloads on slow networks

- Self-update no longer fails 44 MiB downloads on slow links with a cryptic
  "error decoding response body": the total 120 s timeout is gone (connect
  10 s + 60 s per-read stall limit instead), dropped connections are retried
  up to 3 times with backoff, a terminal shows a live `x.x/43.9 MiB`
  progress line, and failures explain themselves ("network stalled" /
  "connection dropped mid-download (received 21.4/43.9 MiB)").

## `perfscale self-update`: interactive by default

- `perfscale self-update` now asks for confirmation before downloading and
  replacing the binary (`Update perfscale 0.24.0 → 0.25.0? [Y/n]` — Enter
  means yes, `n` cancels with exit code 0). For scripts and CI pass
  `--non-interactive`; without it, a non-terminal stdin fails with a hint
  instead of hanging on a prompt nobody can answer. `--check` stays
  read-only and never asks.

## `perfscale cache clean`: reclaim the import/library cache

- The import/library cache (`~/.cache/perfscale`, overridable with
  `PERFSCALE_CACHE_DIR`) grew without bound — every fetched import and
  compiled WASM library stayed forever. `perfscale cache clean` sweeps it,
  printing the reclaimed size; `--dry-run` shows what would go. Everything
  is content-addressed and re-downloadable from a pinned `perfscale.lock`,
  so cleaning is always safe — worst case the next run re-fetches.

## Bounded HTTP connection pools

- Idle keep-alive connections are now capped (64 per host per client shard)
  and expire after 60 s idle instead of living for the whole run. High-VU
  runs against many hosts no longer hold thousands of idle sockets open on
  the target; warm connections survive any iteration cadence under a minute.

## Pure WASM libraries run inline

- Libraries that declare `"pure": true` (no capability hostcalls) now execute
  inline on current-thread runtimes instead of hopping to the blocking pool —
  no thread handoff, no channel wait. Capability libraries keep the hop
  (their sync hostcalls need it); multi-thread runtimes are unchanged.
