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

- Faster startup and lint: the JSON schemas that validate `config.yaml` and test files are now compiled once per process instead of on every parse — `perfscale run` / `perfscale lint` validation is ~10–100× faster, and newly added config fields no longer make every parse slower.
- **Libraries (RFC 005, phase 1)**: pluggable value generators for `${...}` tokens. Declare `libraries: - use: "@std/random@v1"` and call functions as `${random.ulid()}` in any payload — the built-in library ships real uuid v4/v7, ULID, nanoid, weighted picks, named sequences, patterns (`ORD-####-????`), faker basics (names/emails/companies) and dates. An optional trailing key argument reuses one generated value across fields of the same message (`${random.uuid4(order)}` for FIX-style id echoes), and `seed:` in the config makes every generated value in a run reproducible.
- `${...}` generator tokens now expand everywhere, not just ws/gRPC/GraphQL: `std/http` (URL, headers, body), raw TCP/UDP, LLM prompts, pub/sub payloads, file-write content, and DB bind parameters (never the SQL text). Unknown tokens are still left verbatim, but a token that resolves to a declared library and fails now fails the step instead of silently sending raw `${...}` text.
- `perfscale lint` validates `${alias.fn(...)}` tokens against declared libraries (unknown function = error with did-you-mean; unknown alias = warning), and the run summary/export gains an additive `libraries:` section with per-alias calls/errors/p95 so a slow library is visible instead of hiding inside step latency.
