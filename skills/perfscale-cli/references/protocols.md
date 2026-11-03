# Protocols

## HTTP

`std/http@v1`: `with: { method, url, headers?, body? }`. Check `status`,
assert on `body` with `check:`, bind `outputs:` for extraction
(`${{ resp.body.token }}` chains into later steps).

## GraphQL

`std/graphql@v1` sends a query/mutation to one endpoint. The endpoint's
introspected schema validates every document before it is sent (cached once
per run); a typo'd field fails the step — and `perfscale lint` — before any
request.

## gRPC

Unary: `std/grpc@v1`. Streaming: open with `std/grpc-connect@v1`, then
`std/grpc-call@v1` or `std/grpc-stream-open/send/recv/close@v1` using the
connection id from `outputs:`. Server reflection resolves methods.

## WebSocket

One-shot: `std/ws@v1`. Persistent: `std/ws-connect@v1` (binds a connection id
via `outputs:`), then `std/ws-send@v1`, `std/ws-recv@v1`, `std/ws-ping@v1`,
`std/ws-close@v1`.

## SQL databases

`std/db-connect@v1` (`driver: sqlite|postgres|mysql`, `dsn`) holds a pool
across steps; `std/db-query@v1` runs queries with driver-native placeholders
in `params` (SQL text is never interpolated); `std/db-tx-begin/commit/
rollback@v1` for transactions; `std/db-close@v1` to hang up. SQLite needs no
server (`dsn: "sqlite::memory:"`) and is ideal for examples.

## Pub/Sub

`std/pubsub@v1` covers publish/subscribe flows. Asynchronous publish errors
are reported in the run summary.

## Raw TCP/UDP

`std/tcp@v1`, `std/udp@v1` for socket-level checks.

## LLM endpoints

`std/llm@v1` for probing LLM inference endpoints (latency/throughput of
completions).
