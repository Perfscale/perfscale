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

## Database load testing — `std/db-*@v1`

Load-test **PostgreSQL**, **MySQL/MariaDB**, and **SQLite** directly from
perfscale scenarios — free for everyone, like the HTTP, WebSocket, and gRPC
families.

- `std/db-connect@v1` opens a connection pool (`driver`, `dsn`, `tls`,
  `mode: persistent|per-query`, `pool_size`, `timeout_ms`). The `per-query`
  mode opens a fresh connection per query and measures connect+query as one
  unit — the shape of serverless/edge workloads.
- `std/db-query@v1` runs parameterized queries with driver-native
  placeholders (`$1` on Postgres, `?` elsewhere). The SQL text is **never
  interpolated** — values move through bind `params` only, so scenarios can't
  inject themselves. `max_rows` caps result sets (10k by default), query text
  is limited to 64 KiB, and the query timeout is configurable.
- `std/db-tx-begin@v1` / `std/db-tx-commit@v1` / `std/db-tx-rollback@v1`
  measure multi-statement transactions as a single unit.
- New `db_*` metrics: `db_connect_duration`, `db_query_duration`, `db_rows`,
  and `db_errors` split by class (`connection`, `constraint`, `deadlock`,
  `timeout`, `other`) — step names are the only labels, raw SQL never appears
  in metrics, and DSNs/passwords are sanitized out of error messages.

Guides and recipes (read-only analytics, write-heavy inserts, money-transfer
transactions, serverless per-query mode, MySQL variants, SQLite zero-setup,
mixed HTTP+DB scenarios) are in the **Database testing** page of the docs,
and `examples/db-sqlite.test.yaml` runs out of the box with no server.
