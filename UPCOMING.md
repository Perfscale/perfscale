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

- **GraphQL load testing** — new `std/graphql@v1` action drives queries and
  mutations against any GraphQL endpoint: inline documents or `query_file`,
  JSON `variables` with full `${{ … }}` interpolation, POST by default with
  opt-in GET for CDN-cacheable reads. See [docs/core/graphql.md](docs/core/graphql.md)
  and the runnable [examples/graphql.test.yaml](examples/graphql.test.yaml).
- **Schema-aware validation** — queries are validated against the endpoint's
  introspected schema (or a local `schema_file` SDL) before they are sent;
  typos fail fast with did-you-mean suggestions instead of burning requests.
  `perfscale lint` applies the same gate; `--offline` skips the network pass.
- **GraphQL-aware failure semantics and metrics** — `errors` with no `data`
  fails the step, partial data passes and is counted; new
  `graphql_req_duration`, `graphql_errors`, and per-operation
  `graphql_op_<name>_duration` metrics feed thresholds alongside the standard
  HTTP aggregates.
