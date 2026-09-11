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

- **During-run metrics: points are no longer dropped at thresholds.** The
  CPU gate now queues incoming snapshots instead of dropping them (flushed
  in arrival order once the machine recovers), and `max_pending` turned
  from a drop-oldest cap into a soft warn threshold — undelivered batches
  are kept for the whole run and delivered in order. The only remaining
  mid-run shed is the engine's bounded snapshot channel (VU-loop
  protection), and the bounded final drain still applies when the target is
  down at run end.
