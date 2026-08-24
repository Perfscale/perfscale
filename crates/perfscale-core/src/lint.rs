//! YAML linting for test-definition and config files.
//!
//! Goes beyond schema validation: every issue carries *where* it is, *what*
//! is wrong, and — where we can tell — *what to use instead* (including
//! did-you-mean suggestions for typo'd field names, which plain schema
//! validation cannot express because unknown fields are legal for forward
//! compatibility at run time).

use serde_json::Value;

/// One problem found in a document.
#[derive(Debug, Clone, PartialEq)]
pub struct LintIssue {
    /// JSON-pointer-ish location, e.g. `/steps/0` or `(file)`.
    pub location: String,
    /// What is wrong.
    pub problem: String,
    /// What to use instead, when we can tell.
    pub suggestion: Option<String>,
}

/// Which schema a document should be linted against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Test,
    Config,
}

/// Guess the document kind: a mapping with a `steps` key is a test
/// definition, anything else is treated as a config.
pub fn detect_kind(yaml: &str) -> DocKind {
    match serde_yaml::from_str::<Value>(yaml) {
        Ok(v) if v.get("steps").is_some() => DocKind::Test,
        _ => DocKind::Config,
    }
}

/// Lint a document. Empty result = valid.
pub fn lint(yaml: &str, kind: DocKind) -> Vec<LintIssue> {
    let value: Value = match serde_yaml::from_str(yaml) {
        Ok(v) => v,
        Err(e) => {
            return vec![LintIssue {
                location: "(file)".into(),
                problem: format!("invalid YAML: {e}"),
                suggestion: Some(
                    "check indentation and quoting — every step list item starts with `- `".into(),
                ),
            }];
        }
    };

    let mut issues = Vec::new();
    schema_issues(&value, kind, &mut issues);
    match kind {
        DocKind::Test => lint_test_fields(&value, &mut issues),
        DocKind::Config => lint_config_fields(&value, &mut issues),
    }
    issues
}

/// Network pass for `std/graphql@v1` steps, run by `perfscale lint` after the
/// offline checks: fetch each endpoint's schema via introspection and
/// validate the step's literal query against it — the same gate the run
/// itself applies. Steps with a `schema_file` are validated against the local
/// SDL instead; that part is offline by nature and runs even when `offline`
/// is set (only introspection fetching is skipped then).
///
/// Returns `(issues, notes)`: notes are advisory lines for endpoints that
/// could not be introspected (target down, introspection disabled) and never
/// affect the lint exit code. Interpolated `${{ … }}` values skip the step —
/// they resolve only at run time.
pub async fn lint_graphql_remote(yaml: &str, offline: bool) -> (Vec<LintIssue>, Vec<String>) {
    use crate::step::graphql::{
        introspect_schema, schema_from_sdl, validate_against_schema, GraphqlSchema,
    };

    let mut issues = Vec::new();
    let mut notes = Vec::new();
    let Ok(value) = serde_yaml::from_str::<Value>(yaml) else {
        return (issues, notes);
    };
    let Some(steps) = value.get("steps").and_then(|s| s.as_array()) else {
        return (issues, notes);
    };

    fn literal<'a>(with: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
        with.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.contains("${{"))
    }

    let client = reqwest::Client::new();
    // One fetch per endpoint URL, shared across steps of the file.
    let mut schemas: std::collections::HashMap<String, Option<GraphqlSchema>> =
        std::collections::HashMap::new();

    for (i, step) in steps.iter().enumerate() {
        let action = step
            .get("use")
            .or_else(|| step.get("uses"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !matches!(action, "std/graphql@v1" | "graphql") {
            continue;
        }
        let Some(with) = step.get("with").and_then(Value::as_object) else {
            continue;
        };

        // The document under lint: inline `query`, or the `query_file` read
        // from disk (relative to the lint process CWD, as at run time).
        let query = match literal(with, "query") {
            Some(q) => Some(q.to_string()),
            None => match literal(with, "query_file") {
                Some(path) => match std::fs::read_to_string(path) {
                    Ok(q) => Some(q),
                    Err(e) => {
                        notes.push(format!("/steps/{i}: cannot read query_file '{path}': {e}"));
                        None
                    }
                },
                None => None,
            },
        };

        // SDL source: fully offline validation.
        if let Some(path) = literal(with, "schema_file") {
            match std::fs::read_to_string(path)
                .map_err(|e| e.to_string())
                .and_then(|sdl| schema_from_sdl(&sdl))
            {
                Ok(schema) => {
                    if let Some(q) = &query {
                        if let Err(msg) = validate_against_schema(&schema, q) {
                            issues.push(LintIssue {
                                location: format!("/steps/{i}/with/query"),
                                problem: format!("query fails schema validation: {msg}"),
                                suggestion: None,
                            });
                        }
                    }
                }
                Err(msg) => issues.push(LintIssue {
                    location: format!("/steps/{i}/with/schema_file"),
                    problem: format!("schema_file '{path}': {msg}"),
                    suggestion: Some("expected a valid GraphQL SDL file".into()),
                }),
            }
            continue;
        }

        if with.get("introspection").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if offline {
            continue;
        }
        let Some(url) = literal(with, "url") else {
            continue;
        };

        let schema = match schemas.get(url) {
            Some(s) => s.clone(),
            None => {
                let headers: Vec<(String, String)> = with
                    .get("headers")
                    .and_then(Value::as_object)
                    .map(|h| {
                        h.iter()
                            .filter_map(|(k, v)| {
                                v.as_str()
                                    .filter(|s| !s.contains("${{"))
                                    .map(|s| (k.clone(), s.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let fetched = introspect_schema(&client, url, &headers, 5_000).await.ok();
                if fetched.is_none() {
                    notes.push(format!(
                        "/steps/{i}: could not introspect {url} — schema validation skipped"
                    ));
                }
                schemas.insert(url.to_string(), fetched.clone());
                fetched
            }
        };

        if let (Some(schema), Some(q)) = (&schema, &query) {
            if let Err(msg) = validate_against_schema(schema, q) {
                issues.push(LintIssue {
                    location: format!("/steps/{i}/with/query"),
                    problem: format!("query fails schema validation: {msg}"),
                    suggestion: None,
                });
            }
        }
    }
    (issues, notes)
}

// ---------------------------------------------------------------------------
// Schema validation → issues with suggestions
// ---------------------------------------------------------------------------

fn schema_issues(value: &Value, kind: DocKind, issues: &mut Vec<LintIssue>) {
    let schema = match kind {
        DocKind::Test => crate::schema::test_schema(),
        DocKind::Config => crate::schema::config_schema(),
    };
    let compiled =
        jsonschema::JSONSchema::compile(&schema).expect("generated schemas always compile");

    let collected: Vec<(String, String)> = match compiled.validate(value) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|e| (e.instance_path.to_string(), e.to_string()))
            .collect(),
    };

    for (path, problem) in collected {
        let location = if path.is_empty() {
            "(root)".to_string()
        } else {
            path
        };
        let suggestion = schema_error_suggestion(&problem);
        issues.push(LintIssue {
            location,
            problem,
            suggestion,
        });
    }
}

fn schema_error_suggestion(problem: &str) -> Option<String> {
    // A step missing both `use` and `uses` fails the `anyOf` on the Step
    // definition (see `schema::relax_use_alias`); the older wording was
    // `"use" is a required property`.
    if problem.contains("\"use\" is a required property") || problem.contains("anyOf") {
        Some("every step must name an action: `use: std/http@v1` (or the `uses:` alias) — `std/http@v1`, `std/graphql@v1`, `std/tcp@v1`, `std/udp@v1`, `std/pubsub@v1`, `std/llm@v1`, `std/ws@v1`, `std/ws-connect@v1`, `std/ws-send@v1`, `std/ws-recv@v1`, `std/ws-ping@v1`, `std/ws-close@v1`, `std/grpc@v1`, `std/grpc-connect@v1`, `std/grpc-call@v1`, `std/grpc-stream-open@v1`, `std/grpc-stream-send@v1`, `std/grpc-stream-recv@v1`, `std/grpc-stream-close@v1`, `std/db-connect@v1`, `std/db-query@v1`, `std/db-tx-begin@v1`, `std/db-tx-commit@v1`, `std/db-tx-rollback@v1`, `std/db-close@v1`, `std/check@v1`, `std/sleep@v1`, `std/log@v1`, `std/file-read@v1`, `std/file-write@v1`, `std/child_process@v1`, `std/kill_process@v1`, `std/thresholds@v1`, `std/set_shared_variable@v1`, or `std/get_shared_variable@v1`".into())
    } else if problem.contains("\"steps\" is a required property") {
        Some("a test definition is a mapping with a `steps:` list at the top level".into())
    } else if problem.contains("\"url\" is a required property") {
        Some("`report:` needs a `url:` pointing at a running `perfscale serve`, e.g. `url: http://localhost:7999`".into())
    } else if problem.contains("is not of type \"integer\"") {
        Some("use a plain number, e.g. `vus: 10`".into())
    } else if problem.contains("is not of type \"string\"") {
        Some("quote the value if it contains special characters, e.g. `duration: \"30s\"`".into())
    } else if problem.contains("is not of type \"array\"") {
        Some("`steps:` is a list — each entry starts with `- `".into())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Unknown / typo'd fields (beyond what the schema rejects)
// ---------------------------------------------------------------------------

const TEST_TOP_FIELDS: [&str; 1] = ["steps"];
const STEP_FIELDS: [&str; 8] = [
    "name", "use", "uses", "with", "check", "outputs", "severity", "message",
];
const CONFIG_TOP_FIELDS: [&str; 12] = [
    "vus",
    "duration",
    "stages",
    "arrival",
    "gpu",
    "report",
    "before",
    "after",
    "variables",
    "shared_variables",
    "allow_file_actions",
    "allow_process_actions",
];
const REPORT_FIELDS: [&str; 1] = ["url"];
const CHECK_FIELDS: [&str; 7] = [
    "on",
    "status",
    "duration_ms_lt",
    "body_contains",
    "message_contains",
    "message_matches",
    "messages_count_gte",
];
const HTTP_WITH_FIELDS: [&str; 8] = [
    "method",
    "url",
    "headers",
    "body",
    "timeout",
    "insecure",
    "multipart",
    "pool",
];
const GRAPHQL_WITH_FIELDS: [&str; 12] = [
    "url",
    "query",
    "query_file",
    "variables",
    "operation",
    "method",
    "headers",
    "timeout",
    "insecure",
    "introspection",
    "schema_file",
    "pool",
];
// TCP and UDP share the same `with` fields; `read`/`expect` gate the reply.
const RAW_NET_WITH_FIELDS: [&str; 9] = [
    "host",
    "port",
    "address",
    "send",
    "send_base64",
    "read",
    "read_bytes",
    "expect",
    "timeout",
];
// Shared by std/ws@v1 and std/ws-connect@v1 (the Connection Profile), plus
// per-action extras below.
const WS_PROFILE_FIELDS: [&str; 6] = [
    "connection",
    "url",
    "headers",
    "subprotocols",
    "skipTLSVerify",
    "timeout",
];
const WS_SESSION_WITH_FIELDS: [&str; 7] = [
    "connection",
    "url",
    "headers",
    "subprotocols",
    "skipTLSVerify",
    "timeout",
    "messages",
];
const WS_SEND_WITH_FIELDS: [&str; 6] = [
    "id",
    "send",
    "send_base64",
    "repeat",
    "interval_ms",
    "timeout",
];
const WS_RECV_WITH_FIELDS: [&str; 5] = ["id", "count", "until_contains", "until_json", "timeout"];
const WS_PING_WITH_FIELDS: [&str; 2] = ["id", "timeout"];
const WS_CLOSE_WITH_FIELDS: [&str; 4] = ["id", "code", "reason", "timeout"];
const PUBSUB_WITH_FIELDS: [&str; 5] = ["driver", "subject", "url", "publish", "subscribe"];
const SET_SHARED_VARIABLE_WITH_FIELDS: [&str; 4] = ["driver", "name", "op", "value"];
const GET_SHARED_VARIABLE_WITH_FIELDS: [&str; 5] = ["driver", "name", "op", "wait_for", "extract"];
const LLM_WITH_FIELDS: [&str; 12] = [
    "endpoint",
    "url",
    "model",
    "messages",
    "prompt",
    "max_tokens",
    "stream",
    "api_key",
    "headers",
    "params",
    "extract",
    "timeout_ms",
];
// Shared by std/grpc@v1 and std/grpc-connect@v1 (the Channel Profile), plus
// per-action extras below.
const GRPC_PROFILE_FIELDS: [&str; 8] = [
    "connection",
    "url",
    "metadata",
    "skipTLSVerify",
    "descriptor_set",
    "reflection",
    "max_recv_size",
    "timeout",
];
const GRPC_UNARY_WITH_FIELDS: [&str; 12] = [
    "connection",
    "url",
    "metadata",
    "skipTLSVerify",
    "descriptor_set",
    "reflection",
    "max_recv_size",
    "timeout",
    "method",
    "payload",
    "payload_base64",
    "expect_status",
];
const GRPC_CALL_WITH_FIELDS: [&str; 7] = [
    "id",
    "method",
    "payload",
    "payload_base64",
    "metadata",
    "expect_status",
    "timeout",
];
const GRPC_STREAM_OPEN_WITH_FIELDS: [&str; 5] =
    ["id", "method", "payload", "payload_base64", "metadata"];
const GRPC_STREAM_SEND_WITH_FIELDS: [&str; 6] = [
    "id",
    "payload",
    "payload_base64",
    "repeat",
    "interval_ms",
    "timeout",
];
const GRPC_STREAM_RECV_WITH_FIELDS: [&str; 5] =
    ["id", "count", "until_contains", "until_json", "timeout"];
const GRPC_STREAM_CLOSE_WITH_FIELDS: [&str; 3] = ["id", "expect_status", "timeout"];
const DB_CONNECT_WITH_FIELDS: [&str; 6] =
    ["driver", "dsn", "tls", "mode", "pool_size", "timeout_ms"];
const DB_QUERY_WITH_FIELDS: [&str; 5] = ["id", "query", "params", "max_rows", "timeout_ms"];
const DB_TX_WITH_FIELDS: [&str; 2] = ["id", "timeout_ms"];
const DB_CLOSE_WITH_FIELDS: [&str; 1] = ["id"];
const SLEEP_WITH_FIELDS: [&str; 2] = ["ms", "seconds"];
const LOG_WITH_FIELDS: [&str; 1] = ["message"];
const FILE_READ_WITH_FIELDS: [&str; 2] = ["path", "encoding"];
const FILE_WRITE_WITH_FIELDS: [&str; 4] = ["path", "content", "encoding", "append"];
const CHILD_PROCESS_WITH_FIELDS: [&str; 10] = [
    "command",
    "args",
    "env",
    "cwd",
    "port",
    "restart",
    "max_restarts",
    "backoff_ms",
    "waitUntil",
    "buffer_kb",
];
const KILL_PROCESS_WITH_FIELDS: [&str; 5] = ["name", "pid", "signal", "grace_ms", "tree"];
// Nested `waitUntil` object of std/child_process@v1 (the string form is not
// linted — it carries no keys).
const WAIT_UNTIL_FIELDS: [&str; 7] = [
    "stdout_contains",
    "stderr_contains",
    "stdout_matches",
    "stderr_matches",
    "port_open",
    "timeout",
    "on_timeout",
];

fn lint_test_fields(value: &Value, issues: &mut Vec<LintIssue>) {
    if let Some(map) = value.as_object() {
        unknown_field_issues(map, &TEST_TOP_FIELDS, "(root)", issues);
    }

    let Some(steps) = value.get("steps").and_then(|s| s.as_array()) else {
        return;
    };

    for (i, step) in steps.iter().enumerate() {
        lint_step(step, &format!("/steps/{i}"), issues);
    }
}

/// Lint one step map (shared by test `steps` and config `before` steps): known
/// top-level fields, a resolvable action, and per-action `with`/`check` fields.
fn lint_step(step: &Value, loc: &str, issues: &mut Vec<LintIssue>) {
    let Some(map) = step.as_object() else {
        return;
    };

    unknown_field_issues(map, &STEP_FIELDS, loc, issues);

    // `use` is canonical; `uses` is an accepted alias.
    let use_key = if map.contains_key("uses") {
        "uses"
    } else {
        "use"
    };
    let action = map
        .get("use")
        .or_else(|| map.get("uses"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // `pro/*` actions are proprietary and registered at runtime, so the linter
    // can't know them statically — never flag them as unknown.
    if !action.is_empty() && !action.starts_with("pro/") && !is_known_action(action) {
        issues.push(LintIssue {
            location: format!("{loc}/{use_key}"),
            problem: format!("unknown action '{action}'"),
            suggestion: did_you_mean(
                action,
                &[
                    "std/http@v1",
                    "std/graphql@v1",
                    "std/tcp@v1",
                    "std/udp@v1",
                    "std/pubsub@v1",
                    "std/llm@v1",
                    "std/ws@v1",
                    "std/ws-connect@v1",
                    "std/ws-send@v1",
                    "std/ws-recv@v1",
                    "std/ws-ping@v1",
                    "std/ws-close@v1",
                    "std/grpc@v1",
                    "std/grpc-connect@v1",
                    "std/grpc-call@v1",
                    "std/grpc-stream-open@v1",
                    "std/grpc-stream-send@v1",
                    "std/grpc-stream-recv@v1",
                    "std/grpc-stream-close@v1",
                    "std/db-connect@v1",
                    "std/db-query@v1",
                    "std/db-tx-begin@v1",
                    "std/db-tx-commit@v1",
                    "std/db-tx-rollback@v1",
                    "std/db-close@v1",
                    "std/check@v1",
                    "std/sleep@v1",
                    "std/log@v1",
                    "std/file-read@v1",
                    "std/file-write@v1",
                    "std/child_process@v1",
                    "std/kill_process@v1",
                    "std/thresholds@v1",
                    "std/set_shared_variable@v1",
                    "std/get_shared_variable@v1",
                ],
            )
            .or_else(|| {
                Some(
                    "available actions: std/http@v1, std/graphql@v1, std/tcp@v1, std/udp@v1, std/pubsub@v1, std/llm@v1, std/ws@v1, std/ws-connect@v1, std/ws-send@v1, std/ws-recv@v1, std/ws-ping@v1, std/ws-close@v1, std/grpc@v1, std/grpc-connect@v1, std/grpc-call@v1, std/grpc-stream-open@v1, std/grpc-stream-send@v1, std/grpc-stream-recv@v1, std/grpc-stream-close@v1, std/db-connect@v1, std/db-query@v1, std/db-tx-begin@v1, std/db-tx-commit@v1, std/db-tx-rollback@v1, std/db-close@v1, std/check@v1, std/sleep@v1, std/log@v1, std/file-read@v1, std/file-write@v1, std/child_process@v1, std/kill_process@v1, std/thresholds@v1, std/set_shared_variable@v1, std/get_shared_variable@v1"
                        .into(),
                )
            }),
        });
    }

    if let Some(with) = map.get("with").and_then(|v| v.as_object()) {
        let with_fields: Option<&[&str]> = match action {
            "std/http@v1" | "http" => Some(&HTTP_WITH_FIELDS),
            "std/graphql@v1" | "graphql" => Some(&GRAPHQL_WITH_FIELDS),
            "std/tcp@v1" | "tcp" | "std/udp@v1" | "udp" => Some(&RAW_NET_WITH_FIELDS),
            "std/pubsub@v1" | "pubsub" => Some(&PUBSUB_WITH_FIELDS),
            "std/llm@v1" | "llm" => Some(&LLM_WITH_FIELDS),
            "std/ws@v1" | "ws" => Some(&WS_SESSION_WITH_FIELDS),
            "std/ws-connect@v1" | "ws-connect" => Some(&WS_PROFILE_FIELDS),
            "std/ws-send@v1" | "ws-send" => Some(&WS_SEND_WITH_FIELDS),
            "std/ws-recv@v1" | "ws-recv" => Some(&WS_RECV_WITH_FIELDS),
            "std/ws-ping@v1" | "ws-ping" => Some(&WS_PING_WITH_FIELDS),
            "std/ws-close@v1" | "ws-close" => Some(&WS_CLOSE_WITH_FIELDS),
            "std/grpc@v1" | "grpc" => Some(&GRPC_UNARY_WITH_FIELDS),
            "std/grpc-connect@v1" | "grpc-connect" => Some(&GRPC_PROFILE_FIELDS),
            "std/grpc-call@v1" | "grpc-call" => Some(&GRPC_CALL_WITH_FIELDS),
            "std/grpc-stream-open@v1" | "grpc-stream-open" => Some(&GRPC_STREAM_OPEN_WITH_FIELDS),
            "std/grpc-stream-send@v1" | "grpc-stream-send" => Some(&GRPC_STREAM_SEND_WITH_FIELDS),
            "std/grpc-stream-recv@v1" | "grpc-stream-recv" => Some(&GRPC_STREAM_RECV_WITH_FIELDS),
            "std/grpc-stream-close@v1" | "grpc-stream-close" => {
                Some(&GRPC_STREAM_CLOSE_WITH_FIELDS)
            }
            "std/db-connect@v1" | "db-connect" => Some(&DB_CONNECT_WITH_FIELDS),
            "std/db-query@v1" | "db-query" => Some(&DB_QUERY_WITH_FIELDS),
            "std/db-tx-begin@v1"
            | "db-tx-begin"
            | "std/db-tx-commit@v1"
            | "db-tx-commit"
            | "std/db-tx-rollback@v1"
            | "db-tx-rollback" => Some(&DB_TX_WITH_FIELDS),
            "std/db-close@v1" | "db-close" => Some(&DB_CLOSE_WITH_FIELDS),
            "std/check@v1" | "check" => Some(&CHECK_FIELDS),
            "std/sleep@v1" | "sleep" => Some(&SLEEP_WITH_FIELDS),
            "std/log@v1" | "log" => Some(&LOG_WITH_FIELDS),
            "std/file-read@v1" | "file-read" => Some(&FILE_READ_WITH_FIELDS),
            "std/file-write@v1" | "file-write" => Some(&FILE_WRITE_WITH_FIELDS),
            "std/child_process@v1" | "child_process" => Some(&CHILD_PROCESS_WITH_FIELDS),
            "std/kill_process@v1" | "kill_process" => Some(&KILL_PROCESS_WITH_FIELDS),
            "std/set_shared_variable@v1" | "set_shared_variable" => {
                Some(&SET_SHARED_VARIABLE_WITH_FIELDS)
            }
            "std/get_shared_variable@v1" | "get_shared_variable" => {
                Some(&GET_SHARED_VARIABLE_WITH_FIELDS)
            }
            // pro/* and unknown actions: `with` is free-form, don't lint fields.
            _ => None,
        };
        if let Some(fields) = with_fields {
            unknown_field_issues(with, fields, &format!("{loc}/with"), issues);
        }

        // std/child_process@v1's `waitUntil` object gets its own nested lint
        // (the string form carries no keys, so there is nothing to check).
        if matches!(action, "std/child_process@v1" | "child_process") {
            if let Some(wait_until) = with.get("waitUntil").and_then(|v| v.as_object()) {
                unknown_field_issues(
                    wait_until,
                    &WAIT_UNTIL_FIELDS,
                    &format!("{loc}/with/waitUntil"),
                    issues,
                );
            }
        }
    }

    // The DB family additionally lints MISSING required fields and the
    // `driver` value: a db-query without `id`/`query` only fails mid-run, and
    // a typo'd driver fails the whole scenario at its first step — both are
    // cheap to catch here.
    lint_db_with(
        action,
        map.get("with").and_then(|v| v.as_object()),
        loc,
        issues,
    );

    // std/graphql@v1 gets the same treatment: a missing url/query or a
    // malformed GraphQL document otherwise fails only mid-run.
    lint_graphql_with(
        action,
        map.get("with").and_then(|v| v.as_object()),
        loc,
        issues,
    );

    if let Some(check) = map.get("check").and_then(|v| v.as_object()) {
        unknown_field_issues(check, &CHECK_FIELDS, &format!("{loc}/check"), issues);
    }
}

fn lint_config_fields(value: &Value, issues: &mut Vec<LintIssue>) {
    let Some(map) = value.as_object() else { return };
    unknown_field_issues(map, &CONFIG_TOP_FIELDS, "(root)", issues);

    if let Some(report) = map.get("report").and_then(|v| v.as_object()) {
        unknown_field_issues(report, &REPORT_FIELDS, "/report", issues);
    }

    // Load-profile validation: the schema accepts any stage list shape, but a
    // broken profile (both modes at once, an unparseable/zero stage duration,
    // `arrival` without `max_vus`) must fail at lint time, not mid-run.
    // `stages: []` is checked on the raw document — after serde's defaults it
    // is indistinguishable from an absent key.
    if map
        .get("stages")
        .is_some_and(|v| v.as_array().is_some_and(|a| a.is_empty()))
    {
        issues.push(LintIssue {
            location: "/stages".into(),
            problem: "'stages' must contain at least one stage".into(),
            suggestion: Some("e.g. `stages: [{ duration: 30s, target: 10 }]`".into()),
        });
    }
    if let Ok(cfg) = serde_json::from_value::<crate::yaml::ConfigFile>(value.clone()) {
        if let Err(msg) = cfg.run.resolve_schedule() {
            issues.push(LintIssue {
                location: if map.contains_key("stages") {
                    "/stages".into()
                } else {
                    "/arrival".into()
                },
                problem: msg,
                suggestion: None,
            });
        }
    }

    // `before:`/`after:` steps get the same per-step linting as test steps.
    if let Some(before) = map.get("before").and_then(|b| b.as_array()) {
        for (i, step) in before.iter().enumerate() {
            lint_step(step, &format!("/before/{i}"), issues);
        }
    }
    if let Some(after) = map.get("after").and_then(|a| a.as_array()) {
        for (i, step) in after.iter().enumerate() {
            lint_step(step, &format!("/after/{i}"), issues);
        }
    }

    // `before:`/`after:` shared-variable steps can be checked against the
    // declared `shared_variables` right here — they live in the same file.
    // (Test steps live in the test file; the run itself re-validates
    // everything against the declarations before any VU starts.)
    if let Some(decls) = map.get("shared_variables").and_then(|v| v.as_object()) {
        for (key, section) in [("before", "/before"), ("after", "/after")] {
            if let Some(list) = map.get(key).and_then(|v| v.as_array()) {
                for (i, step) in list.iter().enumerate() {
                    let action = step
                        .get("use")
                        .or_else(|| step.get("uses"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let label = step
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("{key}/{i}"));
                    if let Err(msg) = crate::step::shared_variable::check_shared_variable_step(
                        action,
                        step.get("with"),
                        &label,
                        decls,
                    ) {
                        issues.push(LintIssue {
                            location: format!("{section}/{i}/with"),
                            problem: msg,
                            suggestion: None,
                        });
                    }
                }
            }
        }
    }
}

/// Advisory findings that must NOT fail `perfscale lint` (unlike
/// [`lint`] issues): printed as warnings by the CLI, exit code unaffected.
///
/// Currently: a config that sets both a load profile (`stages:`/`arrival:`)
/// and explicit `vus:`/`duration:` — the profile wins and the fixed fields
/// are silently ignored at run time, which usually isn't what the author
/// meant.
pub fn lint_warnings(yaml: &str, kind: DocKind) -> Vec<String> {
    if kind != DocKind::Config {
        return Vec::new();
    }
    let Ok(value) = serde_yaml::from_str::<Value>(yaml) else {
        return Vec::new();
    };
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    let has_profile = map.contains_key("stages") || map.contains_key("arrival");
    let has_fixed = map.contains_key("vus") || map.contains_key("duration");
    if has_profile && has_fixed {
        return vec![
            "`stages`/`arrival` override `vus`/`duration` — the fixed fields are ignored; remove them to avoid confusion"
                .into(),
        ];
    }
    Vec::new()
}

fn is_known_action(action: &str) -> bool {
    matches!(
        action,
        "std/http@v1"
            | "http"
            | "std/graphql@v1"
            | "graphql"
            | "std/tcp@v1"
            | "tcp"
            | "std/udp@v1"
            | "udp"
            | "std/pubsub@v1"
            | "pubsub"
            | "std/llm@v1"
            | "llm"
            | "std/ws@v1"
            | "ws"
            | "std/ws-connect@v1"
            | "ws-connect"
            | "std/ws-send@v1"
            | "ws-send"
            | "std/ws-recv@v1"
            | "ws-recv"
            | "std/ws-ping@v1"
            | "ws-ping"
            | "std/ws-close@v1"
            | "ws-close"
            | "std/grpc@v1"
            | "grpc"
            | "std/grpc-connect@v1"
            | "grpc-connect"
            | "std/grpc-call@v1"
            | "grpc-call"
            | "std/grpc-stream-open@v1"
            | "grpc-stream-open"
            | "std/grpc-stream-send@v1"
            | "grpc-stream-send"
            | "std/grpc-stream-recv@v1"
            | "grpc-stream-recv"
            | "std/grpc-stream-close@v1"
            | "grpc-stream-close"
            | "std/db-connect@v1"
            | "db-connect"
            | "std/db-query@v1"
            | "db-query"
            | "std/db-tx-begin@v1"
            | "db-tx-begin"
            | "std/db-tx-commit@v1"
            | "db-tx-commit"
            | "std/db-tx-rollback@v1"
            | "db-tx-rollback"
            | "std/db-close@v1"
            | "db-close"
            | "std/check@v1"
            | "check"
            | "std/sleep@v1"
            | "sleep"
            | "std/log@v1"
            | "std/file-read@v1"
            | "file-read"
            | "std/file-write@v1"
            | "file-write"
            | "std/child_process@v1"
            | "child_process"
            | "std/kill_process@v1"
            | "kill_process"
            | "std/thresholds@v1"
            | "thresholds"
            | "std/set_shared_variable@v1"
            | "set_shared_variable"
            | "std/get_shared_variable@v1"
            | "get_shared_variable"
            | "log"
    )
}

fn unknown_field_issues(
    map: &serde_json::Map<String, Value>,
    known: &[&str],
    location: &str,
    issues: &mut Vec<LintIssue>,
) {
    for key in map.keys() {
        if !known.contains(&key.as_str()) {
            issues.push(LintIssue {
                location: location.to_string(),
                problem: format!("unknown field '{key}'"),
                suggestion: did_you_mean(key, known)
                    .or_else(|| Some(format!("valid fields here: {}", known.join(", ")))),
            });
        }
    }
}

/// Required-`with`-field checks for the DB action family, plus validation of
/// `db-connect`'s `driver` value. Other actions leave missing fields to
/// runtime validation; the DB family is checked at lint time because a
/// missing `id`/`query` or a typo'd driver only surfaces mid-run otherwise.
/// `with: None` means the step has no `with:` block at all — every required
/// field is missing.
fn lint_db_with(
    action: &str,
    with: Option<&serde_json::Map<String, Value>>,
    loc: &str,
    issues: &mut Vec<LintIssue>,
) {
    let required: &[&str] = match action {
        "std/db-connect@v1" | "db-connect" => &["driver", "dsn"],
        "std/db-query@v1" | "db-query" => &["id", "query"],
        "std/db-tx-begin@v1"
        | "db-tx-begin"
        | "std/db-tx-commit@v1"
        | "db-tx-commit"
        | "std/db-tx-rollback@v1"
        | "db-tx-rollback"
        | "std/db-close@v1"
        | "db-close" => &["id"],
        _ => return,
    };
    for field in required {
        if with.is_some_and(|w| w.contains_key(*field)) {
            continue;
        }
        let hint = match *field {
            "driver" => "postgres, mysql, or sqlite",
            "dsn" => "a driver-native connection string",
            "id" => "the output of `std/db-connect@v1`, e.g. `${{ conn.id }}`",
            "query" => "SQL text with driver-native placeholders",
            _ => unreachable!("the required-fields table only lists the above"),
        };
        issues.push(LintIssue {
            location: format!("{loc}/with"),
            problem: format!("missing required field '{field}'"),
            suggestion: Some(format!("`{action}` needs `{field}` — {hint}")),
        });
    }

    // The driver value is checkable only when it is a literal — interpolated
    // `${{ … }}` values resolve at runtime.
    if matches!(action, "std/db-connect@v1" | "db-connect") {
        if let Some(driver) = with.and_then(|w| w.get("driver")).and_then(Value::as_str) {
            if !driver.contains("${{") && !matches!(driver, "postgres" | "mysql" | "sqlite") {
                issues.push(LintIssue {
                    location: format!("{loc}/with/driver"),
                    problem: format!("unknown driver '{driver}'"),
                    suggestion: Some("expected postgres, mysql, or sqlite".into()),
                });
            }
        }
    }
}

/// Required-`with`-field checks for `std/graphql@v1`, plus offline validation
/// of literal values: the query/document syntax (via `graphql-parser`), the
/// `method` and `pool` enums. Interpolated `${{ … }}` values resolve at
/// runtime and skip every check here.
fn lint_graphql_with(
    action: &str,
    with: Option<&serde_json::Map<String, Value>>,
    loc: &str,
    issues: &mut Vec<LintIssue>,
) {
    if !matches!(action, "std/graphql@v1" | "graphql") {
        return;
    }

    if !with.is_some_and(|w| w.contains_key("url")) {
        issues.push(LintIssue {
            location: format!("{loc}/with"),
            problem: "missing required field 'url'".into(),
            suggestion: Some("`std/graphql@v1` needs `url` — the GraphQL endpoint, e.g. `http://localhost:4000/graphql`".into()),
        });
    }

    let has_query = with.is_some_and(|w| w.contains_key("query"));
    let has_query_file = with.is_some_and(|w| w.contains_key("query_file"));
    match (has_query, has_query_file) {
        (false, false) => issues.push(LintIssue {
            location: format!("{loc}/with"),
            problem: "missing required field 'query'".into(),
            suggestion: Some(
                "`std/graphql@v1` needs `query` (inline document) or `query_file` (a .graphql file)"
                    .into(),
            ),
        }),
        (true, true) => issues.push(LintIssue {
            location: format!("{loc}/with"),
            problem: "'query' and 'query_file' are mutually exclusive".into(),
            suggestion: Some("keep one: inline `query` for short documents, `query_file` for large ones".into()),
        }),
        _ => {}
    }

    // Literal-only checks below: interpolated values resolve at runtime.
    fn literal<'a>(w: Option<&'a serde_json::Map<String, Value>>, k: &str) -> Option<&'a str> {
        w.and_then(|w| w.get(k))
            .and_then(Value::as_str)
            .filter(|s| !s.contains("${{"))
    }

    if let Some(query) = literal(with, "query") {
        if let Err(msg) = crate::step::graphql::validate_query_syntax(query) {
            issues.push(LintIssue {
                location: format!("{loc}/with/query"),
                problem: msg,
                suggestion: Some(
                    "check the document: balanced braces, valid field names, no stray commas"
                        .into(),
                ),
            });
        }
    }

    if let Some(method) = literal(with, "method") {
        if !matches!(method.to_ascii_uppercase().as_str(), "POST" | "GET") {
            issues.push(LintIssue {
                location: format!("{loc}/with/method"),
                problem: format!("invalid method '{method}'"),
                suggestion: Some("GraphQL over HTTP is POST (default) or GET".into()),
            });
        }
    }

    if let Some(pool) = literal(with, "pool") {
        if !matches!(pool, "per-vu" | "shared") {
            issues.push(LintIssue {
                location: format!("{loc}/with/pool"),
                problem: format!("invalid pool '{pool}'"),
                suggestion: Some("expected per-vu (default) or shared".into()),
            });
        }
    }
}

/// The candidate closest to `input` within edit distance 2, if any — the
/// shared core of every did-you-mean suggestion (lint fields, gRPC method
/// resolution, GraphQL schema-field validation).
pub(crate) fn closest_name<'a>(
    input: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    candidates
        .into_iter()
        .map(|c| (c, edit_distance(input, c)))
        .filter(|(_, d)| *d <= 2)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}

/// `Some("did you mean 'check'?")` when a known name is within edit
/// distance 2 of the typo.
fn did_you_mean(input: &str, candidates: &[&str]) -> Option<String> {
    closest_name(input, candidates.iter().copied()).map(|c| format!("did you mean '{c}'?"))
}

/// Levenshtein distance, used by the linter and by `grpc` method resolution
/// for did-you-mean suggestions.
pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];

    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_test_file_has_no_issues() {
        let yaml = r#"
steps:
  - name: ping
    use: std/http@v1
    with:
      method: GET
      url: https://example.com
    check:
      status: 200
    outputs: resp
"#;
        assert_eq!(lint(yaml, DocKind::Test), vec![]);
    }

    #[test]
    fn valid_config_has_no_issues() {
        let yaml = "vus: 10\nduration: 30s\nreport:\n  url: http://localhost:7999\n";
        assert_eq!(lint(yaml, DocKind::Config), vec![]);
    }

    #[test]
    fn malformed_yaml_is_one_issue_with_suggestion() {
        let issues = lint("steps: [oops: {", DocKind::Test);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].problem.contains("invalid YAML"));
        assert!(issues[0].suggestion.is_some());
    }

    #[test]
    fn missing_use_reports_location_and_fix() {
        let yaml = "steps:\n  - name: ping\n    with:\n      url: https://x\n";
        let issues = lint(yaml, DocKind::Test);
        // The step names no action (neither `use` nor `uses`); the schema
        // reports this at /steps/0 and the linter attaches the action hint.
        let missing = issues
            .iter()
            .find(|i| {
                i.location == "/steps/0"
                    && i.suggestion
                        .as_deref()
                        .is_some_and(|s| s.contains("std/http@v1"))
            })
            .unwrap();
        assert!(missing
            .suggestion
            .as_deref()
            .unwrap()
            .contains("std/http@v1"));
    }

    #[test]
    fn typo_in_step_field_gets_did_you_mean() {
        let yaml = "steps:\n  - use: std/http@v1\n    with:\n      url: https://x\n    chek:\n      status: 200\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'chek'"))
            .unwrap();
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'check'?"));
    }

    #[test]
    fn typo_in_check_key_gets_did_you_mean() {
        let yaml = "steps:\n  - use: std/http@v1\n    with:\n      url: https://x\n    check:\n      body_containz: ok\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'body_containz'"))
            .unwrap();
        assert_eq!(typo.location, "/steps/0/check");
        assert_eq!(
            typo.suggestion.as_deref(),
            Some("did you mean 'body_contains'?")
        );
    }

    #[test]
    fn typo_in_http_with_key_gets_did_you_mean() {
        let yaml =
            "steps:\n  - use: std/http@v1\n    with:\n      url: https://x\n      methd: GET\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'methd'"))
            .unwrap();
        assert_eq!(typo.location, "/steps/0/with");
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'method'?"));
    }

    #[test]
    fn unknown_action_lists_alternatives() {
        let yaml = "steps:\n  - use: std/htp@v1\n    with:\n      url: https://x\n";
        let issues = lint(yaml, DocKind::Test);
        let bad = issues
            .iter()
            .find(|i| i.problem.contains("unknown action"))
            .unwrap();
        assert_eq!(bad.location, "/steps/0/use");
        assert_eq!(
            bad.suggestion.as_deref(),
            Some("did you mean 'std/http@v1'?")
        );
    }

    #[test]
    fn config_typo_gets_did_you_mean() {
        let yaml = "vsu: 10\nduration: 30s\n";
        let issues = lint(yaml, DocKind::Config);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'vsu'"))
            .unwrap();
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'vus'?"));
    }

    #[test]
    fn config_wrong_type_gets_type_suggestion() {
        let issues = lint("vus: ten\n", DocKind::Config);
        let wrong = issues
            .iter()
            .find(|i| i.problem.contains("is not of type \"integer\""))
            .unwrap();
        assert!(wrong.suggestion.as_deref().unwrap().contains("vus: 10"));
    }

    #[test]
    fn detect_kind_by_steps_key() {
        assert_eq!(detect_kind("steps: []\n"), DocKind::Test);
        assert_eq!(detect_kind("vus: 5\n"), DocKind::Config);
        assert_eq!(detect_kind("not: [valid"), DocKind::Config);
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("check", "check"), 0);
        assert_eq!(edit_distance("chek", "check"), 1);
        assert_eq!(edit_distance("vsu", "vus"), 2);
        assert!(edit_distance("completely", "different") > 2);
    }

    #[test]
    fn unrelated_unknown_field_lists_valid_fields() {
        let yaml = "steps:\n  - use: std/log@v1\n    frobnicate: yes\n";
        let issues = lint(yaml, DocKind::Test);
        let unknown = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'frobnicate'"))
            .unwrap();
        assert!(unknown
            .suggestion
            .as_deref()
            .unwrap()
            .contains("valid fields here"));
    }

    #[test]
    fn grpc_family_lints_clean() {
        let yaml = r#"
steps:
  - name: connect
    use: std/grpc-connect@v1
    with:
      url: grpcs://api.example.com
      reflection: true
      metadata: { authorization: Bearer x }
    outputs: conn
  - name: call
    use: grpc-call
    with:
      id: "${{ conn.id }}"
      method: "pkg.Svc/Method"
      payload: { message: hi }
      expect_status: 0
      timeout: 5000
  - name: open
    use: std/grpc-stream-open@v1
    with: { id: "${{ conn.id }}", method: "pkg.Svc/Stream" }
    outputs: stream
  - name: send
    use: std/grpc-stream-send@v1
    with: { id: "${{ stream.id }}", payload: { message: hi }, repeat: 3, interval_ms: 10 }
  - name: recv
    use: std/grpc-stream-recv@v1
    with: { id: "${{ stream.id }}", until_contains: hi, timeout: 1000 }
  - name: close
    use: std/grpc-stream-close@v1
    with: { id: "${{ stream.id }}", expect_status: 0 }
  - name: oneshot
    use: std/grpc@v1
    with:
      url: grpc://127.0.0.1:50051
      descriptor_set: "AAAA"
      method: "pkg.Svc/Method"
      payload_base64: "CAE="
"#;
        assert_eq!(lint(yaml, DocKind::Test), vec![]);
    }

    #[test]
    fn typo_in_grpc_action_gets_did_you_mean() {
        let yaml = "steps:\n  - use: std/grpc-clal@v1\n    with:\n      id: grpc-1\n";
        let issues = lint(yaml, DocKind::Test);
        let bad = issues
            .iter()
            .find(|i| i.problem.contains("unknown action"))
            .unwrap();
        assert_eq!(
            bad.suggestion.as_deref(),
            Some("did you mean 'std/grpc-call@v1'?")
        );
    }

    #[test]
    fn typo_in_grpc_with_key_gets_did_you_mean() {
        let yaml =
            "steps:\n  - use: std/grpc-call@v1\n    with:\n      id: grpc-1\n      paylod: {}\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'paylod'"))
            .unwrap();
        assert_eq!(typo.location, "/steps/0/with");
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'payload'?"));
    }

    // -----------------------------------------------------------------
    // std/child_process@v1 / std/kill_process@v1 + config `after`
    // -----------------------------------------------------------------

    #[test]
    fn process_actions_and_after_section_lint_clean() {
        let yaml = r#"
vus: 5
duration: 30s
allow_process_actions: true
before:
  - name: web
    uses: std/child_process@v1
    with:
      command: python3
      args: ["-m", "http.server", "8080"]
      env: { API_URL: http://localhost:8080 }
      cwd: scripts/
      port: 8080
      restart: on-failure
      max_restarts: 3
      backoff_ms: 1000
      buffer_kb: 128
      waitUntil:
        stdout_contains: "Serving HTTP"
        stderr_contains: "warn"
        stdout_matches: "listening on \\d+"
        stderr_matches: "err\\d+"
        port_open: 8080
        timeout: 15s
        on_timeout: fail
    outputs: web
after:
  - name: stop web
    uses: std/kill_process@v1
    with:
      name: web
      signal: TERM
      grace_ms: 5000
      tree: true
"#;
        assert_eq!(lint(yaml, DocKind::Config), vec![]);
    }

    #[test]
    fn process_action_aliases_lint_clean() {
        let yaml = r#"
allow_process_actions: true
before:
  - uses: child_process
    with: { command: sh, args: ["-c", "sleep 5"], waitUntil: 'contains(stdout, "ready")' }
after:
  - uses: kill_process
    with: { pid: 12345 }
"#;
        assert_eq!(lint(yaml, DocKind::Config), vec![]);
    }

    #[test]
    fn typo_in_child_process_with_key_gets_did_you_mean() {
        let yaml = "steps:\n  - use: std/child_process@v1\n    with:\n      comand: sh\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'comand'"))
            .unwrap();
        assert_eq!(typo.location, "/steps/0/with");
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'command'?"));
    }

    #[test]
    fn typo_in_kill_process_with_key_gets_did_you_mean() {
        let yaml = "steps:\n  - use: std/kill_process@v1\n    with:\n      name: web\n      siganl: TERM\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'siganl'"))
            .unwrap();
        assert_eq!(typo.location, "/steps/0/with");
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'signal'?"));
    }

    #[test]
    fn typo_in_wait_until_key_gets_did_you_mean() {
        let yaml = r#"
steps:
  - use: std/child_process@v1
    with:
      command: sh
      waitUntil:
        stdout_containz: ready
"#;
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'stdout_containz'"))
            .unwrap();
        assert_eq!(typo.location, "/steps/0/with/waitUntil");
        assert_eq!(
            typo.suggestion.as_deref(),
            Some("did you mean 'stdout_contains'?")
        );
    }

    #[test]
    fn after_steps_are_linted_like_before_steps() {
        let yaml = r#"
after:
  - use: std/htp@v1
    with: { url: https://x }
"#;
        let issues = lint(yaml, DocKind::Config);
        let bad = issues
            .iter()
            .find(|i| i.problem.contains("unknown action"))
            .unwrap();
        assert_eq!(bad.location, "/after/0/use");
        assert_eq!(
            bad.suggestion.as_deref(),
            Some("did you mean 'std/http@v1'?")
        );
    }

    #[test]
    fn unknown_config_field_near_after_gets_did_you_mean() {
        let yaml = "vus: 1\naftr: []\n";
        let issues = lint(yaml, DocKind::Config);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'aftr'"))
            .unwrap();
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'after'?"));
    }

    // -----------------------------------------------------------------
    // Config load profiles (stages / arrival)
    // -----------------------------------------------------------------

    #[test]
    fn config_with_stages_lints_clean() {
        let yaml = r#"
stages:
  - { duration: 30s, target: 10 }
  - { duration: 1m, target: 10 }
  - { duration: 30s, target: 0 }
"#;
        assert_eq!(lint(yaml, DocKind::Config), vec![]);
        assert!(lint_warnings(yaml, DocKind::Config).is_empty());
    }

    #[test]
    fn config_with_arrival_lints_clean() {
        let yaml = r#"
arrival:
  max_vus: 100
  pre_allocated_vus: 10
  stages:
    - { duration: 30s, rate: 5 }
    - { duration: 1m, rate: 20 }
"#;
        assert_eq!(lint(yaml, DocKind::Config), vec![]);
    }

    #[test]
    fn config_with_stages_and_arrival_is_an_error() {
        let yaml = r#"
stages:
  - { duration: 30s, target: 10 }
arrival:
  max_vus: 10
  stages:
    - { duration: 30s, rate: 5 }
"#;
        let issues = lint(yaml, DocKind::Config);
        assert!(
            issues
                .iter()
                .any(|i| i.problem.contains("mutually exclusive")),
            "{issues:?}"
        );
    }

    #[test]
    fn config_with_empty_or_broken_stages_is_an_error() {
        let issues = lint("stages: []\n", DocKind::Config);
        assert!(
            issues
                .iter()
                .any(|i| i.location == "/stages" && i.problem.contains("at least one stage")),
            "{issues:?}"
        );

        let issues = lint(
            "stages:\n  - { duration: 0s, target: 5 }\n",
            DocKind::Config,
        );
        assert!(
            issues.iter().any(|i| i.problem.contains("stages[0]")),
            "{issues:?}"
        );

        let issues = lint(
            "arrival:\n  stages:\n    - { duration: 30s, rate: 5 }\n",
            DocKind::Config,
        );
        assert!(
            issues.iter().any(|i| i.problem.contains("max_vus")),
            "{issues:?}"
        );
    }

    #[test]
    fn config_with_stages_and_fixed_fields_warns_but_passes() {
        let yaml = "vus: 10\nduration: 30s\nstages:\n  - { duration: 30s, target: 10 }\n";
        // Not an error — the profile simply wins.
        assert_eq!(lint(yaml, DocKind::Config), vec![]);
        let warnings = lint_warnings(yaml, DocKind::Config);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("override"), "{warnings:?}");
        // Test documents never warn.
        assert!(lint_warnings("steps: []\n", DocKind::Test).is_empty());
    }

    #[test]
    fn thresholds_gate_with_step_level_severity_and_message_lints_clean() {
        let yaml = r#"
vus: 1
duration: 30s
after:
  - name: slo gate
    use: std/thresholds@v1
    with:
      db_query_duration: ["p95<500", "max<2000"]
      db_query_failed: ["rate<0.05"]
      db_errors: ["count==0"]
    severity: warn
    message: "checkout SLO"
"#;
        let issues = lint(yaml, DocKind::Config);
        assert!(issues.is_empty(), "{issues:?}");
    }

    // -----------------------------------------------------------------
    // std/db-*@v1 family
    // -----------------------------------------------------------------

    #[test]
    fn db_family_lints_clean() {
        let yaml = r#"
steps:
  - name: open db
    use: std/db-connect@v1
    with:
      driver: sqlite
      dsn: "sqlite::memory:"
    outputs: db
  - name: begin tx
    use: std/db-tx-begin@v1
    with: { id: "${{ db.id }}" }
  - name: record
    use: std/db-query@v1
    with:
      id: "${{ db.id }}"
      query: INSERT INTO hits (path, status) VALUES (?, ?)
      params: ["/api/checkout", 200]
      max_rows: 100
      timeout_ms: 5000
  - name: commit
    use: std/db-tx-commit@v1
    with: { id: "${{ db.id }}" }
  - name: hang up
    use: std/db-close@v1
    with: { id: "${{ db.id }}" }
"#;
        assert_eq!(lint(yaml, DocKind::Test), vec![]);
    }

    #[test]
    fn db_query_missing_required_fields_is_reported() {
        let yaml = "steps:\n  - use: std/db-query@v1\n    with:\n      timeout_ms: 500\n";
        let issues = lint(yaml, DocKind::Test);
        for field in ["id", "query"] {
            let missing = issues
                .iter()
                .find(|i| i.problem == format!("missing required field '{field}'"))
                .unwrap_or_else(|| panic!("no issue for missing '{field}': {issues:?}"));
            assert_eq!(missing.location, "/steps/0/with");
            assert!(missing.suggestion.is_some());
        }
    }

    #[test]
    fn db_query_without_with_block_reports_every_required_field() {
        let yaml = "steps:\n  - use: std/db-query@v1\n";
        let issues = lint(yaml, DocKind::Test);
        for field in ["id", "query"] {
            assert!(
                issues
                    .iter()
                    .any(|i| i.problem == format!("missing required field '{field}'")),
                "no issue for missing '{field}': {issues:?}"
            );
        }
    }

    #[test]
    fn db_tx_and_close_missing_id_is_reported() {
        let yaml = r#"
steps:
  - use: std/db-tx-begin@v1
    with: { timeout_ms: 100 }
  - use: std/db-tx-rollback@v1
    with: {}
  - use: std/db-close@v1
"#;
        let issues = lint(yaml, DocKind::Test);
        for step in 0..3 {
            let missing = issues
                .iter()
                .find(|i| {
                    i.problem == "missing required field 'id'"
                        && i.location == format!("/steps/{step}/with")
                })
                .unwrap_or_else(|| panic!("no missing-id issue for step {step}: {issues:?}"));
            assert!(missing
                .suggestion
                .as_deref()
                .unwrap()
                .contains("db-connect"));
        }
    }

    #[test]
    fn db_connect_missing_driver_and_dsn_is_reported() {
        let yaml = "steps:\n  - use: std/db-connect@v1\n    with:\n      pool_size: 4\n";
        let issues = lint(yaml, DocKind::Test);
        for field in ["driver", "dsn"] {
            assert!(
                issues
                    .iter()
                    .any(|i| i.problem == format!("missing required field '{field}'")),
                "no issue for missing '{field}': {issues:?}"
            );
        }
    }

    #[test]
    fn db_connect_unknown_driver_is_reported() {
        let yaml =
            "steps:\n  - use: std/db-connect@v1\n    with:\n      driver: oracle\n      dsn: x\n";
        let issues = lint(yaml, DocKind::Test);
        let bad = issues
            .iter()
            .find(|i| i.problem.contains("unknown driver 'oracle'"))
            .unwrap();
        assert_eq!(bad.location, "/steps/0/with/driver");
        assert_eq!(
            bad.suggestion.as_deref(),
            Some("expected postgres, mysql, or sqlite")
        );
    }

    #[test]
    fn db_connect_interpolated_driver_is_not_flagged() {
        // Interpolated values resolve at runtime — lint cannot know them.
        let yaml = "steps:\n  - use: std/db-connect@v1\n    with:\n      driver: \"${{ vars.driver }}\"\n      dsn: \"${{ vars.dsn }}\"\n";
        assert_eq!(lint(yaml, DocKind::Test), vec![]);
    }

    #[test]
    fn graphql_valid_step_has_no_issues() {
        let yaml = r#"
steps:
  - name: fetch viewer
    use: std/graphql@v1
    with:
      url: http://localhost:4000/graphql
      query: |
        query GetViewer { viewer { id name } }
      variables: { "id": "${{ vars.id }}" }
      method: POST
      pool: shared
      introspection: false
"#;
        assert_eq!(lint(yaml, DocKind::Test), vec![]);
    }

    #[test]
    fn graphql_missing_url_and_query_is_reported() {
        let yaml = "steps:\n  - use: std/graphql@v1\n    with:\n      timeout: 500\n";
        let issues = lint(yaml, DocKind::Test);
        for field in ["url", "query"] {
            assert!(
                issues
                    .iter()
                    .any(|i| i.problem == format!("missing required field '{field}'")
                        && i.location == "/steps/0/with"),
                "no issue for missing '{field}': {issues:?}"
            );
        }
    }

    #[test]
    fn graphql_query_and_query_file_conflict_is_reported() {
        let yaml = "steps:\n  - use: std/graphql@v1\n    with:\n      url: http://x/graphql\n      query: \"{ a }\"\n      query_file: q.graphql\n";
        let issues = lint(yaml, DocKind::Test);
        assert!(issues
            .iter()
            .any(|i| i.problem.contains("mutually exclusive")));
    }

    #[test]
    fn graphql_bad_syntax_is_reported_with_location() {
        let yaml = "steps:\n  - use: std/graphql@v1\n    with:\n      url: http://x/graphql\n      query: \"{ broken {{ \"\n";
        let issues = lint(yaml, DocKind::Test);
        let syntax = issues
            .iter()
            .find(|i| i.problem.contains("invalid GraphQL syntax"))
            .unwrap_or_else(|| panic!("no syntax issue: {issues:?}"));
        assert_eq!(syntax.location, "/steps/0/with/query");
    }

    #[test]
    fn graphql_interpolated_query_skips_syntax_check() {
        // A `${{ }}` query resolves at run time — lint must not parse it.
        let yaml = "steps:\n  - use: std/graphql@v1\n    with:\n      url: http://x/graphql\n      query: \"${{ vars.q }}\"\n";
        assert_eq!(lint(yaml, DocKind::Test), vec![]);
    }

    #[test]
    fn graphql_bad_method_and_pool_are_reported() {
        let yaml = "steps:\n  - use: std/graphql@v1\n    with:\n      url: http://x/graphql\n      query: \"{ a }\"\n      method: PUT\n      pool: fancy\n";
        let issues = lint(yaml, DocKind::Test);
        assert!(issues
            .iter()
            .any(|i| i.problem == "invalid method 'PUT'" && i.location == "/steps/0/with/method"));
        assert!(issues
            .iter()
            .any(|i| i.problem == "invalid pool 'fancy'" && i.location == "/steps/0/with/pool"));
    }

    #[test]
    fn graphql_unknown_with_field_gets_did_you_mean() {
        let yaml = "steps:\n  - use: std/graphql@v1\n    with:\n      url: http://x/graphql\n      qurey: \"{ a }\"\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'qurey'"))
            .unwrap();
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'query'?"));
    }

    #[tokio::test]
    async fn graphql_remote_unreachable_endpoint_is_a_note_not_an_issue() {
        let yaml = "steps:\n  - use: std/graphql@v1\n    with:\n      url: \"http://127.0.0.1:1/graphql\"\n      query: \"{ a }\"\n";
        let (issues, notes) = lint_graphql_remote(yaml, false).await;
        assert!(issues.is_empty(), "unreachable endpoint never fails lint");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("could not introspect"));
    }

    #[tokio::test]
    async fn graphql_remote_introspection_false_skips_the_step() {
        let yaml = "steps:\n  - use: std/graphql@v1\n    with:\n      url: \"http://127.0.0.1:1/graphql\"\n      query: \"{ a }\"\n      introspection: false\n";
        let (issues, notes) = lint_graphql_remote(yaml, false).await;
        assert!(issues.is_empty() && notes.is_empty());
    }

    #[tokio::test]
    async fn graphql_remote_sdl_file_validates_offline() {
        let dir = tempfile::tempdir().unwrap();
        let sdl = dir.path().join("schema.graphql");
        std::fs::write(
            &sdl,
            "type Query { viewer: Viewer }\ntype Viewer { id: ID }",
        )
        .unwrap();
        let yaml = format!(
            "steps:\n  - use: std/graphql@v1\n    with:\n      url: \"http://x/graphql\"\n      query: \"{{ viewr {{ id }} }}\"\n      schema_file: {}\n",
            sdl.display()
        );
        // `offline: true` still validates against the SDL — no network needed.
        let (issues, _) = lint_graphql_remote(&yaml, true).await;
        assert_eq!(issues.len(), 1);
        assert!(issues[0].problem.contains("unknown field 'viewr'"));
        assert_eq!(issues[0].location, "/steps/0/with/query");
    }

    #[tokio::test]
    async fn graphql_remote_interpolated_values_skip_the_step() {
        let yaml = "steps:\n  - use: std/graphql@v1\n    with:\n      url: \"${{ vars.url }}\"\n      query: \"${{ vars.q }}\"\n";
        let (issues, notes) = lint_graphql_remote(yaml, false).await;
        assert!(issues.is_empty() && notes.is_empty());
    }

    #[test]
    fn typo_in_db_query_with_key_gets_did_you_mean() {
        let yaml =
            "steps:\n  - use: std/db-query@v1\n    with:\n      id: db-1\n      qurey: SELECT 1\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'qurey'"))
            .unwrap();
        assert_eq!(typo.location, "/steps/0/with");
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'query'?"));
        // …and the missing required field is still reported on its own.
        assert!(issues
            .iter()
            .any(|i| i.problem == "missing required field 'query'"));
    }
}
