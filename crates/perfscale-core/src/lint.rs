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
        Some("every step must name an action: `use: std/http@v1` (or the `uses:` alias) — `std/http@v1`, `std/tcp@v1`, `std/udp@v1`, `std/ws@v1`, `std/ws-connect@v1`, `std/ws-send@v1`, `std/ws-recv@v1`, `std/ws-ping@v1`, `std/ws-close@v1`, `std/grpc@v1`, `std/grpc-connect@v1`, `std/grpc-call@v1`, `std/grpc-stream-open@v1`, `std/grpc-stream-send@v1`, `std/grpc-stream-recv@v1`, `std/grpc-stream-close@v1`, `std/db-connect@v1`, `std/db-query@v1`, `std/db-tx-begin@v1`, `std/db-tx-commit@v1`, `std/db-tx-rollback@v1`, `std/db-close@v1`, `std/check@v1`, `std/sleep@v1`, `std/log@v1`, `std/file-read@v1`, `std/file-write@v1`, `std/child_process@v1`, `std/kill_process@v1`, or `std/thresholds@v1`".into())
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
    "name",
    "use",
    "uses",
    "with",
    "check",
    "outputs",
    "severity",
    "message",
];
const CONFIG_TOP_FIELDS: [&str; 7] = [
    "vus",
    "duration",
    "report",
    "before",
    "after",
    "variables",
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
const HTTP_WITH_FIELDS: [&str; 7] = [
    "method",
    "url",
    "headers",
    "body",
    "timeout",
    "insecure",
    "multipart",
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
                    "std/tcp@v1",
                    "std/udp@v1",
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
                ],
            )
            .or_else(|| {
                Some(
                    "available actions: std/http@v1, std/tcp@v1, std/udp@v1, std/ws@v1, std/ws-connect@v1, std/ws-send@v1, std/ws-recv@v1, std/ws-ping@v1, std/ws-close@v1, std/grpc@v1, std/grpc-connect@v1, std/grpc-call@v1, std/grpc-stream-open@v1, std/grpc-stream-send@v1, std/grpc-stream-recv@v1, std/grpc-stream-close@v1, std/db-connect@v1, std/db-query@v1, std/db-tx-begin@v1, std/db-tx-commit@v1, std/db-tx-rollback@v1, std/db-close@v1, std/check@v1, std/sleep@v1, std/log@v1, std/file-read@v1, std/file-write@v1, std/child_process@v1, std/kill_process@v1, std/thresholds@v1"
                        .into(),
                )
            }),
        });
    }

    if let Some(with) = map.get("with").and_then(|v| v.as_object()) {
        let with_fields: Option<&[&str]> = match action {
            "std/http@v1" | "http" => Some(&HTTP_WITH_FIELDS),
            "std/tcp@v1" | "tcp" | "std/udp@v1" | "udp" => Some(&RAW_NET_WITH_FIELDS),
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
            "std/db-tx-begin@v1" | "db-tx-begin" | "std/db-tx-commit@v1" | "db-tx-commit"
            | "std/db-tx-rollback@v1" | "db-tx-rollback" => Some(&DB_TX_WITH_FIELDS),
            "std/db-close@v1" | "db-close" => Some(&DB_CLOSE_WITH_FIELDS),
            "std/check@v1" | "check" => Some(&CHECK_FIELDS),
            "std/sleep@v1" | "sleep" => Some(&SLEEP_WITH_FIELDS),
            "std/log@v1" | "log" => Some(&LOG_WITH_FIELDS),
            "std/file-read@v1" | "file-read" => Some(&FILE_READ_WITH_FIELDS),
            "std/file-write@v1" | "file-write" => Some(&FILE_WRITE_WITH_FIELDS),
            "std/child_process@v1" | "child_process" => Some(&CHILD_PROCESS_WITH_FIELDS),
            "std/kill_process@v1" | "kill_process" => Some(&KILL_PROCESS_WITH_FIELDS),
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
    lint_db_with(action, map.get("with").and_then(|v| v.as_object()), loc, issues);

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
}

fn is_known_action(action: &str) -> bool {
    matches!(
        action,
        "std/http@v1"
            | "http"
            | "std/tcp@v1"
            | "tcp"
            | "std/udp@v1"
            | "udp"
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
        "std/db-tx-begin@v1" | "db-tx-begin"
        | "std/db-tx-commit@v1" | "db-tx-commit"
        | "std/db-tx-rollback@v1" | "db-tx-rollback"
        | "std/db-close@v1" | "db-close" => &["id"],
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

/// `Some("did you mean 'check'?")` when a known name is within edit
/// distance 2 of the typo.
fn did_you_mean(input: &str, candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .map(|c| (c, edit_distance(input, c)))
        .filter(|(_, d)| *d <= 2)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| format!("did you mean '{c}'?"))
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
        let yaml =
            "steps:\n  - use: std/http@v1\n    with:\n      url: https://x\n    check:\n      body_containz: ok\n";
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
                issues.iter().any(|i| i.problem == format!("missing required field '{field}'")),
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
                .find(|i| i.problem == "missing required field 'id'" && i.location == format!("/steps/{step}/with"))
                .unwrap_or_else(|| panic!("no missing-id issue for step {step}: {issues:?}"));
            assert!(missing.suggestion.as_deref().unwrap().contains("db-connect"));
        }
    }

    #[test]
    fn db_connect_missing_driver_and_dsn_is_reported() {
        let yaml = "steps:\n  - use: std/db-connect@v1\n    with:\n      pool_size: 4\n";
        let issues = lint(yaml, DocKind::Test);
        for field in ["driver", "dsn"] {
            assert!(
                issues.iter().any(|i| i.problem == format!("missing required field '{field}'")),
                "no issue for missing '{field}': {issues:?}"
            );
        }
    }

    #[test]
    fn db_connect_unknown_driver_is_reported() {
        let yaml = "steps:\n  - use: std/db-connect@v1\n    with:\n      driver: oracle\n      dsn: x\n";
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
    fn typo_in_db_query_with_key_gets_did_you_mean() {
        let yaml = "steps:\n  - use: std/db-query@v1\n    with:\n      id: db-1\n      qurey: SELECT 1\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'qurey'"))
            .unwrap();
        assert_eq!(typo.location, "/steps/0/with");
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'query'?"));
        // …and the missing required field is still reported on its own.
        assert!(issues.iter().any(|i| i.problem == "missing required field 'query'"));
    }
}
