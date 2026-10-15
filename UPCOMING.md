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
