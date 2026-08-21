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

- **Docs: dedicated Pub/Sub load-testing guide** — `std/pubsub@v1` gets its
  own page under Core (`docs/core/pubsub.md`): drivers, roundtrip /
  producer-only / consumer / producer-consumer pair patterns, metrics and
  thresholds, connection posture.
