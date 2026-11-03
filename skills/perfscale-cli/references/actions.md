# Actions catalog (`std/*`)

Every step is `use: <action>@v1` with action-specific `with:` inputs, optional
`check:` assertions, and `outputs:` bindings. Short aliases drop the `std/`
prefix and `@v1` suffix (`use: http`).

## HTTP / API

- `std/http@v1` — one HTTP request. `with: { method, url, headers?, body? }`,
  result has `status`, `headers`, `body`, `duration_ms`.
- `std/graphql@v1` — GraphQL query/mutation; the endpoint's introspected
  schema validates the document before sending.
- `std/grpc@v1`, `std/grpc-connect@v1`, `std/grpc-call@v1`,
  `std/grpc-stream-open@v1`, `std/grpc-stream-send@v1`,
  `std/grpc-stream-recv@v1`, `std/grpc-stream-close@v1` — unary and streaming
  gRPC with server reflection.

## WebSocket

- `std/ws@v1` — one-shot connect/send/close.
- `std/ws-connect@v1`, `std/ws-send@v1`, `std/ws-recv@v1`, `std/ws-ping@v1`,
  `std/ws-close@v1` — persistent connection across steps via `outputs:` id.

## Databases

- `std/db-connect@v1` — open a pool (`driver: sqlite|postgres|mysql`, `dsn`).
- `std/db-query@v1` — run a query; values move through `params` (never
  interpolated into SQL text).
- `std/db-tx-begin@v1`, `std/db-tx-commit@v1`, `std/db-tx-rollback@v1` —
  transactions.
- `std/db-close@v1` — hang up.

## Messaging

- `std/pubsub@v1` — publish/subscribe primitives (see references/protocols.md).

## Flow & utility

- `std/check@v1` — standalone assertions over bound variables.
- `std/sleep@v1` — `with: { ms }` pacing/think time.
- `std/log@v1` — `with: { message }` (interpolated).
- `std/random@v1` — random values for test data.
- `std/pick@v1` — pick from a list (data-driven scenarios).
- `std/file-read@v1`, `std/read-json@v1`, `std/file-write@v1` — file I/O.
- `std/child_process@v1`, `std/kill_process@v1` — manage helper processes.
- `std/set_shared_variable@v1`, `std/get_shared_variable@v1` — cross-VU shared state.
- `std/thresholds@v1` — run-level SLO gate (see references/thresholds.md).
- `std/tcp@v1`, `std/udp@v1` — raw sockets.
- `std/llm@v1` — LLM endpoint probing.

Custom actions come from WASM libraries (`imports:`) — see the
`perfscale-library-sdk` skill.
