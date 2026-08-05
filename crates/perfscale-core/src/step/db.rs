//! Database actions — PostgreSQL, MySQL/MariaDB, and SQLite over `sqlx`.
//!
//! | Action ID               | What it does                                       |
//! |-------------------------|----------------------------------------------------|
//! | `std/db-connect@v1`     | Open a pool (or store a per-query profile), return its id |
//! | `std/db-query@v1`       | Run a parameterized query on a parked connection   |
//! | `std/db-tx-begin@v1`    | Begin a transaction on a persistent connection     |
//! | `std/db-tx-commit@v1`   | Commit the open transaction                        |
//! | `std/db-tx-rollback@v1` | Roll back the open transaction                     |
//! | `std/db-close@v1`       | Close a parked connection and release its id       |
//!
//! # Modes: persistent vs per-query
//!
//! In **`persistent`** mode (the default) `db-connect` opens a connection
//! pool (`pool_size`, default 1 — steps within a VU run strictly
//! sequentially, so one connection is all a scenario can drive) and parks it
//! under a Connection ID (`db-1`, …); later steps address it via
//! `id: "${{ conn.id }}"`. Whatever a scenario leaves open is dropped at
//! iteration end.
//!
//! In **`per-query`** mode the connect step only parses and stores the
//! connect config — no connection is opened. Every `db-query` then opens a
//! fresh connection, runs the query, and closes, so `db_query_duration`
//! measures connect + query as one unit (the shape of serverless/edge
//! workloads, and of connect-time benchmarking). Transactions are rejected
//! on per-query ids. Note that per-query against `sqlite::memory:` is
//! pointless (each fresh connection sees an empty database) — use a file
//! DSN. Likewise an in-memory SQLite pool must stay at `pool_size: 1` (the
//! default): a second pooled connection would be a *different* database.
//!
//! # Connection poolers (PgBouncer, Supabase/Supavisor)
//!
//! Every statement executes **unnamed** ([`step_query`] sets
//! `persistent(false)`), so a transaction-mode pooler never trips over a
//! cached named statement on the shared backend (`prepared statement
//! "sqlx_s_1" already exists`). sqlx still flushes Parse+Sync and
//! Bind+Execute+Sync as separate batches, and such a pooler may reassign
//! the backend at the Sync under concurrency; when that happens
//! ([`is_pg_pooler_split`] — always a pre-execution error, so the statement
//! provably never ran), the query retries once inside BEGIN/COMMIT, which
//! pins the backend, and the connection wraps its remaining autocommit
//! queries the same way (`DbConn::wrap_tx`). Direct connections never pay
//! for this. For Supabase: use the pooler DSN (port 6543, IPv4) with
//! `tls: "skip-verify"` (their CA chain is not in webpki-roots) or install
//! their CA.
//!
//! # SQL and bind parameters
//!
//! `query` is SQL text with **driver-native placeholders** (`$1`, `$2`, …
//! for PostgreSQL, `?` for MySQL/MariaDB and SQLite) and is **never
//! interpolated** — a `${{` sequence inside the SQL text is passed to the
//! database verbatim.
//! Values move through `params`, a positional array; each entry *is*
//! interpolated by the engine's usual value interpolation, then bound:
//! strings → text, numbers → i64/f64, booleans → bool, null → a typed NULL
//! (text-typed on PostgreSQL — cast in SQL, e.g. `$1::int`, when inserting
//! into a non-text column). Arrays/objects cannot be bound. INSERT, UPDATE,
//! DELETE, and DDL are allowed by default. The SQL text is capped at 64 KiB
//! (`max_query_bytes`, a hard limit). One `query` may carry several
//! `;`-separated statements: they run in order over a single stream,
//! `rows_affected` sums across them, and SELECT rows from every statement
//! collect into `data` (up to `max_rows`).
//!
//! # Rows
//!
//! SELECT rows are returned as JSON objects (`data`) up to `max_rows`
//! (default 10000 — a hard cap on rows read into memory; the row that would
//! exceed the cap only sets `truncated: true`, it is not collected). Column
//! mapping: booleans, integers, floats, text, JSON, and binary (base64) are
//! decoded; anything else — NUMERIC/DECIMAL, temporal types, UUID, arrays
//! (no chrono/time/decimal features are compiled in) — surfaces as `null`,
//! and the row still counts. MySQL `TINYINT` (its boolean spelling)
//! surfaces as a number; SQLite values decode by storage class.
//!
//! `rows_affected` follows driver semantics: on SQLite a SELECT reports the
//! *previous* DML statement's change count (a `sqlite3_changes()` quirk),
//! so read `rows`/`db_rows` as the headline number.
//!
//! # Metrics
//!
//! Emitted via the reserved `metrics` output key (see `step/grpc.rs`):
//!
//! - `db_connect_duration` (histogram, ms) — successful `db-connect`.
//! - `db_query_duration` (histogram, ms) — every `db-query` and transaction
//!   step; in per-query mode it includes the fresh connect.
//! - `db_rows` (counter) — rows returned per `db-query`, or rows affected
//!   when the statement returned none.
//! - `db_errors` (counter) — every failed DB step, plus one classified
//!   counter per kind: `db_errors_connection`, `db_errors_constraint`,
//!   `db_errors_deadlock`, `db_errors_timeout`, `db_errors_other`.
//!   Classification maps sqlx error kinds and SQLSTATE class (PostgreSQL),
//!   errno (MySQL/MariaDB), or result code (SQLite). The failing step output
//!   also carries `error_kind` with the same class. Successful DB steps emit
//!   `db_errors: 0` explicitly, so the counter exists (at 0) even on fully
//!   healthy runs — `std/thresholds@v1` gates like `db_errors: ["count==0"]`
//!   can assert a clean run instead of erroring on an unknown metric.
//!
//! Step-name labels only — raw SQL never appears in metrics.
//!
//! # Secrets
//!
//! The DSN (which usually embeds a password) is never logged: log lines use
//! a sanitized `host:port/database` label built from the parsed connect
//! options, and error details pass through [`sanitize_detail`], which
//! scrubs the DSN and its password.
//!
//! # Server-backed integration tests
//!
//! The unit/SQLite tests below run anywhere. PostgreSQL/MySQL flows are
//! gated behind env vars so CI without Docker stays green:
//!
//! ```sh
//! docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=perfscale postgres:16
//! docker run --rm -d -p 3306:3306 -e MYSQL_ROOT_PASSWORD=perfscale -e MYSQL_DATABASE=perfscale mariadb:11
//! PERFSCALE_TEST_PG_DSN=postgres://postgres:perfscale@127.0.0.1:5432/postgres \
//! PERFSCALE_TEST_MYSQL_DSN=mysql://root:perfscale@127.0.0.1:3306/perfscale \
//!   cargo test -p perfscale-core db::
//! ```

use std::str::FromStr;
use std::time::Instant;

use futures_util::TryStreamExt as _;
use serde_json::{json, Value};
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlRow, MySqlSslMode};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgRow, PgSslMode};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Column as _, ConnectOptions as _, Row as _, TypeInfo as _, ValueRef as _};
use tokio::time::Duration;

use super::actions::{err, error_chain, ActionOutput, LogTag};
use super::context::Context;
use super::resources::{DbConn, DbDriver, DbPool, DbProfile, DbState, DbTx};
use super::ws::u64_param;

/// Default step timeout for every DB action (`timeout_ms`).
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Default row cap for `std/db-query@v1` (`max_rows`).
const DEFAULT_MAX_ROWS: u64 = 10_000;
/// Hard limit on the SQL text length (`max_query_bytes`) — a step is a
/// query, not a dump tool.
const MAX_QUERY_BYTES: usize = 64 * 1024;
/// Default pool size in persistent mode (see the module doc's Modes section).
const DEFAULT_POOL_SIZE: u64 = 1;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// TLS policy for `db-connect` (`tls` parameter). Ignored for sqlite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsMode {
    /// Full certificate + hostname verification (the default).
    Verify,
    /// Encrypt, but accept any certificate (self-signed staging only).
    SkipVerify,
    /// Plaintext.
    Off,
}

/// Accept `true` / `"true"` / `false` / `"false"` / `"skip-verify"` —
/// interpolated `${{ … }}` values are always strings, so bool params must
/// take both forms (same rationale as `ws::bool_param`).
fn parse_tls(v: &Value) -> Result<TlsMode, String> {
    match v {
        Value::Bool(true) => Ok(TlsMode::Verify),
        Value::Bool(false) => Ok(TlsMode::Off),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Ok(TlsMode::Verify),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Ok(TlsMode::Off),
        Value::String(s) if s.eq_ignore_ascii_case("skip-verify") => Ok(TlsMode::SkipVerify),
        _ => Err("'tls' must be true, false, or \"skip-verify\"".into()),
    }
}

/// Validated `std/db-connect@v1` parameters.
struct ConnectParams {
    driver: DbDriver,
    dsn: String,
    tls: TlsMode,
    per_query: bool,
    timeout_ms: u64,
    pool_size: u64,
}

fn parse_connect_params(params: &Value) -> Result<ConnectParams, String> {
    let driver = match params.get("driver").and_then(Value::as_str) {
        Some("postgres") => DbDriver::Postgres,
        Some("mysql") => DbDriver::MySql,
        Some("sqlite") => DbDriver::Sqlite,
        Some(other) => {
            return Err(format!(
                "unknown driver '{other}' — expected postgres, mysql, or sqlite"
            ))
        }
        None => return Err("'driver' is required (postgres, mysql, or sqlite)".into()),
    };
    let dsn = params
        .get("dsn")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("'dsn' is required (a driver-native connection string)")?
        .to_string();
    let tls = params.get("tls").map(parse_tls).transpose()?.unwrap_or(TlsMode::Verify);
    let per_query = match params.get("mode") {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) if s == "persistent" => false,
        Some(Value::String(s)) if s == "per-query" => true,
        Some(_) => return Err("'mode' must be persistent or per-query".into()),
    };
    let timeout_ms = params
        .get("timeout_ms")
        .map(|v| u64_param(v, DEFAULT_TIMEOUT_MS))
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let pool_size = params
        .get("pool_size")
        .map(|v| u64_param(v, DEFAULT_POOL_SIZE))
        .unwrap_or(DEFAULT_POOL_SIZE)
        .clamp(1, 1024);
    Ok(ConnectParams {
        driver,
        dsn,
        tls,
        per_query,
        timeout_ms,
        pool_size,
    })
}

/// Validated `std/db-query@v1` parameters (borrows from the step params).
struct QuerySpec<'a> {
    sql: &'a str,
    binds: &'a [Value],
    max_rows: usize,
    timeout_ms: u64,
}

fn parse_query_spec(params: &Value) -> Result<QuerySpec<'_>, String> {
    let sql = params
        .get("query")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or("'query' is required (SQL text with driver-native placeholders)")?;
    if sql.len() > MAX_QUERY_BYTES {
        return Err(format!(
            "'query' is {} bytes — over the {} KiB hard limit (max_query_bytes)",
            sql.len(),
            MAX_QUERY_BYTES / 1024
        ));
    }
    let binds: &[Value] = match params.get("params") {
        None | Some(Value::Null) => &[],
        Some(Value::Array(a)) => a,
        Some(_) => return Err("'params' must be an array of positional bind values".into()),
    };
    let max_rows = params
        .get("max_rows")
        .map(|v| u64_param(v, DEFAULT_MAX_ROWS))
        .unwrap_or(DEFAULT_MAX_ROWS) as usize;
    let timeout_ms = params
        .get("timeout_ms")
        .map(|v| u64_param(v, DEFAULT_TIMEOUT_MS))
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    Ok(QuerySpec {
        sql,
        binds,
        max_rows,
        timeout_ms,
    })
}

/// Step timeout shared by the transaction and close steps.
fn timeout_ms_param(params: &Value) -> u64 {
    params
        .get("timeout_ms")
        .map(|v| u64_param(v, DEFAULT_TIMEOUT_MS))
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

// ---------------------------------------------------------------------------
// Secrets: DSN sanitization
// ---------------------------------------------------------------------------

/// Extract the password portion of a DSN (`scheme://user:password@host/…`),
/// if any. The userinfo ends at the *last* `@` before the host (passwords
/// may themselves contain `@`); sqlite DSNs carry no userinfo at all.
fn dsn_password(dsn: &str) -> Option<&str> {
    let after_scheme = dsn.split_once("://")?.1;
    let (userinfo, _) = after_scheme.rsplit_once('@')?;
    if userinfo.contains('/') {
        return None;
    }
    match userinfo.split_once(':') {
        Some((_, password)) if !password.is_empty() => Some(password),
        _ => None,
    }
}

/// Scrub the DSN and its password out of an error detail string. sqlx
/// messages do not embed the DSN today, but a driver change must never leak
/// credentials into a log line.
fn sanitize_detail(detail: &str, dsn: &str) -> String {
    let mut out = detail.replace(dsn, "[dsn]");
    if let Some(password) = dsn_password(dsn) {
        out = replace_whole_word(&out, password, "[redacted]");
    }
    out
}

/// Replace whole-word occurrences of `needle` with `replacement`. A match
/// counts as a word only when bounded by a non-alphanumeric character or a
/// string edge — so scrubbing a one-character password cannot mangle
/// unrelated words ("open" must not become "o[redacted]en"), while a
/// password echoed on its own ("authentication failed for 's3cret'") is
/// still scrubbed.
fn replace_whole_word(haystack: &str, needle: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(pos) = rest.find(needle) {
        let after = &rest[pos + needle.len()..];
        let bounded = rest[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric())
            && after.chars().next().is_none_or(|c| !c.is_alphanumeric());
        if bounded {
            out.push_str(&rest[..pos]);
            out.push_str(replacement);
        } else {
            out.push_str(&rest[..pos + needle.len()]);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

/// Classify a sqlx error into the `db_errors` kinds: `connection`,
/// `constraint`, `deadlock`, `timeout`, `other`.
fn classify(e: &sqlx::Error, driver: DbDriver) -> &'static str {
    match e {
        sqlx::Error::Database(db_err) => {
            classify_db_error(db_err.kind(), db_err.code().as_deref(), driver)
        }
        sqlx::Error::PoolTimedOut => "timeout",
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => "connection",
        _ => "other",
    }
}

/// Map a database error to its class. sqlx's own `kind()` already pins the
/// constraint classes; the driver-specific codes (PostgreSQL SQLSTATE,
/// MySQL/MariaDB errno, SQLite result code) fill in deadlock/timeout/
/// connection where `kind()` is `Other`.
fn classify_db_error(
    kind: sqlx::error::ErrorKind,
    code: Option<&str>,
    driver: DbDriver,
) -> &'static str {
    use sqlx::error::ErrorKind as K;
    match kind {
        K::UniqueViolation | K::ForeignKeyViolation | K::NotNullViolation | K::CheckViolation => {
            return "constraint"
        }
        // `Other` now, plus anything a future sqlx adds (non_exhaustive).
        _ => {}
    }
    let Some(code) = code else { return "other" };
    match driver {
        DbDriver::Postgres => {
            // SQLSTATE classes: 08 = connection exception, 23 = integrity
            // constraint violation.
            if code.starts_with("08") {
                return "connection";
            }
            if code.starts_with("23") {
                return "constraint";
            }
            match code {
                "40001" | "40P01" => "deadlock", // serialization failure / deadlock detected
                "57014" => "timeout",            // query canceled (e.g. statement_timeout)
                _ => "other",
            }
        }
        DbDriver::MySql => match code {
            "1062" | "1451" | "1452" | "1048" | "4025" => "constraint", // dup key, FK x2, not-null, MariaDB check
            "1213" => "deadlock",                                       // ER_LOCK_DEADLOCK
            "1205" => "timeout",                                        // ER_LOCK_WAIT_TIMEOUT
            "1040" | "1042" | "1043" | "1044" | "1045" | "1129" => "connection",
            _ => "other",
        },
        DbDriver::Sqlite => match code.parse::<u32>() {
            // Extended result codes keep the primary code in the low byte.
            Ok(n) => match n & 0xFF {
                5 | 6 => "deadlock", // BUSY / LOCKED (lock contention)
                19 => "constraint",  // CONSTRAINT* (1555 unique, 787 FK, 1299 not-null, …)
                _ => "other",
            },
            Err(_) => "other",
        },
    }
}

// ---------------------------------------------------------------------------
// Connect options per driver (+ password-free log target)
// ---------------------------------------------------------------------------

/// Parse the DSN into PostgreSQL connect options and apply the TLS policy.
/// Returns the options plus a sanitized `host:port/database` label.
fn pg_options(cp: &ConnectParams) -> Result<(PgConnectOptions, String), String> {
    let opts = PgConnectOptions::from_str(&cp.dsn).map_err(|e| {
        format!(
            "invalid dsn for driver 'postgres': {}",
            sanitize_detail(&error_chain(&e), &cp.dsn)
        )
    })?;
    let opts = opts.ssl_mode(match cp.tls {
        TlsMode::Verify => PgSslMode::VerifyFull,
        TlsMode::SkipVerify => PgSslMode::Require,
        TlsMode::Off => PgSslMode::Disable,
    });
    let target = match opts.get_database() {
        Some(db) => format!("{}:{}/{}", opts.get_host(), opts.get_port(), db),
        None => format!("{}:{}", opts.get_host(), opts.get_port()),
    };
    Ok((opts, target))
}

/// Parse the DSN into MySQL/MariaDB connect options (TLS policy applied).
fn mysql_options(cp: &ConnectParams) -> Result<(MySqlConnectOptions, String), String> {
    let opts = MySqlConnectOptions::from_str(&cp.dsn).map_err(|e| {
        format!(
            "invalid dsn for driver 'mysql': {}",
            sanitize_detail(&error_chain(&e), &cp.dsn)
        )
    })?;
    let opts = opts.ssl_mode(match cp.tls {
        TlsMode::Verify => MySqlSslMode::VerifyIdentity,
        TlsMode::SkipVerify => MySqlSslMode::Required,
        TlsMode::Off => MySqlSslMode::Disabled,
    });
    let target = match opts.get_database() {
        Some(db) => format!("{}:{}/{}", opts.get_host(), opts.get_port(), db),
        None => format!("{}:{}", opts.get_host(), opts.get_port()),
    };
    Ok((opts, target))
}

/// Parse the DSN into SQLite connect options (`tls` is ignored — SQLite is
/// a local file).
fn sqlite_options(cp: &ConnectParams) -> Result<(SqliteConnectOptions, String), String> {
    let opts = SqliteConnectOptions::from_str(&cp.dsn).map_err(|e| {
        format!(
            "invalid dsn for driver 'sqlite': {}",
            sanitize_detail(&error_chain(&e), &cp.dsn)
        )
    })?;
    let target = opts.get_filename().display().to_string();
    Ok((opts, target))
}

// ---------------------------------------------------------------------------
// Query execution (generic over the driver)
// ---------------------------------------------------------------------------

/// What one `std/db-query@v1` execution produced.
struct QueryOutcome {
    /// Rows collected into `data` (capped at `max_rows`).
    rows: u64,
    /// Sum of rows-affected across the statement's results (DML/DDL).
    rows_affected: u64,
    /// The result had more rows than `max_rows`.
    truncated: bool,
    data: Vec<Value>,
}

/// The query one step executes. Statements are always **non-persistent**
/// (unnamed): behind a transaction-mode pooler (PgBouncer, Supabase's :6543
/// pooler) the server-side backend changes underneath the client connection,
/// so a named statement cached per client connection collides on the shared
/// backend — `prepared statement "sqlx_s_1" already exists` on every query
/// after the first. Unnamed statements are re-parsed per execution, which
/// costs a Parse round trip and works everywhere: poolers, direct
/// connections, and (a no-op conceptually) mysql/sqlite.
fn step_query<'q, DB>(sql: &'q str) -> sqlx::query::Query<'q, DB, <DB as sqlx::Database>::Arguments<'q>>
where
    DB: sqlx::Database + sqlx::database::HasStatementCache,
{
    sqlx::query::<DB>(sql).persistent(false)
}

/// Bind `params`, run `sql`, and collect rows up to `max_rows`. Works on
/// every executor shape the family uses: a pool reference (persistent
/// queries), a live transaction, or a fresh connection (per-query mode).
///
/// `row_fn` / `rows_affected_fn` are per-driver because their traits
/// (`Row`, `QueryResult`) are not object-safe across drivers.
async fn run_query<'c, E, DB>(
    executor: E,
    sql: &'c str,
    params: &[Value],
    max_rows: usize,
    row_fn: fn(&DB::Row) -> Value,
    rows_affected_fn: fn(&DB::QueryResult) -> u64,
) -> Result<QueryOutcome, sqlx::Error>
where
    E: sqlx::Executor<'c, Database = DB>,
    DB: sqlx::Database + sqlx::database::HasStatementCache,
    for<'q> bool: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    for<'q> i64: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    for<'q> f64: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    for<'q> String: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    for<'q> Option<String>: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    for<'q> <DB as sqlx::Database>::Arguments<'q>: sqlx::IntoArguments<'q, DB>,
{
    let mut q = step_query::<DB>(sql);
    for (i, p) in params.iter().enumerate() {
        q = match p {
            // A typed NULL (text on PostgreSQL — see the module doc).
            Value::Null => q.bind(Option::<String>::None),
            Value::Bool(b) => q.bind(*b),
            Value::Number(n) => {
                if let Some(int) = n.as_i64() {
                    q.bind(int)
                } else if let Some(float) = n.as_f64() {
                    q.bind(float)
                } else {
                    return Err(sqlx::Error::Encode(
                        format!("bind param #{}: unsigned integer does not fit in i64", i + 1)
                            .into(),
                    ));
                }
            }
            Value::String(s) => q.bind(s.clone()),
            _ => {
                return Err(sqlx::Error::Encode(
                    format!("bind param #{}: arrays and objects cannot be bound", i + 1).into(),
                ))
            }
        };
    }

    // fetch_many streams both result-set rows and statement results, so one
    // path serves SELECT, DML, DDL, and multi-statement scripts alike. The
    // `Query::fetch_many` convenience wrapper is deprecated (over SQLite
    // multi-statement semantics, which a single parameterized statement
    // never hits; `RawSql`, the suggested replacement, cannot bind
    // parameters) — call the `Executor` trait method directly.
    let mut stream = executor.fetch_many(q);
    let mut rows_affected = 0u64;
    let mut data = Vec::new();
    let mut truncated = false;
    while let Some(item) = stream.try_next().await? {
        match item {
            sqlx::Either::Left(result) => rows_affected += rows_affected_fn(&result),
            sqlx::Either::Right(row) => {
                if data.len() >= max_rows {
                    // The cap is on rows read into memory: this row only
                    // flips the flag, it is not collected.
                    truncated = true;
                    break;
                }
                data.push(row_fn(&row));
            }
        }
    }
    let rows = data.len() as u64;
    Ok(QueryOutcome {
        rows,
        rows_affected,
        truncated,
        data,
    })
}

// ---------------------------------------------------------------------------
// Transaction-mode poolers (PgBouncer, Supabase's Supavisor)
// ---------------------------------------------------------------------------

/// SQLSTATEs a transaction-mode pooler produces when it splits one
/// extended-protocol exchange across backends: the Parse lands on one
/// backend, the Bind on another (sqlx flushes Parse+Sync and Bind+Execute+
/// Sync as separate batches, and a Sync is exactly where such a pooler may
/// reassign). All three fire BEFORE the statement executes, so re-running
/// the statement is exactly-once safe — even for writes.
///
/// - 42P05 duplicate_prepared_statement ("sqlx_s_1 already exists")
/// - 26000 invalid_sql_statement_name ("unnamed prepared statement does not
///   exist")
/// - 08P01 protocol_violation ("bind message supplies N parameters, but
///   prepared statement requires M")
fn is_pg_pooler_split(e: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db_err) = e else {
        return false;
    };
    matches!(
        db_err.code().as_deref(),
        Some("42P05") | Some("26000") | Some("08P01")
    )
}

/// One query inside BEGIN/COMMIT on a single acquired connection — the only
/// shape a transaction-mode pooler cannot split, because an open transaction
/// pins the backend until COMMIT/ROLLBACK. sqlx's own Transaction gives the
/// cleanup semantics (drop rolls back) for free.
async fn run_pg_wrapped<'c, A>(
    acq: A,
    sql: &str,
    params: &[Value],
    max_rows: usize,
) -> Result<QueryOutcome, sqlx::Error>
where
    A: sqlx::Acquire<'c, Database = sqlx::Postgres>,
{
    let mut tx = acq.begin().await?;
    let out = run_query(&mut *tx, sql, params, max_rows, pg_row_to_json, |r| {
        r.rows_affected()
    })
    .await;
    match out {
        Ok(o) => {
            tx.commit().await?;
            Ok(o)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// Autocommit query on a Postgres pool, learning pooler compatibility on
/// the fly. The returned flag tells the caller to route every later query
/// on this connection straight to the wrapped path.
///
/// Plain path first — one exchange, no distortion on direct connections. A
/// pooler split ([`is_pg_pooler_split`], always a pre-execution failure)
/// retries once inside BEGIN/COMMIT: an open transaction pins the backend,
/// so Parse and Bind cannot be separated. The retry is invisible in
/// metrics — the step simply succeeds. A genuine user error (a real
/// bind-count mismatch is also 08P01) fails the wrapped retry too: it is
/// reported unchanged and the flag stays off.
async fn run_pg_pool_autocommit(
    pool: &sqlx::PgPool,
    wrap_tx: bool,
    sql: &str,
    params: &[Value],
    max_rows: usize,
) -> (Result<QueryOutcome, sqlx::Error>, bool) {
    if !wrap_tx {
        let plain = run_query(pool, sql, params, max_rows, pg_row_to_json, |r| {
            r.rows_affected()
        })
        .await;
        match plain {
            Err(e) if is_pg_pooler_split(&e) => {} // fall through into the wrapped retry
            other => return (other, false),
        }
    }
    let out = run_pg_wrapped(pool, sql, params, max_rows).await;
    let learned = out.is_ok();
    (out, learned)
}

/// The per-query-mode twin: connect fresh for the plain attempt, and on a
/// pooler split retry on a NEW connection inside BEGIN/COMMIT (the split
/// connection's stream state cannot be trusted).
async fn run_pg_profile_autocommit(
    opts: &sqlx::postgres::PgConnectOptions,
    wrap_tx: bool,
    sql: &str,
    params: &[Value],
    max_rows: usize,
) -> (Result<QueryOutcome, sqlx::Error>, bool) {
    if !wrap_tx {
        let plain = match opts.connect().await {
            Ok(mut fresh) => {
                run_query(&mut fresh, sql, params, max_rows, pg_row_to_json, |r| {
                    r.rows_affected()
                })
                .await
            }
            Err(e) => Err(e),
        };
        match plain {
            Err(e) if is_pg_pooler_split(&e) => {} // retry wrapped, on a fresh connection
            other => return (other, false),
        }
    }
    let out = match opts.connect().await {
        Ok(mut fresh) => run_pg_wrapped(&mut fresh, sql, params, max_rows).await,
        Err(e) => Err(e),
    };
    let learned = out.is_ok();
    (out, learned)
}

// ---------------------------------------------------------------------------
// Row → JSON decoding
// ---------------------------------------------------------------------------

/// f64 → JSON, mapping non-finite values (NaN/±Inf — PostgreSQL allows
/// them) to null, which `serde_json::Number` cannot represent.
fn json_f64(f: f64) -> Value {
    serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
}

/// Standard base64 for binary cells (BYTEA/BLOB) — the same encoding
/// `std/http@v1` uses for `body_base64`.
fn base64_cell(bytes: &[u8]) -> Value {
    use base64::Engine as _;
    Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// A whole row as a JSON object keyed by column name.
fn row_to_json<R: sqlx::Row>(row: &R, cell: fn(&R, usize) -> Value) -> Value {
    let mut obj = serde_json::Map::with_capacity(row.len());
    for (i, col) in row.columns().iter().enumerate() {
        obj.insert(col.name().to_owned(), cell(row, i));
    }
    Value::Object(obj)
}

fn pg_row_to_json(row: &PgRow) -> Value {
    row_to_json(row, pg_cell)
}

fn mysql_row_to_json(row: &MySqlRow) -> Value {
    row_to_json(row, mysql_cell)
}

fn sqlite_row_to_json(row: &SqliteRow) -> Value {
    row_to_json(row, sqlite_cell)
}

/// PostgreSQL cell: dispatch on the column's type name. Types without a
/// compiled-in decoder (NUMERIC, temporal, uuid, arrays, …) fail the String
/// fallback decode and surface as null — the row still counts.
fn pg_cell(row: &PgRow, idx: usize) -> Value {
    match row.try_get_raw(idx) {
        Ok(raw) if !raw.is_null() => {}
        _ => return Value::Null,
    }
    match row.columns()[idx].type_info().name() {
        "BOOL" => row.try_get::<bool, _>(idx).map(Value::Bool),
        // sqlx-pg widens: i64 decodes INT2/INT4/INT8 alike.
        "INT2" | "INT4" | "INT8" => row.try_get::<i64, _>(idx).map(Value::from),
        "FLOAT4" | "FLOAT8" => row.try_get::<f64, _>(idx).map(json_f64),
        "JSON" | "JSONB" => row.try_get::<Value, _>(idx),
        "BYTEA" => row.try_get::<Vec<u8>, _>(idx).map(|b| base64_cell(&b)),
        // TEXT, VARCHAR, CHAR, NAME, … — and everything unmapped, which
        // simply fails the decode.
        _ => row.try_get::<String, _>(idx).map(Value::String),
    }
    .unwrap_or(Value::Null)
}

/// MySQL/MariaDB cell: decode is driven by the wire value's type, so a
/// widening cascade (first type that decodes wins) works. TINYINT — MySQL's
/// boolean spelling — surfaces as a number; temporal and DECIMAL values
/// have no chrono/decimal support compiled in and surface as null.
fn mysql_cell(row: &MySqlRow, idx: usize) -> Value {
    match row.try_get_raw(idx) {
        Ok(raw) if !raw.is_null() => {}
        _ => return Value::Null,
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<u64, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return json_f64(v);
    }
    if let Ok(v) = row.try_get::<Value, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Value::String(v);
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return base64_cell(&v);
    }
    Value::Null
}

/// SQLite cell: decode by storage class (SQLite is dynamically typed).
/// There is no native boolean — declared BOOLEANs are integers.
fn sqlite_cell(row: &SqliteRow, idx: usize) -> Value {
    match row.try_get_raw(idx) {
        Ok(raw) if !raw.is_null() => {}
        _ => return Value::Null,
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return Value::from(v);
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return json_f64(v);
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Value::String(v);
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return base64_cell(&v);
    }
    Value::Null
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Look up the parked connection for `params.id`, or explain what went wrong.
fn take_conn(params: &Value, ctx: &Context) -> Result<(String, DbConn), String> {
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("'id' is required (the output of std/db-connect@v1)")?;
    let conn = ctx.resources.take_db(id).ok_or(format!(
        "unknown connection id '{id}' — not connected in this iteration, already closed, \
         or opened in `before:` setup (connections do not cross into VU iterations)"
    ))?;
    Ok((id.to_string(), conn))
}

/// Interpolation for `std/db-query@v1`: every parameter EXCEPT the SQL text
/// itself. The query is bound, never string-interpolated — a `${{`
/// sequence in SQL must reach the database verbatim.
pub(crate) fn interpolate_query_params(params: &Value, ctx: &Context) -> Value {
    let Value::Object(map) = params else {
        return params.clone();
    };
    let mut map = map.clone();
    let raw_query = map.remove("query");
    let mut value = Value::Object(map);
    if super::actions::has_placeholder(&value) {
        value = ctx.interpolate_value(&value);
    }
    if let (Value::Object(obj), Some(query)) = (&mut value, raw_query) {
        obj.insert("query".to_string(), query);
    }
    value
}

/// The `metrics` object every failing DB step emits: the total counter plus
/// one per class, so `db_errors` stays the headline and classes aggregate.
fn error_metrics(class: &str) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::with_capacity(2);
    m.insert("db_errors".to_string(), Value::from(1));
    m.insert(format!("db_errors_{class}"), Value::from(1));
    m
}

// ---------------------------------------------------------------------------
// std/db-connect@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   driver      – postgres | mysql | sqlite (required; mysql covers MariaDB)
//   dsn         – driver-native connection string (required); may embed
//                 `${{ vars.db_dsn }}`-style interpolation for secrets
//   tls         – true | false | "skip-verify", default true; ignored for
//                 sqlite. true verifies certificate + hostname (PostgreSQL
//                 VerifyFull, MySQL VerifyIdentity); skip-verify encrypts
//                 without verification (Require/Required)
//   mode        – persistent | per-query, default persistent (see the
//                 module doc's Modes section)
//   pool_size   – persistent mode pool size, default 1 (max 1024)
//   timeout_ms  – ms for the connect, default 30000
//
// Output:
//   { "id": "db-1", "driver": "postgres", "mode": "persistent",
//     "connected": true, "duration_ms": <f64>,
//     "metrics": { "db_connect_duration": [<f64>] } }
//
// In per-query mode no connection is opened (`connected: false`) — the
// connect step stores the config and each db-query pays connect + query.
// A failed connect emits no duration sample, but it does count in
// `db_errors` (usually class `connection`).

pub(crate) async fn db_connect_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    let cp = match parse_connect_params(params) {
        Ok(p) => p,
        Err(msg) => return err(step_name, &msg),
    };

    let t0 = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_millis(cp.timeout_ms), connect_inner(&cp))
        .await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let (state, target) = match outcome {
        Ok(Ok(pair)) => pair,
        Ok(Err(fail)) => {
            return ActionOutput {
                value: json!({
                    "connected": false,
                    "error": fail.detail,
                    "error_kind": fail.class,
                    "duration_ms": duration_ms,
                    "metrics": Value::Object(error_metrics(fail.class)),
                }),
                logs: vec![(
                    LogTag::Err,
                    format!(
                        "{step_name}: {} connect failed ({}): {}",
                        cp.driver.as_str(),
                        fail.class,
                        fail.detail
                    ),
                )],
                success: false,
                http_sample: None,
            };
        }
        Err(_) => {
            let detail = format!("connect TIMEOUT after {duration_ms:.2}ms");
            return ActionOutput {
                value: json!({
                    "connected": false,
                    "error": detail,
                    "error_kind": "timeout",
                    "duration_ms": duration_ms,
                    "metrics": Value::Object(error_metrics("timeout")),
                }),
                logs: vec![(
                    LogTag::Err,
                    format!("{step_name}: {} connect failed (timeout): {detail}", cp.driver.as_str()),
                )],
                success: false,
                http_sample: None,
            };
        }
    };

    let id = ctx.resources.insert_db(DbConn {
        driver: cp.driver,
        target: target.clone(),
        per_query: cp.per_query,
        state,
        tx: None,
        wrap_tx: false,
    });

    ActionOutput {
        value: json!({
            "id": id,
            "driver": cp.driver.as_str(),
            "mode": if cp.per_query { "per-query" } else { "persistent" },
            "connected": !cp.per_query,
            "duration_ms": duration_ms,
            "metrics": { "db_connect_duration": [duration_ms], "db_errors": 0 },
        }),
        logs: vec![(
            LogTag::Out,
            if cp.per_query {
                format!(
                    "{} connect {target} → {id} (per-query: config stored, connects per query) ({duration_ms:.2}ms)",
                    cp.driver.as_str()
                )
            } else {
                format!(
                    "{} connect {target} → {id} ({duration_ms:.2}ms)",
                    cp.driver.as_str()
                )
            },
        )],
        success: true,
        http_sample: None,
    }
}

/// A failed connect: classified detail, already DSN-sanitized.
struct DbFail {
    class: &'static str,
    detail: String,
}

impl DbFail {
    /// DSN parse/validation failure — a config error, not a connection one.
    fn config(msg: String) -> Self {
        Self {
            class: "other",
            detail: msg,
        }
    }

    fn from_sqlx(e: &sqlx::Error, driver: DbDriver, dsn: &str) -> Self {
        Self {
            class: classify(e, driver),
            detail: sanitize_detail(&error_chain(e), dsn),
        }
    }
}

/// Establish (persistent) or validate/store (per-query) the connect config.
async fn connect_inner(cp: &ConnectParams) -> Result<(DbState, String), DbFail> {
    match cp.driver {
        DbDriver::Postgres => {
            let (opts, target) = pg_options(cp).map_err(DbFail::config)?;
            if cp.per_query {
                return Ok((DbState::Profile(Box::new(DbProfile::Pg(opts))), target));
            }
            let pool = PgPoolOptions::new()
                .max_connections(cp.pool_size as u32)
                .min_connections(0)
                .acquire_timeout(Duration::from_millis(cp.timeout_ms))
                // A query step should measure the query, not a health check.
                .test_before_acquire(false)
                // connect_with opens at least one connection, so a dead
                // server fails here, at the connect step.
                .connect_with(opts)
                .await
                .map_err(|e| DbFail::from_sqlx(&e, DbDriver::Postgres, &cp.dsn))?;
            Ok((DbState::Pool(DbPool::Pg(pool)), target))
        }
        DbDriver::MySql => {
            let (opts, target) = mysql_options(cp).map_err(DbFail::config)?;
            if cp.per_query {
                return Ok((DbState::Profile(Box::new(DbProfile::My(opts))), target));
            }
            let pool = MySqlPoolOptions::new()
                .max_connections(cp.pool_size as u32)
                .min_connections(0)
                .acquire_timeout(Duration::from_millis(cp.timeout_ms))
                .test_before_acquire(false)
                .connect_with(opts)
                .await
                .map_err(|e| DbFail::from_sqlx(&e, DbDriver::MySql, &cp.dsn))?;
            Ok((DbState::Pool(DbPool::My(pool)), target))
        }
        DbDriver::Sqlite => {
            let (opts, target) = sqlite_options(cp).map_err(DbFail::config)?;
            if cp.per_query {
                return Ok((DbState::Profile(Box::new(DbProfile::Sqlite(opts))), target));
            }
            let pool = SqlitePoolOptions::new()
                .max_connections(cp.pool_size as u32)
                .min_connections(0)
                .acquire_timeout(Duration::from_millis(cp.timeout_ms))
                .test_before_acquire(false)
                .connect_with(opts)
                .await
                .map_err(|e| DbFail::from_sqlx(&e, DbDriver::Sqlite, &cp.dsn))?;
            Ok((DbState::Pool(DbPool::Sqlite(pool)), target))
        }
    }
}

// ---------------------------------------------------------------------------
// Connectivity probe (editor support — re-exported as crate::introspect::probe_db)
// ---------------------------------------------------------------------------

/// Validate `driver`/`tls`, open one connection, run a trivial round-trip
/// (`SELECT 1`), and return the wall time in milliseconds. This is the
/// `std/db-connect@v1` connect path minus the pool, so what the probe
/// reports is what a run would do. `timeout_ms` wraps connect + round-trip.
/// Errors are DSN-sanitized exactly like connect errors (see
/// [`sanitize_detail`]) — no DSN or password ever leaks into the string.
pub async fn probe_db(driver: &str, dsn: &str, tls: &str, timeout_ms: u64) -> Result<u128, String> {
    let driver = match driver {
        "postgres" => DbDriver::Postgres,
        "mysql" => DbDriver::MySql,
        "sqlite" => DbDriver::Sqlite,
        other => {
            return Err(format!(
                "unknown driver '{other}' — expected postgres, mysql, or sqlite"
            ))
        }
    };
    let tls = parse_tls(&Value::String(tls.to_string()))?;
    let cp = ConnectParams {
        driver,
        dsn: dsn.to_string(),
        tls,
        per_query: false,
        timeout_ms,
        pool_size: 1,
    };

    let t0 = Instant::now();
    match tokio::time::timeout(Duration::from_millis(timeout_ms), probe_roundtrip(&cp)).await {
        Ok(Ok(())) => Ok(t0.elapsed().as_millis()),
        Ok(Err(detail)) => Err(detail),
        Err(_) => Err(format!("probe timeout after {}ms", t0.elapsed().as_millis())),
    }
}

/// One connect + `SELECT 1` per driver, on a single bare connection (no
/// pool). The probe uses the same unnamed statements as every other query
/// ([`step_query`]) — against a transaction-mode pooler a named statement
/// would collide on the shared backend just the same.
async fn probe_roundtrip(cp: &ConnectParams) -> Result<(), String> {
    /// DSN-sanitized error detail, same as a failed db-connect reports.
    fn detail(e: &sqlx::Error, cp: &ConnectParams) -> String {
        sanitize_detail(&error_chain(e), &cp.dsn)
    }
    match cp.driver {
        DbDriver::Postgres => {
            let (opts, _) = pg_options(cp)?;
            let mut conn = opts.connect().await.map_err(|e| detail(&e, cp))?;
            step_query::<sqlx::Postgres>("SELECT 1")
                .execute(&mut conn)
                .await
                .map_err(|e| detail(&e, cp))?;
        }
        DbDriver::MySql => {
            let (opts, _) = mysql_options(cp)?;
            let mut conn = opts.connect().await.map_err(|e| detail(&e, cp))?;
            step_query::<sqlx::MySql>("SELECT 1")
                .execute(&mut conn)
                .await
                .map_err(|e| detail(&e, cp))?;
        }
        DbDriver::Sqlite => {
            let (opts, _) = sqlite_options(cp)?;
            let mut conn = opts.connect().await.map_err(|e| detail(&e, cp))?;
            step_query::<sqlx::Sqlite>("SELECT 1")
                .execute(&mut conn)
                .await
                .map_err(|e| detail(&e, cp))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// std/db-query@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   id          – Connection ID from std/db-connect@v1 (required;
//                 `id: "${{ conn.id }}"` interpolation is the norm)
//   query       – SQL text with driver-native placeholders ($1/?) — NEVER
//                 interpolated; 64 KiB hard limit (max_query_bytes)
//   params      – positional bind values (each entry MAY be interpolated);
//                 strings/numbers/booleans/null bind, arrays/objects are
//                 rejected
//   max_rows    – hard cap on rows read into memory, default 10000; extra
//                 rows only set `truncated: true`
//   timeout_ms  – ms for the query (per-query mode: connect + query),
//                 default 30000
//
// Output:
//   { "rows": <u64>, "rows_affected": <u64>, "truncated": <bool>,
//     "data": [ { "col": <value>, … }, … ], "duration_ms": <f64>,
//     "metrics": { "db_query_duration": [<f64>], "db_rows": <u64> } }
//
// INSERT/UPDATE/DELETE/DDL are allowed by default. With a transaction open
// on the connection, the query runs inside it. A failed query fails the
// step but leaves the connection (and any open transaction) parked — the
// scenario decides whether to roll back.

pub(crate) async fn db_query_action(params: &Value, ctx: &Context, step_name: &str) -> ActionOutput {
    let spec = match parse_query_spec(params) {
        Ok(s) => s,
        Err(msg) => return err(step_name, &msg),
    };
    let (id, mut conn) = match take_conn(params, ctx) {
        Ok(pair) => pair,
        Err(msg) => return err(step_name, &msg),
    };
    let driver = conn.driver;
    let target = conn.target.clone();

    let t0 = Instant::now();
    // Pooler state learned by earlier queries on this connection (Postgres
    // autocommit only — see the pooler section above).
    let wrap_tx = conn.wrap_tx;
    let mut wrap_tx_learned = false;
    let run = tokio::time::timeout(Duration::from_millis(spec.timeout_ms), async {
        let sql = spec.sql;
        let binds = spec.binds;
        let max_rows = spec.max_rows;
        match (&mut conn.state, conn.tx.as_mut()) {
            (DbState::Pool(DbPool::Pg(pool)), None) => {
                let (outcome, learned) =
                    run_pg_pool_autocommit(pool, wrap_tx, sql, binds, max_rows).await;
                wrap_tx_learned = learned;
                outcome
            }
            (DbState::Pool(DbPool::Pg(_)), Some(DbTx::Pg(tx))) => {
                run_query(&mut **tx, sql, binds, max_rows, pg_row_to_json, |r| {
                    r.rows_affected()
                })
                .await
            }
            (DbState::Pool(DbPool::My(pool)), None) => {
                run_query(&*pool, sql, binds, max_rows, mysql_row_to_json, |r| {
                    r.rows_affected()
                })
                .await
            }
            (DbState::Pool(DbPool::My(_)), Some(DbTx::My(tx))) => {
                run_query(&mut **tx, sql, binds, max_rows, mysql_row_to_json, |r| {
                    r.rows_affected()
                })
                .await
            }
            (DbState::Pool(DbPool::Sqlite(pool)), None) => {
                run_query(&*pool, sql, binds, max_rows, sqlite_row_to_json, |r| {
                    r.rows_affected()
                })
                .await
            }
            (DbState::Pool(DbPool::Sqlite(_)), Some(DbTx::Sqlite(tx))) => {
                run_query(&mut **tx, sql, binds, max_rows, sqlite_row_to_json, |r| {
                    r.rows_affected()
                })
                .await
            }
            (DbState::Profile(profile), None) => match &mut **profile {
                DbProfile::Pg(opts) => {
                    let (outcome, learned) =
                        run_pg_profile_autocommit(opts, wrap_tx, sql, binds, max_rows).await;
                    wrap_tx_learned = learned;
                    outcome
                }
                DbProfile::My(opts) => {
                    let mut fresh = opts.connect().await?;
                    run_query(&mut fresh, sql, binds, max_rows, mysql_row_to_json, |r| {
                        r.rows_affected()
                    })
                    .await
                }
                DbProfile::Sqlite(opts) => {
                    let mut fresh = opts.connect().await?;
                    run_query(&mut fresh, sql, binds, max_rows, sqlite_row_to_json, |r| {
                        r.rows_affected()
                    })
                    .await
                }
            },
            (DbState::Profile(_), Some(_)) => Err(sqlx::Error::Protocol(
                "per-query connections cannot hold a transaction".into(),
            )),
            // A pool/transaction driver pair never mismatches by
            // construction — the transaction is always begun from this
            // connection's own pool.
            _ => Err(sqlx::Error::Protocol(
                "connection state does not match its transaction".into(),
            )),
        }
    })
    .await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    if wrap_tx_learned {
        conn.wrap_tx = true;
    }
    // Whatever happened, the connection survives a query error — a failed
    // statement no more kills a pool than a failed RPC kills a channel.
    ctx.resources.put_back_db(&id, conn);

    match run {
        Ok(Ok(outcome)) => {
            let db_rows = if outcome.rows > 0 {
                outcome.rows
            } else {
                outcome.rows_affected
            };
            // The log shows the headline number; the JSON keeps both raw
            // values. (On SQLite a SELECT's rows_affected is the previous
            // DML's change count — a C API quirk — so mixing both into one
            // log line would read as nonsense there.)
            let what = if outcome.rows > 0 {
                format!("{} rows", outcome.rows)
            } else if outcome.rows_affected > 0 {
                format!("{} affected", outcome.rows_affected)
            } else {
                "ok".to_string()
            };
            let mut log = format!(
                "{} {target} [{id}] → {what} ({duration_ms:.2}ms)",
                driver.as_str()
            );
            if outcome.truncated {
                log.push_str(" [truncated at max_rows]");
            }
            ActionOutput {
                value: json!({
                    "rows": outcome.rows,
                    "rows_affected": outcome.rows_affected,
                    "truncated": outcome.truncated,
                    "data": outcome.data,
                    "duration_ms": duration_ms,
                    "metrics": {
                        "db_query_duration": [duration_ms],
                        "db_rows": db_rows,
                        "db_errors": 0,
                    },
                }),
                logs: vec![(LogTag::Out, log)],
                success: true,
                http_sample: None,
            }
        }
        Ok(Err(e)) => {
            let class = classify(&e, driver);
            // Query errors come from the server/protocol, never from the
            // DSN — no sanitization needed here (connect errors get it).
            let detail = error_chain(&e);
            let fail_conn = DbConnRef {
                driver,
                target: &target,
            };
            db_fail_ref(step_name, fail_conn, &id, class, &detail, duration_ms)
        }
        Err(_) => {
            let detail = format!("query TIMEOUT after {duration_ms:.2}ms");
            let fail_conn = DbConnRef {
                driver,
                target: &target,
            };
            db_fail_ref(
                step_name,
                fail_conn,
                &id,
                "timeout",
                &detail,
                duration_ms,
            )
        }
    }
}

/// Borrowed view of a connection's log identity for [`db_fail_ref`] (the
/// owned connection was already returned to the registry).
struct DbConnRef<'a> {
    driver: DbDriver,
    target: &'a str,
}

/// Failure output for `db-query` — always carries the duration sample.
fn db_fail_ref(
    step_name: &str,
    conn: DbConnRef<'_>,
    id: &str,
    class: &str,
    detail: &str,
    duration_ms: f64,
) -> ActionOutput {
    let mut metrics = error_metrics(class);
    metrics.insert("db_query_duration".to_string(), json!([duration_ms]));
    ActionOutput {
        value: json!({
            "error": detail,
            "error_kind": class,
            "duration_ms": duration_ms,
            "metrics": Value::Object(metrics),
        }),
        logs: vec![(
            LogTag::Err,
            format!(
                "{step_name}: {} {} [{id}] → ERROR ({class}): {detail}",
                conn.driver.as_str(),
                conn.target
            ),
        )],
        success: false,
        http_sample: None,
    }
}

// ---------------------------------------------------------------------------
// std/db-tx-begin@v1 / std/db-tx-commit@v1 / std/db-tx-rollback@v1
// ---------------------------------------------------------------------------
//
// Parameters (all three):
//   id          – Connection ID from std/db-connect@v1 (required)
//   timeout_ms  – ms for the BEGIN/COMMIT/ROLLBACK, default 30000
//
// Output:
//   begin:    { "id": "db-1", "tx": true, "duration_ms": <f64>,
//               "metrics": { "db_query_duration": [<f64>] } }
//   commit:   { "id": "db-1", "committed": true, … }
//   rollback: { "id": "db-1", "rolled_back": true, … }
//
// Transactions need mode: persistent (a per-query connection has nothing
// to hold a transaction on). One open transaction per connection; while it
// is open, db-query runs inside it. Each step is timed as one unit via the
// usual step-duration mechanics (`duration_ms` + `db_query_duration`).

pub(crate) async fn db_tx_begin_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    let (id, mut conn) = match take_conn(params, ctx) {
        Ok(pair) => pair,
        Err(msg) => return err(step_name, &msg),
    };
    if conn.per_query {
        ctx.resources.put_back_db(&id, conn);
        return err(
            step_name,
            &format!("transactions require mode: persistent — '{id}' is a per-query connection"),
        );
    }
    if conn.tx.is_some() {
        ctx.resources.put_back_db(&id, conn);
        return err(
            step_name,
            &format!("'{id}' already has an open transaction — commit or roll back first"),
        );
    }
    let timeout_ms = timeout_ms_param(params);
    let driver = conn.driver;
    let target = conn.target.clone();

    let t0 = Instant::now();
    let begun = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        match &conn.state {
            DbState::Pool(DbPool::Pg(pool)) => pool.begin().await.map(DbTx::Pg),
            DbState::Pool(DbPool::My(pool)) => pool.begin().await.map(DbTx::My),
            DbState::Pool(DbPool::Sqlite(pool)) => pool.begin().await.map(DbTx::Sqlite),
            DbState::Profile(_) => Err(sqlx::Error::Protocol(
                "per-query connections cannot hold a transaction".into(),
            )),
        }
    })
    .await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    match begun {
        Ok(Ok(tx)) => {
            conn.tx = Some(tx);
            ctx.resources.put_back_db(&id, conn);
            ActionOutput {
                value: json!({
                    "id": id,
                    "tx": true,
                    "duration_ms": duration_ms,
                    "metrics": { "db_query_duration": [duration_ms], "db_errors": 0 },
                }),
                logs: vec![(
                    LogTag::Out,
                    format!("{} {target} [{id}] → BEGIN ({duration_ms:.2}ms)", driver.as_str()),
                )],
                success: true,
                http_sample: None,
            }
        }
        Ok(Err(e)) => {
            let class = classify(&e, driver);
            let detail = error_chain(&e);
            let fail_conn = DbConnRef {
                driver,
                target: &target,
            };
            ctx.resources.put_back_db(&id, conn);
            db_fail_ref(step_name, fail_conn, &id, class, &detail, duration_ms)
        }
        Err(_) => {
            let detail = format!("BEGIN TIMEOUT after {duration_ms:.2}ms");
            let fail_conn = DbConnRef {
                driver,
                target: &target,
            };
            ctx.resources.put_back_db(&id, conn);
            db_fail_ref(
                step_name,
                fail_conn,
                &id,
                "timeout",
                &detail,
                duration_ms,
            )
        }
    }
}

/// Shared core of commit/rollback.
async fn db_tx_finish_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
    commit: bool,
) -> ActionOutput {
    let (id, mut conn) = match take_conn(params, ctx) {
        Ok(pair) => pair,
        Err(msg) => return err(step_name, &msg),
    };
    let Some(tx) = conn.tx.take() else {
        ctx.resources.put_back_db(&id, conn);
        return err(
            step_name,
            &format!("no open transaction on '{id}' — begin one with std/db-tx-begin@v1"),
        );
    };
    let timeout_ms = timeout_ms_param(params);
    let driver = conn.driver;
    let target = conn.target.clone();
    let verb = if commit { "COMMIT" } else { "ROLLBACK" };

    let t0 = Instant::now();
    let finished = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        match (tx, commit) {
            (DbTx::Pg(t), true) => t.commit().await,
            (DbTx::Pg(t), false) => t.rollback().await,
            (DbTx::My(t), true) => t.commit().await,
            (DbTx::My(t), false) => t.rollback().await,
            (DbTx::Sqlite(t), true) => t.commit().await,
            (DbTx::Sqlite(t), false) => t.rollback().await,
        }
    })
    .await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
    ctx.resources.put_back_db(&id, conn);

    match finished {
        Ok(Ok(())) => {
            let mut value = json!({
                "id": id,
                "duration_ms": duration_ms,
                "metrics": { "db_query_duration": [duration_ms], "db_errors": 0 },
            });
            value[if commit { "committed" } else { "rolled_back" }] = Value::Bool(true);
            ActionOutput {
                value,
                logs: vec![(
                    LogTag::Out,
                    format!("{} {target} [{id}] → {verb} ({duration_ms:.2}ms)", driver.as_str()),
                )],
                success: true,
                http_sample: None,
            }
        }
        Ok(Err(e)) => {
            let class = classify(&e, driver);
            let detail = error_chain(&e);
            let fail_conn = DbConnRef {
                driver,
                target: &target,
            };
            db_fail_ref(step_name, fail_conn, &id, class, &detail, duration_ms)
        }
        Err(_) => {
            let detail = format!("{verb} TIMEOUT after {duration_ms:.2}ms");
            let fail_conn = DbConnRef {
                driver,
                target: &target,
            };
            db_fail_ref(
                step_name,
                fail_conn,
                &id,
                "timeout",
                &detail,
                duration_ms,
            )
        }
    }
}

pub(crate) async fn db_tx_commit_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    db_tx_finish_action(params, ctx, step_name, true).await
}

pub(crate) async fn db_tx_rollback_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    db_tx_finish_action(params, ctx, step_name, false).await
}

// ---------------------------------------------------------------------------
// std/db-close@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   id – Connection ID from std/db-connect@v1 (required)
//
// Output:
//   { "id": "db-1", "closed": true, "duration_ms": <f64> }
//
// Closes the pool (per-query profiles simply drop — there is nothing to
// close) and releases the id. An open transaction rolls back as it drops.
// Connections left open are closed implicitly at iteration end.

pub(crate) async fn db_close_action(params: &Value, ctx: &Context, step_name: &str) -> ActionOutput {
    let (id, mut conn) = match take_conn(params, ctx) {
        Ok(pair) => pair,
        Err(msg) => return err(step_name, &msg),
    };
    let driver = conn.driver;
    let target = conn.target.clone();

    let t0 = Instant::now();
    // Drop a parked transaction BEFORE closing: dropping it starts its
    // rollback and returns its connection to the pool, which `close()` waits
    // for — awaiting `close()` with the transaction still checked out hangs.
    drop(conn.tx.take());
    if let DbState::Pool(pool) = &conn.state {
        match pool {
            DbPool::Pg(p) => p.close().await,
            DbPool::My(p) => p.close().await,
            DbPool::Sqlite(p) => p.close().await,
        }
    }
    // The id is not returned to the registry — it is closed. The transaction
    // dropped above rolled back as it went.
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    ActionOutput {
        value: json!({
            "id": id,
            "closed": true,
            "duration_ms": duration_ms,
        }),
        logs: vec![(
            LogTag::Out,
            format!("{} {target} [{id}] → closed ({duration_ms:.2}ms)", driver.as_str()),
        )],
        success: true,
        http_sample: None,
    }
}


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::actions::execute_action;
    use serde_json::json;
    use sqlx::error::ErrorKind;

    // -----------------------------------------------------------------
    // Parameter parsing / validation
    // -----------------------------------------------------------------

    #[test]
    fn connect_params_defaults() {
        let cp = parse_connect_params(&json!({
            "driver": "postgres",
            "dsn": "postgres://u:p@db.internal:5432/app",
        }))
        .unwrap();
        assert_eq!(cp.driver, DbDriver::Postgres);
        assert_eq!(cp.tls, TlsMode::Verify);
        assert!(!cp.per_query);
        assert_eq!(cp.timeout_ms, 30_000);
        assert_eq!(cp.pool_size, 1);
    }

    #[test]
    fn connect_params_full_override() {
        let cp = parse_connect_params(&json!({
            "driver": "mysql",
            "dsn": "mysql://u:p@db.internal:3306/app",
            "tls": "skip-verify",
            "mode": "per-query",
            "timeout_ms": 500,
            "pool_size": 8,
        }))
        .unwrap();
        assert_eq!(cp.driver, DbDriver::MySql);
        assert_eq!(cp.tls, TlsMode::SkipVerify);
        assert!(cp.per_query);
        assert_eq!(cp.timeout_ms, 500);
        assert_eq!(cp.pool_size, 8);
    }

    #[test]
    fn connect_params_interpolated_string_forms() {
        // Interpolated `${{ … }}` values arrive as strings.
        let cp = parse_connect_params(&json!({
            "driver": "sqlite",
            "dsn": "sqlite::memory:",
            "tls": "false",
            "timeout_ms": "2500",
        }))
        .unwrap();
        assert_eq!(cp.tls, TlsMode::Off);
        assert_eq!(cp.timeout_ms, 2500);
    }

    #[test]
    fn connect_params_rejections() {
        // `.err()` avoids the `T: Debug` bound of `unwrap_err` — and a
        // derived Debug on ConnectParams would print the DSN on failure.
        let missing_driver = parse_connect_params(&json!({ "dsn": "sqlite::memory:" }));
        assert!(missing_driver.err().unwrap().contains("'driver' is required"));

        let bad_driver = parse_connect_params(&json!({ "driver": "oracle", "dsn": "x" }));
        assert!(bad_driver.err().unwrap().contains("unknown driver 'oracle'"));

        let missing_dsn = parse_connect_params(&json!({ "driver": "sqlite" }));
        assert!(missing_dsn.err().unwrap().contains("'dsn' is required"));

        let bad_tls = parse_connect_params(&json!({
            "driver": "sqlite", "dsn": "sqlite::memory:", "tls": "maybe",
        }));
        assert!(bad_tls.err().unwrap().contains("'tls' must be"));

        let bad_mode = parse_connect_params(&json!({
            "driver": "sqlite", "dsn": "sqlite::memory:", "mode": "sometimes",
        }));
        assert!(bad_mode.err().unwrap().contains("'mode' must be"));
    }

    #[test]
    fn tls_parse_variants() {
        assert_eq!(parse_tls(&json!(true)).unwrap(), TlsMode::Verify);
        assert_eq!(parse_tls(&json!("true")).unwrap(), TlsMode::Verify);
        assert_eq!(parse_tls(&json!(false)).unwrap(), TlsMode::Off);
        assert_eq!(parse_tls(&json!("FALSE")).unwrap(), TlsMode::Off);
        assert_eq!(parse_tls(&json!("skip-verify")).unwrap(), TlsMode::SkipVerify);
        assert!(parse_tls(&json!(1)).is_err());
        assert!(parse_tls(&json!("require")).is_err());
    }

    #[test]
    fn query_spec_defaults_and_overrides() {
        let params = json!({ "query": "SELECT 1" });
        let spec = parse_query_spec(&params).unwrap();
        assert_eq!(spec.max_rows, 10_000, "max_rows default");
        assert_eq!(spec.timeout_ms, 30_000, "timeout_ms default");
        assert!(spec.binds.is_empty());

        let params = json!({
            "query": "SELECT 1",
            "params": [1, "two", true, null],
            "max_rows": 500,
            "timeout_ms": "1500",
        });
        let spec = parse_query_spec(&params).unwrap();
        assert_eq!(spec.max_rows, 500);
        assert_eq!(spec.timeout_ms, 1500);
        assert_eq!(spec.binds.len(), 4);
    }

    #[test]
    fn query_spec_rejections() {
        // Inline assertions: the spec borrows the params, so the Result
        // must be consumed within the same statement.
        assert!(parse_query_spec(&json!({})).err().unwrap().contains("'query' is required"));
        assert!(parse_query_spec(&json!({ "query": "   " }))
            .err()
            .unwrap()
            .contains("'query' is required"));
        assert!(parse_query_spec(&json!({ "query": "SELECT 1", "params": "x" }))
            .err()
            .unwrap()
            .contains("'params' must be an array"));
    }

    #[test]
    fn query_spec_64kib_hard_limit() {
        let at_limit = "x".repeat(MAX_QUERY_BYTES);
        assert!(parse_query_spec(&json!({ "query": at_limit })).is_ok());

        let over_limit = "x".repeat(MAX_QUERY_BYTES + 1);
        let err = parse_query_spec(&json!({ "query": over_limit })).err().unwrap();
        assert!(err.contains("64 KiB hard limit"), "unexpected: {err}");
    }

    // -----------------------------------------------------------------
    // Error classification
    // -----------------------------------------------------------------

    #[test]
    fn classify_constraint_via_sqlx_kind() {
        // ErrorKind is neither Copy nor Clone — construct fresh values.
        for driver in [DbDriver::Postgres, DbDriver::MySql, DbDriver::Sqlite] {
            assert_eq!(classify_db_error(ErrorKind::UniqueViolation, None, driver), "constraint");
            assert_eq!(classify_db_error(ErrorKind::ForeignKeyViolation, None, driver), "constraint");
            assert_eq!(classify_db_error(ErrorKind::NotNullViolation, None, driver), "constraint");
            assert_eq!(classify_db_error(ErrorKind::CheckViolation, None, driver), "constraint");
        }
    }

    #[test]
    fn classify_postgres_sqlstate() {
        let pg = DbDriver::Postgres;
        assert_eq!(classify_db_error(ErrorKind::Other, Some("40P01"), pg), "deadlock");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("40001"), pg), "deadlock");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("57014"), pg), "timeout");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("08006"), pg), "connection");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("08P01"), pg), "connection");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("23505"), pg), "constraint");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("42P01"), pg), "other");
        assert_eq!(classify_db_error(ErrorKind::Other, None, pg), "other");
    }

    #[test]
    fn classify_mysql_errno() {
        let my = DbDriver::MySql;
        assert_eq!(classify_db_error(ErrorKind::Other, Some("1213"), my), "deadlock");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("1205"), my), "timeout");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("1062"), my), "constraint");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("1452"), my), "constraint");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("1045"), my), "connection");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("1040"), my), "connection");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("1146"), my), "other");
    }

    #[test]
    fn classify_sqlite_result_code() {
        let lite = DbDriver::Sqlite;
        assert_eq!(classify_db_error(ErrorKind::Other, Some("5"), lite), "deadlock"); // BUSY
        assert_eq!(classify_db_error(ErrorKind::Other, Some("6"), lite), "deadlock"); // LOCKED
        assert_eq!(classify_db_error(ErrorKind::Other, Some("19"), lite), "constraint");
        assert_eq!(classify_db_error(ErrorKind::Other, Some("1555"), lite), "constraint"); // PK
        assert_eq!(classify_db_error(ErrorKind::Other, Some("2067"), lite), "constraint"); // UNIQUE
        assert_eq!(classify_db_error(ErrorKind::Other, Some("787"), lite), "constraint"); // FK
        assert_eq!(classify_db_error(ErrorKind::Other, Some("1"), lite), "other");
        assert_eq!(classify_db_error(ErrorKind::Other, None, lite), "other");
    }

    #[test]
    fn classify_outer_variants() {
        let pg = DbDriver::Postgres;
        let io_err = sqlx::Error::Io(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "x"));
        assert_eq!(classify(&io_err, pg), "connection");
        assert_eq!(classify(&sqlx::Error::PoolTimedOut, pg), "timeout");
        assert_eq!(classify(&sqlx::Error::PoolClosed, pg), "connection");
        assert_eq!(classify(&sqlx::Error::WorkerCrashed, pg), "connection");
        assert_eq!(classify(&sqlx::Error::RowNotFound, pg), "other");
        assert_eq!(classify(&sqlx::Error::Protocol("x".into()), pg), "other");
    }

    // -----------------------------------------------------------------
    // DSN sanitization
    // -----------------------------------------------------------------

    #[test]
    fn dsn_password_extraction() {
        assert_eq!(dsn_password("postgres://u:s3cret@h:5432/db"), Some("s3cret"));
        assert_eq!(dsn_password("mysql://root:p%40ss@h/db"), Some("p%40ss"));
        // A password may itself contain '@' — userinfo ends at the last one.
        assert_eq!(dsn_password("postgres://u:p@ss@h/db"), Some("p@ss"));
        assert_eq!(dsn_password("postgres://u@h/db"), None); // no password
        assert_eq!(dsn_password("postgres://u:@h/db"), None); // empty password
        assert_eq!(dsn_password("postgres://h:5432/db"), None); // no userinfo
        assert_eq!(dsn_password("postgres://h/p@th"), None); // '@' past the path
        assert_eq!(dsn_password("sqlite://data.db"), None);
        assert_eq!(dsn_password("sqlite::memory:"), None);
    }

    #[test]
    fn sanitize_detail_scrubs_dsn_and_password() {
        let dsn = "postgres://u:s3cret@db.internal:5432/app";
        let detail = format!("connection failed: could not connect to {dsn}: password authentication failed for user u");
        let out = sanitize_detail(&detail, dsn);
        assert!(!out.contains("s3cret"), "password leaked: {out}");
        assert!(!out.contains(dsn), "dsn leaked: {out}");
        assert!(out.contains("[dsn]"));

        // Password appearing on its own is scrubbed too.
        let out = sanitize_detail("auth failed for password s3cret", dsn);
        assert!(!out.contains("s3cret"));
        assert!(out.contains("[redacted]"));

        // A sqlite DSN has no secret — sanitization is a no-op.
        let detail = "unable to open database file";
        assert_eq!(sanitize_detail(detail, "sqlite://data.db"), detail);
    }

    // -----------------------------------------------------------------
    // Interpolation: query text is never interpolated
    // -----------------------------------------------------------------

    #[test]
    fn interpolate_query_params_skips_query_text() {
        let mut ctx = Context::new();
        ctx.set("conn", json!({ "id": "db-1" }));
        ctx.set("who", json!("alice"));

        let out = interpolate_query_params(
            &json!({
                "id": "${{ conn.id }}",
                "query": "INSERT INTO t VALUES ('${{ who }}', ?)",
                "params": ["${{ who }}", 7],
            }),
            &ctx,
        );
        // The SQL text passes through verbatim…
        assert_eq!(out["query"], "INSERT INTO t VALUES ('${{ who }}', ?)");
        // …while every other parameter interpolates as usual.
        assert_eq!(out["id"], "db-1");
        assert_eq!(out["params"][0], "alice");
        assert_eq!(out["params"][1], 7);
    }

    // -----------------------------------------------------------------
    // Action-level validation (no database needed)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn query_missing_id_or_query_fails_fast() {
        let ctx = Context::new();
        let out = execute_action("std/db-query@v1", &json!({ "query": "SELECT 1" }), &ctx, "q").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("'id' is required"), "{:?}", out.logs);

        let out = execute_action("std/db-query@v1", &json!({ "id": "db-1" }), &ctx, "q").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("'query' is required"), "{:?}", out.logs);
    }

    #[tokio::test]
    async fn query_oversize_sql_rejected() {
        let ctx = Context::new();
        let big = "SELECT '".to_string() + &"x".repeat(MAX_QUERY_BYTES) + "'";
        let out = execute_action(
            "std/db-query@v1",
            &json!({ "id": "db-1", "query": big }),
            &ctx,
            "q",
        )
        .await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("64 KiB hard limit"), "{:?}", out.logs);
    }

    #[tokio::test]
    async fn unknown_id_errors_on_all_id_steps() {
        let ctx = Context::new();
        for action in [
            "std/db-query@v1",
            "std/db-tx-begin@v1",
            "std/db-tx-commit@v1",
            "std/db-tx-rollback@v1",
            "std/db-close@v1",
        ] {
            let params = if action == "std/db-query@v1" {
                json!({ "id": "db-99", "query": "SELECT 1" })
            } else {
                json!({ "id": "db-99" })
            };
            let out = execute_action(action, &params, &ctx, "s").await;
            assert!(!out.success, "{action} should fail");
            assert!(
                out.logs[0].1.contains("unknown connection id 'db-99'"),
                "{action}: {:?}",
                out.logs
            );
        }
    }

    #[tokio::test]
    async fn connect_bad_driver_rejected() {
        let ctx = Context::new();
        let out = execute_action(
            "std/db-connect@v1",
            &json!({ "driver": "oracle", "dsn": "x" }),
            &ctx,
            "c",
        )
        .await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("unknown driver 'oracle'"), "{:?}", out.logs);
    }

    #[tokio::test]
    async fn connect_invalid_dsn_does_not_leak_password() {
        let ctx = Context::new();
        let out = execute_action(
            "std/db-connect@v1",
            &json!({
                "driver": "postgres",
                "dsn": "postgres://u:s3cret@db.internal:not-a-port/app",
            }),
            &ctx,
            "c",
        )
        .await;
        assert!(!out.success);
        let detail = out.value["error"].as_str().unwrap_or("");
        assert!(!detail.contains("s3cret"), "password leaked: {detail}");
        assert!(!out.logs[0].1.contains("s3cret"), "log leaked: {:?}", out.logs);
    }

    // -----------------------------------------------------------------
    // SQLite integration (no server needed)
    // -----------------------------------------------------------------

    /// Run one step through the full dispatch (interpolation included).
    async fn run(ctx: &Context, action: &str, params: Value) -> ActionOutput {
        execute_action(action, &params, ctx, "test").await
    }

    #[tokio::test]
    async fn sqlite_full_flow() {
        let mut ctx = Context::new();
        ctx.set("who", json!("alice"));

        // connect (persistent, in-memory, default pool_size 1)
        let out = run(
            &ctx,
            "std/db-connect@v1",
            json!({ "driver": "sqlite", "dsn": "sqlite::memory:" }),
        )
        .await;
        assert!(out.success, "connect: {:?}", out.logs);
        assert_eq!(out.value["mode"], "persistent");
        assert_eq!(out.value["connected"], true);
        assert!(out.value["metrics"]["db_connect_duration"].is_array());
        let id = out.value["id"].as_str().unwrap().to_string();
        ctx.set("conn", out.value.clone());

        // tx-begin
        let out = run(&ctx, "std/db-tx-begin@v1", json!({ "id": "${{ conn.id }}" })).await;
        assert!(out.success, "tx-begin: {:?}", out.logs);
        assert_eq!(out.value["tx"], true);
        assert!(out.value["metrics"]["db_query_duration"].is_array());

        // CREATE TABLE inside the transaction
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": "${{ conn.id }}", "query": "CREATE TABLE t (name TEXT NOT NULL, n INTEGER)" }),
        )
        .await;
        assert!(out.success, "create: {:?}", out.logs);

        // INSERT with interpolated bind params (two rows)
        for (name, n) in [("alice", 1), ("bob", 2)] {
            let out = run(
                &ctx,
                "std/db-query@v1",
                json!({
                    "id": "${{ conn.id }}",
                    "query": "INSERT INTO t (name, n) VALUES (?, ?)",
                    "params": [name, n],
                }),
            )
            .await;
            assert!(out.success, "insert: {:?}", out.logs);
            assert_eq!(out.value["rows_affected"], 1);
            assert_eq!(out.value["metrics"]["db_rows"], 1);
        }
        // An interpolated bind param resolves through the context.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({
                "id": "${{ conn.id }}",
                "query": "INSERT INTO t (name, n) VALUES (?, ?)",
                "params": ["${{ who }}", 7],
            }),
        )
        .await;
        assert!(out.success, "insert interpolated: {:?}", out.logs);

        // SELECT inside the tx: rows, data content, metrics
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": "${{ conn.id }}", "query": "SELECT name, n FROM t ORDER BY n" }),
        )
        .await;
        assert!(out.success, "select: {:?}", out.logs);
        assert_eq!(out.value["rows"], 3);
        assert!(!out.value["truncated"].as_bool().unwrap());
        assert_eq!(out.value["metrics"]["db_rows"], 3);
        assert!(out.value["metrics"]["db_query_duration"].is_array());
        let data = out.value["data"].as_array().unwrap();
        assert_eq!(data[0]["name"], "alice");
        assert_eq!(data[0]["n"], 1);
        assert_eq!(data[2]["n"], 7);

        // max_rows enforcement on a >cap result
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({
                "id": "${{ conn.id }}",
                "query": "SELECT name, n FROM t",
                "max_rows": 2,
            }),
        )
        .await;
        assert!(out.success, "capped select: {:?}", out.logs);
        assert_eq!(out.value["rows"], 2);
        assert_eq!(out.value["data"].as_array().unwrap().len(), 2);
        assert_eq!(out.value["truncated"], true);
        assert_eq!(out.value["metrics"]["db_rows"], 2);

        // commit
        let out = run(&ctx, "std/db-tx-commit@v1", json!({ "id": "${{ conn.id }}" })).await;
        assert!(out.success, "commit: {:?}", out.logs);
        assert_eq!(out.value["committed"], true);

        // SELECT after commit — the data persisted past the transaction
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": "${{ conn.id }}", "query": "SELECT count(*) AS c FROM t" }),
        )
        .await;
        assert!(out.success, "post-commit select: {:?}", out.logs);
        assert_eq!(out.value["data"][0]["c"], 3);

        // rollback path: delete inside a tx, roll back, data survives
        let out = run(&ctx, "std/db-tx-begin@v1", json!({ "id": id })).await;
        assert!(out.success);
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "DELETE FROM t WHERE n = 7" }),
        )
        .await;
        assert!(out.success);
        assert_eq!(out.value["rows_affected"], 1);
        let out = run(&ctx, "std/db-tx-rollback@v1", json!({ "id": id })).await;
        assert!(out.success, "rollback: {:?}", out.logs);
        assert_eq!(out.value["rolled_back"], true);
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT count(*) AS c FROM t" }),
        )
        .await;
        assert_eq!(out.value["data"][0]["c"], 3, "rollback restored the row");

        // double begin is rejected while a tx is open
        let out = run(&ctx, "std/db-tx-begin@v1", json!({ "id": id })).await;
        assert!(out.success);
        let out = run(&ctx, "std/db-tx-begin@v1", json!({ "id": id })).await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("already has an open transaction"), "{:?}", out.logs);
        let out = run(&ctx, "std/db-tx-rollback@v1", json!({ "id": id })).await;
        assert!(out.success);

        // commit without an open tx is a clear error
        let out = run(&ctx, "std/db-tx-commit@v1", json!({ "id": id })).await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("no open transaction"), "{:?}", out.logs);

        // close, then the id is gone
        let out = run(&ctx, "std/db-close@v1", json!({ "id": id })).await;
        assert!(out.success, "close: {:?}", out.logs);
        assert_eq!(out.value["closed"], true);
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT 1" }),
        )
        .await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("unknown connection id"), "{:?}", out.logs);
    }

    #[tokio::test]
    async fn sqlite_constraint_violation_is_classified() {
        let ctx = Context::new();
        let out = run(
            &ctx,
            "std/db-connect@v1",
            json!({ "driver": "sqlite", "dsn": "sqlite::memory:" }),
        )
        .await;
        let id = out.value["id"].as_str().unwrap().to_string();

        run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "CREATE TABLE u (email TEXT UNIQUE)" }),
        )
        .await;
        run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "INSERT INTO u VALUES ('a@x.io')" }),
        )
        .await;
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "INSERT INTO u VALUES ('a@x.io')" }),
        )
        .await;
        assert!(!out.success, "duplicate must fail: {:?}", out.value);
        assert_eq!(out.value["error_kind"], "constraint");
        assert_eq!(out.value["metrics"]["db_errors"], 1);
        assert_eq!(out.value["metrics"]["db_errors_constraint"], 1);
        assert!(out.value["metrics"]["db_query_duration"].is_array());
        // A failed query does not kill the connection.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT count(*) AS c FROM u" }),
        )
        .await;
        assert!(out.success);
        assert_eq!(out.value["data"][0]["c"], 1);
    }

    #[tokio::test]
    async fn sqlite_sql_error_is_other_and_never_echoes_sql() {
        let ctx = Context::new();
        let out = run(
            &ctx,
            "std/db-connect@v1",
            json!({ "driver": "sqlite", "dsn": "sqlite::memory:" }),
        )
        .await;
        let id = out.value["id"].as_str().unwrap().to_string();
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT * FROM no_such_table" }),
        )
        .await;
        assert!(!out.success);
        assert_eq!(out.value["error_kind"], "other");
        assert_eq!(out.value["metrics"]["db_errors_other"], 1);
    }

    #[tokio::test]
    async fn sqlite_query_text_is_never_interpolated() {
        let mut ctx = Context::new();
        ctx.set("who", json!("SHOULD_NOT_APPEAR"));
        let out = run(
            &ctx,
            "std/db-connect@v1",
            json!({ "driver": "sqlite", "dsn": "sqlite::memory:" }),
        )
        .await;
        ctx.set("conn", out.value.clone());

        run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": "${{ conn.id }}", "query": "CREATE TABLE t (v TEXT)" }),
        )
        .await;
        // The `${{ who }}` inside the SQL text must reach SQLite verbatim.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": "${{ conn.id }}", "query": "INSERT INTO t VALUES ('${{ who }}')" }),
        )
        .await;
        assert!(out.success, "insert literal: {:?}", out.logs);
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": "${{ conn.id }}", "query": "SELECT v FROM t" }),
        )
        .await;
        assert_eq!(out.value["data"][0]["v"], "${{ who }}");
    }

    #[tokio::test]
    async fn sqlite_per_query_mode_on_file_dsn() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let dsn = format!("sqlite://{}?mode=rwc", file.path().display());
        let ctx = Context::new();

        // Seed the file through a persistent connection first.
        let out = run(
            &ctx,
            "std/db-connect@v1",
            json!({ "driver": "sqlite", "dsn": dsn }),
        )
        .await;
        assert!(out.success, "seed connect: {:?}", out.logs);
        let seed_id = out.value["id"].as_str().unwrap().to_string();
        run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": seed_id, "query": "CREATE TABLE t (name TEXT)" }),
        )
        .await;
        run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": seed_id, "query": "INSERT INTO t VALUES ('durable')" }),
        )
        .await;
        run(&ctx, "std/db-close@v1", json!({ "id": seed_id })).await;

        // per-query: the connect step stores the config, no connection yet.
        let out = run(
            &ctx,
            "std/db-connect@v1",
            json!({ "driver": "sqlite", "dsn": dsn, "mode": "per-query" }),
        )
        .await;
        assert!(out.success, "per-query connect: {:?}", out.logs);
        assert_eq!(out.value["mode"], "per-query");
        assert_eq!(out.value["connected"], false);
        let id = out.value["id"].as_str().unwrap().to_string();

        // Each query opens a fresh connection — and sees the seeded data.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT name FROM t" }),
        )
        .await;
        assert!(out.success, "per-query select: {:?}", out.logs);
        assert_eq!(out.value["rows"], 1);
        assert_eq!(out.value["data"][0]["name"], "durable");
        assert!(out.value["metrics"]["db_query_duration"].is_array());

        // Writes through fresh connections persist too.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "INSERT INTO t VALUES ('second')" }),
        )
        .await;
        assert!(out.success);
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT count(*) AS c FROM t" }),
        )
        .await;
        assert_eq!(out.value["data"][0]["c"], 2);

        // Transactions are rejected on a per-query id.
        let out = run(&ctx, "std/db-tx-begin@v1", json!({ "id": id })).await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("mode: persistent"), "{:?}", out.logs);
        // Commit/rollback likewise find no transaction to finish — a clean
        // error, not a panic on the missing pool.
        for action in ["std/db-tx-commit@v1", "std/db-tx-rollback@v1"] {
            let out = run(&ctx, action, json!({ "id": id })).await;
            assert!(!out.success, "{action} on per-query id must fail");
            assert!(out.logs[0].1.contains("no open transaction"), "{action}: {:?}", out.logs);
        }

        let out = run(&ctx, "std/db-close@v1", json!({ "id": id })).await;
        assert!(out.success);
    }

    #[tokio::test]
    async fn sqlite_bind_param_type_errors() {
        let ctx = Context::new();
        let out = run(
            &ctx,
            "std/db-connect@v1",
            json!({ "driver": "sqlite", "dsn": "sqlite::memory:" }),
        )
        .await;
        let id = out.value["id"].as_str().unwrap().to_string();

        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT ?", "params": [[1, 2]] }),
        )
        .await;
        assert!(!out.success);
        assert!(out.value["error"].as_str().unwrap().contains("cannot be bound"), "{:?}", out.value);
        assert_eq!(out.value["error_kind"], "other");
    }

    // -----------------------------------------------------------------
    // Server-backed integration (gated — see the module doc)
    // -----------------------------------------------------------------

    fn gated_dsn(var: &str) -> Option<String> {
        std::env::var(var).ok().filter(|s| !s.is_empty())
    }

    /// The shared server-backed flow: connect → create → insert with bind →
    /// select (driver-native placeholder) → constraint classification →
    /// close. `placeholder` is "$1" for PostgreSQL, "?" for MySQL.
    async fn server_flow(driver: &str, dsn: &str, placeholder: &str) {
        let ctx = Context::new();
        let out = run(
            &ctx,
            "std/db-connect@v1",
            json!({ "driver": driver, "dsn": dsn, "tls": false }),
        )
        .await;
        assert!(out.success, "connect: {:?}", out.logs);
        assert_eq!(out.value["connected"], true);
        assert!(out.value["metrics"]["db_connect_duration"].is_array());
        let id = out.value["id"].as_str().unwrap().to_string();

        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "DROP TABLE IF EXISTS perfscale_db_test" }),
        )
        .await;
        assert!(out.success, "drop: {:?}", out.logs);
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "CREATE TABLE perfscale_db_test (id INTEGER PRIMARY KEY, name TEXT)" }),
        )
        .await;
        assert!(out.success, "create: {:?}", out.logs);

        // Placeholder numbering differs: pg $1/$2, mysql ?/?.
        let insert_sql = if placeholder == "$1" {
            "INSERT INTO perfscale_db_test (id, name) VALUES ($1, $2)".to_string()
        } else {
            "INSERT INTO perfscale_db_test (id, name) VALUES (?, ?)".to_string()
        };
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": insert_sql, "params": [1, "pg-row"] }),
        )
        .await;
        assert!(out.success, "insert: {:?}", out.logs);
        assert_eq!(out.value["rows_affected"], 1);

        let select_sql = if placeholder == "$1" {
            "SELECT id, name FROM perfscale_db_test WHERE id = $1"
        } else {
            "SELECT id, name FROM perfscale_db_test WHERE id = ?"
        };
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": select_sql, "params": [1] }),
        )
        .await;
        assert!(out.success, "select: {:?}", out.logs);
        assert_eq!(out.value["rows"], 1);
        assert_eq!(out.value["data"][0]["name"], "pg-row");
        assert_eq!(out.value["metrics"]["db_rows"], 1);

        // tx round trip
        let out = run(&ctx, "std/db-tx-begin@v1", json!({ "id": id })).await;
        assert!(out.success, "tx-begin: {:?}", out.logs);
        let insert2 = if placeholder == "$1" {
            "INSERT INTO perfscale_db_test (id, name) VALUES ($1, $2)"
        } else {
            "INSERT INTO perfscale_db_test (id, name) VALUES (?, ?)"
        };
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": insert2, "params": [2, "tx-row"] }),
        )
        .await;
        assert!(out.success, "tx insert: {:?}", out.logs);
        let out = run(&ctx, "std/db-tx-commit@v1", json!({ "id": id })).await;
        assert!(out.success, "commit: {:?}", out.logs);
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT count(*) AS c FROM perfscale_db_test" }),
        )
        .await;
        assert_eq!(out.value["data"][0]["c"], 2, "committed rows visible");

        // constraint classification on a duplicate primary key
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": insert2, "params": [1, "dup"] }),
        )
        .await;
        assert!(!out.success, "duplicate pk must fail: {:?}", out.value);
        assert_eq!(out.value["error_kind"], "constraint", "{:?}", out.value);
        assert_eq!(out.value["metrics"]["db_errors_constraint"], 1);

        // Bind-count mismatches are rejected server-side (unlike SQLite,
        // which binds NULL/ignores extras) — a clean classified error.
        let one_ph = if placeholder == "$1" {
            "SELECT id FROM perfscale_db_test WHERE id = $1"
        } else {
            "SELECT id FROM perfscale_db_test WHERE id = ?"
        };
        for params in [json!([]), json!([1, 2])] {
            let out = run(
                &ctx,
                "std/db-query@v1",
                json!({ "id": id, "query": one_ph, "params": params }),
            )
            .await;
            assert!(!out.success, "{driver} count mismatch {params} must fail");
            assert_eq!(out.value["metrics"]["db_errors"], 1, "{params}: {:?}", out.value);
        }

        // Empty result set: rows 0, empty data, no error.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT id FROM perfscale_db_test WHERE 1 = 0" }),
        )
        .await;
        assert!(out.success, "empty select: {:?}", out.value);
        assert_eq!(out.value["rows"], 0);
        assert_eq!(out.value["data"], json!([]));

        // Value decoding per the module doc's mapping: NULL → null, unicode
        // text intact, binary → base64, i64-max integer, f64.
        let decode_sql = if placeholder == "$1" {
            "SELECT NULL::text AS n, 'héllo 🦀'::text AS u, '\\x00ff10'::bytea AS b, \
             9223372036854775807::int8 AS big, 3.141592653589793::float8 AS f"
        } else {
            "SELECT NULL AS n, 'héllo 🦀' AS u, X'00FF10' AS b, \
             9223372036854775807 AS big, 3.141592653589793 AS f"
        };
        let out = run(&ctx, "std/db-query@v1", json!({ "id": id, "query": decode_sql })).await;
        assert!(out.success, "decode select: {:?}", out.value);
        let row = &out.value["data"][0];
        assert_eq!(row["n"], Value::Null, "{driver} NULL → null");
        assert_eq!(row["u"], "héllo 🦀", "{driver} unicode intact");
        assert_eq!(row["b"], "AP8Q", "{driver} binary → base64");
        assert_eq!(row["big"], i64::MAX, "{driver} bigint");
        assert_eq!(row["f"], std::f64::consts::PI, "{driver} float");

        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "DROP TABLE perfscale_db_test" }),
        )
        .await;
        assert!(out.success, "cleanup: {:?}", out.logs);
        let out = run(&ctx, "std/db-close@v1", json!({ "id": id })).await;
        assert!(out.success);
    }

    #[tokio::test]
    async fn postgres_flow_gated() {
        let Some(dsn) = gated_dsn("PERFSCALE_TEST_PG_DSN") else {
            eprintln!("skipped: PERFSCALE_TEST_PG_DSN not set (see step/db.rs module doc)");
            return;
        };
        server_flow("postgres", &dsn, "$1").await;
    }

    #[tokio::test]
    async fn mysql_flow_gated() {
        let Some(dsn) = gated_dsn("PERFSCALE_TEST_MYSQL_DSN") else {
            eprintln!("skipped: PERFSCALE_TEST_MYSQL_DSN not set (see step/db.rs module doc)");
            return;
        };
        server_flow("mysql", &dsn, "?").await;
    }

    /// The BEGIN/COMMIT wrap machinery, exercised against a direct server.
    /// The pooler-split detection that triggers it live needs a real
    /// transaction-mode pooler (PgBouncer/Supavisor) and is covered by live
    /// runs only — no fake can reproduce a backend swap.
    #[tokio::test]
    async fn postgres_pooler_wrap_gated() {
        let Some(dsn) = gated_dsn("PERFSCALE_TEST_PG_DSN") else {
            eprintln!("skipped: PERFSCALE_TEST_PG_DSN not set (see step/db.rs module doc)");
            return;
        };
        let opts = sqlx::postgres::PgConnectOptions::from_str(&dsn)
            .unwrap()
            .ssl_mode(sqlx::postgres::PgSslMode::Disable);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        // QueryOutcome has no Debug — unwrap with the sqlx error as context.
        fn unwrap_outcome(r: Result<QueryOutcome, sqlx::Error>) -> QueryOutcome {
            r.unwrap_or_else(|e| panic!("query failed: {e}"))
        }

        // Plain path on a direct server: works, and never learns the wrap.
        let (out, learned) =
            run_pg_pool_autocommit(&pool, false, "DROP TABLE IF EXISTS perfscale_wrap_test", &[], 10)
                .await;
        unwrap_outcome(out);
        assert!(!learned, "direct server must not learn the wrap");
        let (out, learned) = run_pg_pool_autocommit(
            &pool,
            false,
            "CREATE TABLE perfscale_wrap_test (n INTEGER)",
            &[],
            10,
        )
        .await;
        unwrap_outcome(out);
        assert!(!learned);

        // Forced wrap (the state learned on a pooler): insert + read back.
        let (out, learned) = run_pg_pool_autocommit(
            &pool,
            true,
            "INSERT INTO perfscale_wrap_test VALUES ($1)",
            &[json!(1)],
            10,
        )
        .await;
        unwrap_outcome(out);
        assert!(learned);
        let (out, learned) =
            run_pg_pool_autocommit(&pool, true, "SELECT n FROM perfscale_wrap_test", &[], 10).await;
        assert!(learned);
        let out = unwrap_outcome(out);
        assert_eq!(out.rows, 1);
        assert_eq!(out.data[0]["n"], 1);

        // A genuine error still errors when wrapped — and is never learned.
        let (out, learned) =
            run_pg_pool_autocommit(&pool, true, "SELECT * FROM no_such_table", &[], 10).await;
        assert!(out.is_err());
        assert!(!learned);

        let (out, _) =
            run_pg_pool_autocommit(&pool, false, "DROP TABLE perfscale_wrap_test", &[], 10).await;
        unwrap_outcome(out);
    }

    #[test]
    fn connect_params_pool_size_is_clamped() {
        // 0 would make a pool that can never hand out a connection; the
        // ceiling keeps a typo from opening a thousand sockets.
        for (given, want) in [(0, 1), (1, 1), (9999, 1024), (1024, 1024)] {
            let cp = parse_connect_params(&json!({
                "driver": "sqlite",
                "dsn": "sqlite::memory:",
                "pool_size": given,
            }))
            .unwrap();
            assert_eq!(cp.pool_size, want, "pool_size {given}");
        }
        // Interpolated string form clamps the same way.
        let cp = parse_connect_params(&json!({
            "driver": "sqlite",
            "dsn": "sqlite::memory:",
            "pool_size": "0",
        }))
        .unwrap();
        assert_eq!(cp.pool_size, 1);
    }

    #[test]
    fn sanitize_detail_does_not_mangle_unrelated_words() {
        // A one-character password must not substring-replace its way through
        // the message ("open" → "o[redacted]en" was the bug): only whole-word
        // occurrences are scrubbed.
        let dsn = "postgres://u:p@h/db";
        let detail = "unable to open database file";
        assert_eq!(sanitize_detail(detail, dsn), detail);

        // …while a password echoed on its own is still scrubbed — quotes and
        // punctuation count as word boundaries.
        let dsn = "postgres://u:s3cret@h/db";
        assert_eq!(
            sanitize_detail("authentication failed for 's3cret'", dsn),
            "authentication failed for '[redacted]'"
        );
        assert_eq!(
            sanitize_detail("pwd=s3cret, try again", dsn),
            "pwd=[redacted], try again"
        );
        // A password that is only a substring of a longer word stays (it is
        // not the password — a partial scrub would corrupt the message).
        assert_eq!(
            sanitize_detail("unknown database s3cretdb", dsn),
            "unknown database s3cretdb"
        );
    }

    #[test]
    fn queries_use_unnamed_statements() {
        // PgBouncer-style transaction poolers swap server backends under the
        // client connection, so a cached NAMED statement collides on the
        // shared backend ("prepared statement \"sqlx_s_1\" already exists").
        // step_query must therefore build non-persistent (unnamed)
        // statements — observable via the Execute::persistent flag. (The
        // backend-split retry this enables is covered by live pooler runs;
        // it cannot be reproduced with a local server.)
        fn is_persistent<DB>(sql: &str) -> bool
        where
            DB: sqlx::Database + sqlx::database::HasStatementCache,
            for<'q> <DB as sqlx::Database>::Arguments<'q>: sqlx::IntoArguments<'q, DB>,
        {
            sqlx::Execute::persistent(&step_query::<DB>(sql))
        }
        assert!(!is_persistent::<sqlx::Postgres>("SELECT $1"));
        assert!(!is_persistent::<sqlx::MySql>("SELECT ?"));
        assert!(!is_persistent::<sqlx::Sqlite>("SELECT ?"));
    }

    /// Minimal `DatabaseError` with a settable code, for classification
    /// tests that need no server.
    #[derive(Debug)]
    struct FakeDbErr(&'static str);

    impl std::fmt::Display for FakeDbErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for FakeDbErr {}

    impl sqlx::error::DatabaseError for FakeDbErr {
        fn message(&self) -> &str {
            "fake db error"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(std::borrow::Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    #[test]
    fn pooler_split_sqlstates_are_recognized() {
        let db_err = |code| sqlx::Error::Database(Box::new(FakeDbErr(code)));
        // The three pre-execution pooler-split signatures…
        assert!(is_pg_pooler_split(&db_err("42P05"))); // already exists
        assert!(is_pg_pooler_split(&db_err("26000"))); // does not exist
        assert!(is_pg_pooler_split(&db_err("08P01"))); // bind param mismatch
        // …and nothing else: real SQL/user errors must NOT trigger a retry.
        assert!(!is_pg_pooler_split(&db_err("42601"))); // syntax error
        assert!(!is_pg_pooler_split(&db_err("23505"))); // unique violation
        assert!(!is_pg_pooler_split(&db_err("57014"))); // query canceled
        assert!(!is_pg_pooler_split(&sqlx::Error::RowNotFound));
        assert!(!is_pg_pooler_split(&sqlx::Error::PoolTimedOut));
    }

    // -----------------------------------------------------------------
    // SQLite edge cases (no server needed)
    // -----------------------------------------------------------------

    /// Fresh in-memory connection, returning its id.
    async fn connect_memory(ctx: &Context) -> String {
        let out = run(
            ctx,
            "std/db-connect@v1",
            json!({ "driver": "sqlite", "dsn": "sqlite::memory:" }),
        )
        .await;
        assert!(out.success, "connect: {:?}", out.logs);
        out.value["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn sqlite_bind_param_count_mismatch_does_not_panic() {
        // SQLite semantics (sqlite3 C API): an unbound placeholder reads as
        // NULL and extra binds are ignored — a count mismatch is NOT an
        // error there. (PostgreSQL/MySQL reject mismatches server-side; see
        // the gated server_flow.) What must never happen is a panic.
        let ctx = Context::new();
        let id = connect_memory(&ctx).await;

        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT ? AS a, ? AS b", "params": [1] }),
        )
        .await;
        assert!(out.success, "fewer binds than placeholders: {:?}", out.value);
        assert_eq!(out.value["data"][0]["a"], 1);
        assert_eq!(out.value["data"][0]["b"], Value::Null, "unbound placeholder is NULL");

        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT ? AS a", "params": [1, 2] }),
        )
        .await;
        assert!(out.success, "more binds than placeholders: {:?}", out.value);
        assert_eq!(out.value["data"][0]["a"], 1);
    }

    #[tokio::test]
    async fn sqlite_empty_result_set_is_ok() {
        let ctx = Context::new();
        let id = connect_memory(&ctx).await;
        run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "CREATE TABLE e (x INTEGER)" }),
        )
        .await;
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT x FROM e WHERE x > 100" }),
        )
        .await;
        assert!(out.success, "empty select: {:?}", out.value);
        assert_eq!(out.value["rows"], 0);
        assert_eq!(out.value["data"], json!([]));
        assert_eq!(out.value["truncated"], false);
        // Metrics are still emitted; on a fresh connection the SQLite
        // previous-DML change-count quirk is 0, so db_rows reads 0.
        assert_eq!(out.value["metrics"]["db_rows"], 0);
        assert!(out.value["metrics"]["db_query_duration"].is_array());
    }

    #[tokio::test]
    async fn sqlite_row_value_decoding() {
        let ctx = Context::new();
        let id = connect_memory(&ctx).await;
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({
                "id": id,
                "query": "SELECT NULL AS n, 'héllo 🦀' AS u, X'00FF10' AS b, \
                          9223372036854775807 AS big, -9223372036854775808 AS small, \
                          3.141592653589793 AS f",
            }),
        )
        .await;
        assert!(out.success, "decode select: {:?}", out.value);
        let row = &out.value["data"][0];
        assert_eq!(row["n"], Value::Null, "NULL → null");
        assert_eq!(row["u"], "héllo 🦀", "unicode text survives");
        assert_eq!(row["b"], "AP8Q", "blob → base64");
        assert_eq!(row["big"], i64::MAX);
        assert_eq!(row["small"], i64::MIN);
        assert_eq!(row["f"], std::f64::consts::PI, "f64 round-trips");

        // The same mapping applies to values bound through params.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT ? AS u, ? AS n", "params": ["🦀αβ", null] }),
        )
        .await;
        assert!(out.success);
        assert_eq!(out.value["data"][0]["u"], "🦀αβ");
        assert_eq!(out.value["data"][0]["n"], Value::Null);
    }

    #[tokio::test]
    async fn sqlite_query_timeout_is_classified() {
        let ctx = Context::new();
        let id = connect_memory(&ctx).await;
        // A recursive CTE summing 10M rows cannot finish in 50ms on any
        // hardware (that would need >200M rows/s through SQLite's CTE
        // machinery), so the step timeout always fires first.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({
                "id": id,
                "query": "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt LIMIT 10000000) SELECT sum(x) FROM cnt",
                "timeout_ms": 50,
            }),
        )
        .await;
        assert!(!out.success, "heavy query must time out: {:?}", out.value);
        assert_eq!(out.value["error_kind"], "timeout");
        assert_eq!(out.value["metrics"]["db_errors"], 1);
        assert_eq!(out.value["metrics"]["db_errors_timeout"], 1);
        assert!(out.value["metrics"]["db_query_duration"].is_array());
        assert!(out.value["error"].as_str().unwrap().contains("TIMEOUT"), "{:?}", out.value);
    }

    #[tokio::test]
    async fn sqlite_multi_statement_in_one_query() {
        let ctx = Context::new();
        let id = connect_memory(&ctx).await;

        // Documented behavior: one query text may carry several statements;
        // they run in order over one stream.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "CREATE TABLE a (x INTEGER); CREATE TABLE b (y INTEGER)" }),
        )
        .await;
        assert!(out.success, "multi-ddl: {:?}", out.value);

        // rows_affected sums across the statements; binds apply in order.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({
                "id": id,
                "query": "INSERT INTO a VALUES (?); INSERT INTO b VALUES (?)",
                "params": [10, 20],
            }),
        )
        .await;
        assert!(out.success, "multi-dml: {:?}", out.value);
        assert_eq!(out.value["rows_affected"], 2);

        // Rows from every SELECT in the text are collected into one `data`.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT x AS v FROM a; SELECT y AS v FROM b" }),
        )
        .await;
        assert!(out.success, "multi-select: {:?}", out.value);
        let vals: Vec<&Value> = out.value["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| &r["v"])
            .collect();
        assert!(vals.contains(&&json!(10)) && vals.contains(&&json!(20)), "{vals:?}");
    }

    #[tokio::test]
    async fn sqlite_max_rows_edge_values() {
        let ctx = Context::new();
        let id = connect_memory(&ctx).await;
        run(&ctx, "std/db-query@v1", json!({ "id": id, "query": "CREATE TABLE m (n INTEGER)" })).await;
        for n in 1..=3 {
            run(
                &ctx,
                "std/db-query@v1",
                json!({ "id": id, "query": "INSERT INTO m VALUES (?)", "params": [n] }),
            )
            .await;
        }

        // max_rows 0 collects nothing; the first row only flips `truncated`.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT n FROM m", "max_rows": 0 }),
        )
        .await;
        assert!(out.success, "max_rows 0: {:?}", out.value);
        assert_eq!(out.value["rows"], 0);
        assert_eq!(out.value["data"], json!([]));
        assert_eq!(out.value["truncated"], true);

        // A huge cap is just a cap — no overflow, no panic, all rows.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT n FROM m", "max_rows": u64::MAX }),
        )
        .await;
        assert!(out.success, "max_rows u64::MAX: {:?}", out.value);
        assert_eq!(out.value["rows"], 3);
        assert_eq!(out.value["truncated"], false);
    }

    #[tokio::test]
    async fn sqlite_strict_table_rejects_type_mismatch() {
        let ctx = Context::new();
        let id = connect_memory(&ctx).await;

        // Plain tables have type AFFINITY: a string into an INTEGER column
        // is not an error (SQLite stores it as TEXT when it does not look
        // numeric) — dynamic typing is documented SQLite behavior.
        run(&ctx, "std/db-query@v1", json!({ "id": id, "query": "CREATE TABLE ns (n INTEGER)" })).await;
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "INSERT INTO ns VALUES (?)", "params": ["abc"] }),
        )
        .await;
        assert!(out.success, "plain table accepts affinity mismatch: {:?}", out.value);

        // STRICT tables turn the same insert into a datatype constraint
        // violation (SQLITE_CONSTRAINT_DATATYPE, low byte 19 → constraint).
        run(&ctx, "std/db-query@v1", json!({ "id": id, "query": "CREATE TABLE s (n INTEGER) STRICT" })).await;
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "INSERT INTO s VALUES (?)", "params": ["not-a-number"] }),
        )
        .await;
        assert!(!out.success, "STRICT mismatch must fail: {:?}", out.value);
        assert_eq!(out.value["error_kind"], "constraint");
        assert_eq!(out.value["metrics"]["db_errors_constraint"], 1);

        // The connection survives the failed statement.
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id, "query": "SELECT count(*) AS c FROM s" }),
        )
        .await;
        assert!(out.success);
        assert_eq!(out.value["data"][0]["c"], 0);
    }

    #[tokio::test]
    async fn sqlite_rollback_without_open_tx_errors() {
        let ctx = Context::new();
        let id = connect_memory(&ctx).await;
        let out = run(&ctx, "std/db-tx-rollback@v1", json!({ "id": id })).await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("no open transaction"), "{:?}", out.logs);
    }

    #[tokio::test]
    async fn sqlite_close_with_open_tx_rolls_back() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let dsn = format!("sqlite://{}?mode=rwc", file.path().display());
        let ctx = Context::new();

        let out = run(&ctx, "std/db-connect@v1", json!({ "driver": "sqlite", "dsn": dsn })).await;
        let id = out.value["id"].as_str().unwrap().to_string();
        run(&ctx, "std/db-query@v1", json!({ "id": id, "query": "CREATE TABLE t (x INTEGER)" })).await;

        // Begin a tx, write inside it, then close WITHOUT committing.
        run(&ctx, "std/db-tx-begin@v1", json!({ "id": id })).await;
        run(&ctx, "std/db-query@v1", json!({ "id": id, "query": "INSERT INTO t VALUES (1)" })).await;
        // db-close drops the open transaction (rolling it back) before
        // closing the pool — awaiting the close with the tx still checked
        // out used to hang, so bound the step: a regression fails here
        // instead of sticking the whole test run.
        let closed = tokio::time::timeout(
            Duration::from_secs(10),
            run(&ctx, "std/db-close@v1", json!({ "id": id })),
        )
        .await
        .expect("db-close with an open transaction must not hang");
        assert!(closed.success, "close: {:?}", closed.logs);
        assert_eq!(closed.value["closed"], true);

        // A fresh connection sees none of the uncommitted data.
        let out = run(&ctx, "std/db-connect@v1", json!({ "driver": "sqlite", "dsn": dsn })).await;
        let id2 = out.value["id"].as_str().unwrap().to_string();
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id2, "query": "SELECT count(*) AS c FROM t" }),
        )
        .await;
        assert!(out.success, "reconnect: {:?}", out.logs);
        assert_eq!(out.value["data"][0]["c"], 0, "uncommitted insert was rolled back");
    }

    #[tokio::test]
    async fn sqlite_drain_with_open_tx_rolls_back() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let dsn = format!("sqlite://{}?mode=rwc", file.path().display());
        let ctx = Context::new();

        let out = run(&ctx, "std/db-connect@v1", json!({ "driver": "sqlite", "dsn": dsn })).await;
        let id = out.value["id"].as_str().unwrap().to_string();
        run(&ctx, "std/db-query@v1", json!({ "id": id, "query": "CREATE TABLE t (x INTEGER)" })).await;
        run(&ctx, "std/db-query@v1", json!({ "id": id, "query": "INSERT INTO t VALUES (1)" })).await;

        // Iteration-end cleanup with an uncommitted tx parked: the drain
        // drops the connection, and SQLite rolls the tx back on drop.
        run(&ctx, "std/db-tx-begin@v1", json!({ "id": id })).await;
        run(&ctx, "std/db-query@v1", json!({ "id": id, "query": "INSERT INTO t VALUES (2)" })).await;
        assert_eq!(ctx.resources.drain(), 1);

        let out = run(&ctx, "std/db-connect@v1", json!({ "driver": "sqlite", "dsn": dsn })).await;
        let id2 = out.value["id"].as_str().unwrap().to_string();
        let out = run(
            &ctx,
            "std/db-query@v1",
            json!({ "id": id2, "query": "SELECT count(*) AS c FROM t" }),
        )
        .await;
        assert!(out.success, "reconnect: {:?}", out.logs);
        assert_eq!(out.value["data"][0]["c"], 1, "only the committed row survives");
    }

    #[tokio::test]
    async fn sqlite_locked_write_is_classified_deadlock() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let dsn = format!("sqlite://{}?mode=rwc", file.path().display());
        let ctx = Context::new();

        let out = run(&ctx, "std/db-connect@v1", json!({ "driver": "sqlite", "dsn": dsn })).await;
        let id_a = out.value["id"].as_str().unwrap().to_string();
        let out = run(&ctx, "std/db-connect@v1", json!({ "driver": "sqlite", "dsn": dsn })).await;
        let id_b = out.value["id"].as_str().unwrap().to_string();
        run(&ctx, "std/db-query@v1", json!({ "id": id_a, "query": "CREATE TABLE l (x INTEGER)" })).await;

        // No waiting on the loser: the lock conflict surfaces immediately as
        // SQLITE_BUSY (low byte 5 → deadlock) instead of after a busy-timeout.
        let out = run(&ctx, "std/db-query@v1", json!({ "id": id_b, "query": "PRAGMA busy_timeout = 0" })).await;
        assert!(out.success, "pragma: {:?}", out.logs);

        // A holds an open write transaction…
        run(&ctx, "std/db-tx-begin@v1", json!({ "id": id_a })).await;
        let out = run(&ctx, "std/db-query@v1", json!({ "id": id_a, "query": "INSERT INTO l VALUES (1)" })).await;
        assert!(out.success);
        // …so B's write conflicts with the held lock.
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            run(&ctx, "std/db-query@v1", json!({ "id": id_b, "query": "INSERT INTO l VALUES (2)" })),
        )
        .await
        .expect("locked write must fail fast, not hang");
        assert!(!out.success, "conflicting write must fail: {:?}", out.value);
        assert_eq!(out.value["error_kind"], "deadlock", "{:?}", out.value);
        assert_eq!(out.value["metrics"]["db_errors_deadlock"], 1);

        // B's connection is still usable once A lets go.
        run(&ctx, "std/db-tx-rollback@v1", json!({ "id": id_a })).await;
        let out = run(&ctx, "std/db-query@v1", json!({ "id": id_b, "query": "INSERT INTO l VALUES (2)" })).await;
        assert!(out.success, "write succeeds after rollback: {:?}", out.logs);
    }

    #[tokio::test]
    async fn connect_malformed_dsn_errors_are_clean() {
        let ctx = Context::new();

        // sqlx-sqlite accepts a bare filename as a DSN, so garbage and
        // wrong-scheme strings fail at open time rather than at parse — a
        // clean, classified config error either way.
        for dsn in ["garbage string with spaces", "postgres://u:p@h/db"] {
            let out = run(&ctx, "std/db-connect@v1", json!({ "driver": "sqlite", "dsn": dsn })).await;
            assert!(!out.success, "{dsn} must fail");
            assert_eq!(out.value["error_kind"], "other", "{dsn}: {:?}", out.value);
            assert_eq!(out.value["metrics"]["db_errors_other"], 1);
            let detail = out.value["error"].as_str().unwrap();
            assert!(detail.contains("unable to open database file"), "{dsn}: {detail}");
            assert!(!detail.contains("postgres://"), "dsn leaked: {detail}");
        }

        // PostgreSQL rejects garbage at DSN parse time.
        let out = run(
            &ctx,
            "std/db-connect@v1",
            json!({ "driver": "postgres", "dsn": "not a dsn at all" }),
        )
        .await;
        assert!(!out.success);
        assert_eq!(out.value["error_kind"], "other");
        assert!(
            out.value["error"].as_str().unwrap().contains("invalid dsn for driver 'postgres'"),
            "{:?}",
            out.value
        );
    }

    // -----------------------------------------------------------------
    // probe_db — the editor connectivity probe (crate::introspect)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn probe_db_sqlite_memory_ok() {
        let latency = probe_db("sqlite", "sqlite::memory:", "false", 5_000)
            .await
            .expect("memory probe connects");
        assert!(latency < 5_000, "{latency}");
    }

    #[tokio::test]
    async fn probe_db_sqlite_file_ok() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let dsn = format!("sqlite://{}?mode=rwc", file.path().display());
        let latency = probe_db("sqlite", &dsn, "true", 5_000)
            .await
            .expect("file probe connects (tls is ignored for sqlite)");
        assert!(latency < 5_000, "{latency}");
    }

    #[tokio::test]
    async fn probe_db_rejects_bad_driver_and_tls() {
        let err = probe_db("oracle", "sqlite::memory:", "false", 5_000)
            .await
            .expect_err("unknown driver");
        assert!(err.contains("unknown driver 'oracle'"), "{err}");

        let err = probe_db("sqlite", "sqlite::memory:", "maybe", 5_000)
            .await
            .expect_err("bad tls value");
        assert!(err.contains("'tls' must be"), "{err}");
    }

    #[tokio::test]
    async fn probe_db_bad_dsn_error_is_sanitized() {
        let dsn = "postgres://user:sup3rs3cret@?bad dsn";
        let err = probe_db("postgres", dsn, "false", 5_000)
            .await
            .expect_err("garbage dsn fails at parse");
        assert!(err.contains("invalid dsn for driver 'postgres'"), "{err}");
        assert!(!err.contains("sup3rs3cret"), "password leaked: {err}");
        assert!(!err.contains(dsn), "dsn leaked: {err}");
    }

    #[tokio::test]
    async fn probe_db_refused_connection_errors_without_leaking() {
        let dsn = "postgres://user:sup3rs3cret@127.0.0.1:1/db";
        let err = probe_db("postgres", dsn, "false", 2_000)
            .await
            .expect_err("nothing listens on port 1");
        assert!(!err.is_empty());
        assert!(!err.contains("sup3rs3cret"), "password leaked: {err}");
    }
}
