# GraphQL load testing

perfscale drives **GraphQL** endpoints under load with the native step engine:
one operation per step, queries and mutations over HTTP POST (or opt-in GET),
with the query validated against the endpoint's own schema before it is ever
sent.

No codegen and no client stubs: the document is plain text in the YAML (or a
`.graphql` file), variables are JSON, and responses come back as structured
`data`/`errors` the next steps extract from with `${{ … }}` interpolation.

This page is the guide. Per-step parameters and outputs live in the
[actions reference](actions.md#stdgraphqlv1); a runnable scenario ships as
[`examples/graphql.test.yaml`](../../examples/graphql.test.yaml).

## One action: `std/graphql@v1`

```yaml
steps:
  - name: fetch viewer
    use: std/graphql@v1
    with:
      url: https://api.example.com/graphql
      query: |
        query GetViewer($id: ID!) {
          viewer(id: $id) { id name }
        }
      variables: { "id": "${{ vars.viewer_id }}" }
    check:
      status: 200
    outputs: viewer

  - name: rename widget
    use: std/graphql@v1
    with:
      url: https://api.example.com/graphql
      query: |
        mutation Rename($id: ID!, $name: String!) {
          renameWidget(id: $id, name: $name) { id }
        }
      variables: { "id": "${{ viewer.data.viewer.id }}", "name": "w-${seq}" }
```

- `query` is inline (multiline YAML reads well); `query_file` points at a
  `.graphql` file instead — the two are mutually exclusive.
- `variables` is a JSON object. `${{ … }}` interpolation applies everywhere in
  `with:`, so values extracted from earlier steps flow straight in, and
  single-brace `${…}` generator tokens (`${uuid}`, `${rand}`, `${now}`)
  expand per execution — every iteration can send a fresh name or id.
- A document with several operations needs `operation: GetViewer` to pick
  one — the same `operationName` the server receives.
- `method: GET` moves the operation into URL query parameters for
  CDN-cacheable reads. Mutations stay on POST (the default).

## Schema validation

Every query is parsed before it is sent, and — when a schema is available —
validated against it. A typo fails the step (and `perfscale lint`) with a
did-you-mean suggestion instead of burning requests against the target:

```text
fetch viewer: query validation failed: unknown field 'viewr' on type 'Query' — did you mean 'viewer'?
```

Two schema sources:

- **Introspection** (default) — the engine POSTs an introspection query to the
  endpoint once per run, caches the schema process-wide, and validates every
  query against it. One round trip per endpoint, no matter how many VUs.
- **`schema_file: schema.graphql`** — validate against a local SDL file.
  This is the fallback for endpoints that refuse introspection (common in
  production): when introspection fails and no `schema_file` is given, the
  step still runs — unvalidated, with one `[sys]` log line saying so.
  `introspection: false` opts out of fetching entirely.

`perfscale lint` applies the same gate: syntax always (offline), schema when
the endpoint is reachable or an SDL file is given. `perfscale lint --offline`
skips the network pass.

## What counts as failure

GraphQL errors travel in a `200 OK` body, so HTTP status alone is not the
verdict:

- HTTP status ≥ 400 → the step fails.
- `errors` present, `data` null/absent → the step fails (nothing resolved).
- partial `data` **plus** `errors` → the step **passes** — the server resolved
  what it could — and the errors are counted in `graphql_errors`.

Standard `check:` assertions work unchanged: `status`, `duration_ms_lt`,
`body_contains` (against the raw body). Field-level assertions read the
decoded payload via a `std/check@v1` step:

```yaml
  - name: viewer id present
    use: std/check@v1
    with:
      on: viewer.data.viewer
      message_contains: "id"   # …or assert via outputs interpolation
```

## Metrics

- `graphql_req_duration` — histogram of every operation's round trip; the
  runner derives `graphql_req_failed` (rate). Gate on it with
  `std/thresholds@v1`: `"graphql_req_duration": ["p99<200"]`.
- `graphql_errors` — counter of GraphQL-level errors, including the
  partial-data ones that pass the step.
- `graphql_op_<operationName>_duration` — per-operation histogram, emitted
  only when the operation is named (explicit `operation` or a single named
  operation), so metric cardinality stays bounded by the test definition.
- The request also feeds the standard `http_req_duration` / `http_req_failed`
  / `http_reqs` aggregates, like any HTTP step.

## Connection pooling

`pool: per-vu` (default) pins the step to the VU's HTTP client shard — the
same keep-alive behaviour as `std/http@v1`, so a VU reuses its warm
connections across iterations. `pool: shared` puts every VU on one
process-global client: maximal connection reuse against a single endpoint, at
the cost of pool-lock contention under very high VU counts.

## Limits

- Subscriptions are not supported (the transport is HTTP request/response);
  a document containing one fails validation.
- Request batching (arrays of operations) and incremental delivery
  (`@defer`/`@stream`) are not supported — one operation per step.
- `query_file` and `schema_file` are filesystem access: they require
  `allow_file_actions` in the run config and honour `fs_root` confinement.
