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

- Built-in `html` reporter (`std/report/html@v1`, `--report html:report.html`):
  a single self-contained dark-themed page with the run identity, threshold
  verdicts, a sortable per-step table with samples sparklines, and metric
  families — all CSS/JS inlined, opens offline, archives as a CI artifact.
