//! Built-in action implementations.
//!
//! | Action ID        | What it does                                     |
//! |------------------|--------------------------------------------------|
//! | `std/http@v1`    | HTTP request (any method) with timing            |
//! | `std/check@v1`   | Assert properties of a previous step output      |
//! | `std/sleep@v1`   | Wait N milliseconds                              |
//! | `std/log@v1`     | Emit a log line                                  |

use std::time::Instant;

use serde_json::{json, Value};
use tokio::time::Duration;

use crate::step::context::Context;

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// Result of executing one action.
#[derive(Debug, Clone)]
pub struct ActionOutput {
    /// JSON value stored in the context under the step's `outputs` name.
    pub value: Value,
    /// Log lines emitted by this action.
    pub logs: Vec<(LogTag, String)>,
    /// Whether this step is considered successful.
    pub success: bool,
    /// HTTP timing (only for `std/http@v1`).
    pub http_sample: Option<HttpSample>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogTag {
    Out, // "[out]" → stdout
    Err, // "[err]" → stderr
    Sys, // "[sys]" → system
}

/// Raw timing from one HTTP request.
///
/// Sub-millisecond precision matters here: against a fast local/loopback
/// target, most requests complete in well under 1ms — truncating to whole
/// milliseconds would round essentially every sample down to 0 and flatten
/// every percentile to "0.00ms".
#[derive(Debug, Clone)]
pub struct HttpSample {
    pub duration_ms: f64,
    pub status: u16,
    pub failed: bool,
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Execute a step action by its ID, with interpolation already resolved.
pub async fn execute_action(
    action_id: &str,
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    let resolved = ctx.interpolate_value(params);

    match action_id {
        "std/http@v1" | "http" => http_action(&resolved, step_name).await,
        "std/check@v1" | "check" => check_action(&resolved, ctx, step_name),
        "std/sleep@v1" | "sleep" => sleep_action(&resolved, step_name).await,
        "std/log@v1" | "log" => log_action(&resolved, step_name),
        unknown => ActionOutput {
            value: Value::Null,
            logs: vec![(
                LogTag::Err,
                format!("{step_name}: unknown action '{unknown}'"),
            )],
            success: false,
            http_sample: None,
        },
    }
}

// ---------------------------------------------------------------------------
// std/http@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   method   – HTTP method, default "GET". Any valid token is accepted,
//              including extension methods like QUERY (safe method with a
//              body, draft-ietf-httpbis-safe-method-w-body)
//   url      – required
//   headers  – optional JSON object { "Name": "Value" }
//   body     – optional: JSON object → application/json, string → text/plain
//   timeout  – optional timeout in ms, default 10000
//   insecure – optional bool: skip TLS certificate verification (self-signed
//              targets like `perfscale serve --tls`), default false
//
// Output:
//   { "status": <u16>, "body": <string>, "duration_ms": <f64> }

/// Process-wide HTTP client: connection pooling / keep-alive across
/// iterations and VUs. A fresh client per request would open a new TCP
/// connection every time and exhaust ephemeral ports under load. The
/// per-request `timeout` parameter is applied on the request builder, so the
/// shared client itself carries no default timeout.
fn shared_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// Like [`shared_client`], but skips TLS certificate verification — used only
/// when a step opts in with `insecure: true`. A separate client so secure
/// requests never share a connection pool with unverified ones.
fn shared_insecure_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("insecure client construction cannot fail")
    })
}

async fn http_action(params: &Value, step_name: &str) -> ActionOutput {
    let method = params["method"].as_str().unwrap_or("GET").to_uppercase();
    let url = match params["url"].as_str() {
        Some(u) => u.to_string(),
        None => return err(step_name, "'url' is required"),
    };
    let timeout_ms = params["timeout"].as_u64().unwrap_or(10_000);
    let insecure = params["insecure"].as_bool().unwrap_or(false);

    let reqwest_method = match reqwest::Method::from_bytes(method.as_bytes()) {
        Ok(m) => m,
        Err(_) => return err(step_name, &format!("invalid HTTP method '{method}'")),
    };

    let client = if insecure {
        shared_insecure_client()
    } else {
        shared_client()
    };
    let mut req = client
        .request(reqwest_method, &url)
        .timeout(Duration::from_millis(timeout_ms));

    if let Some(headers) = params["headers"].as_object() {
        for (k, v) in headers {
            if let Some(val) = v.as_str() {
                req = req.header(k.as_str(), val);
            }
        }
    }

    if !params["body"].is_null() {
        match &params["body"] {
            Value::String(s) => req = req.header("content-type", "text/plain").body(s.clone()),
            other => {
                req = req
                    .header("content-type", "application/json")
                    .body(other.to_string())
            }
        }
    }

    let t0 = Instant::now();
    match req.send().await {
        Ok(resp) => {
            let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let status = resp.status().as_u16();
            let reason = resp.status().canonical_reason().unwrap_or("");
            let body = resp.text().await.unwrap_or_default();
            let failed = status >= 400;

            ActionOutput {
                value: json!({ "status": status, "body": body, "duration_ms": duration_ms }),
                logs: vec![(
                    if failed { LogTag::Err } else { LogTag::Out },
                    format!("{method} {url} → {status} {reason} ({duration_ms:.2}ms)"),
                )],
                success: !failed,
                http_sample: Some(HttpSample {
                    duration_ms,
                    status,
                    failed,
                }),
            }
        }
        Err(e) => {
            let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let detail = error_chain(&e);
            let msg = if e.is_timeout() {
                format!("{method} {url} → TIMEOUT after {duration_ms:.2}ms")
            } else {
                format!("{method} {url} → ERROR: {detail}")
            };
            ActionOutput {
                value: json!({ "error": detail, "duration_ms": duration_ms }),
                logs: vec![(LogTag::Err, msg)],
                success: false,
                http_sample: Some(HttpSample {
                    duration_ms,
                    status: 0,
                    failed: true,
                }),
            }
        }
    }
}

/// Flatten an error and its source chain into one line — reqwest's `Display`
/// alone is just "error sending request for url (...)", which hides the actual
/// cause (connection refused, reset, dns, ...).
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

// ---------------------------------------------------------------------------
// std/check@v1
// ---------------------------------------------------------------------------
//
// Parameters (the check object itself):
//   on            – variable name to check (optional; defaults to "__last__")
//   status        – HTTP status must equal this value
//   duration_ms_lt – duration_ms must be strictly less than this value
//   body_contains  – response body must contain this string

fn check_action(params: &Value, ctx: &Context, step_name: &str) -> ActionOutput {
    let on_var = params.get("on").and_then(|v| v.as_str());
    let target = on_var
        .and_then(|name| ctx.vars.get(name))
        .or_else(|| ctx.vars.get("__last__"))
        .cloned()
        .unwrap_or(Value::Null);

    let mut all_pass = true;
    let mut logs = Vec::new();

    for (key, expected) in params.as_object().into_iter().flatten() {
        if key == "on" {
            continue;
        }

        let (pass, detail) = match key.as_str() {
            "status" => {
                let got = target["status"].as_u64().unwrap_or(0);
                let want = expected.as_u64().unwrap_or(0);
                let ok = got == want;
                (
                    ok,
                    format!(
                        "status=={want} → {} (got {got})",
                        if ok { "PASS" } else { "FAIL" }
                    ),
                )
            }
            "duration_ms_lt" => {
                let got = target["duration_ms"].as_f64().unwrap_or(f64::MAX);
                let want = expected.as_f64().unwrap_or(0.0);
                let ok = got < want;
                (
                    ok,
                    format!(
                        "duration<{want}ms → {} ({got:.2}ms)",
                        if ok { "PASS" } else { "FAIL" }
                    ),
                )
            }
            "body_contains" => {
                let body = target["body"].as_str().unwrap_or("");
                let want = expected.as_str().unwrap_or("");
                let ok = body.contains(want);
                (
                    ok,
                    format!(
                        "body contains {:?} → {}",
                        want,
                        if ok { "PASS" } else { "FAIL" }
                    ),
                )
            }
            other => (false, format!("unknown check type '{other}'")),
        };

        all_pass &= pass;
        logs.push((
            if pass { LogTag::Out } else { LogTag::Err },
            format!("[check] {step_name}: {detail}"),
        ));
    }

    ActionOutput {
        value: json!({ "passed": all_pass }),
        logs,
        success: all_pass,
        http_sample: None,
    }
}

// ---------------------------------------------------------------------------
// std/sleep@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   ms      – milliseconds to sleep (default 1000)
//   seconds – alternative to ms

async fn sleep_action(params: &Value, step_name: &str) -> ActionOutput {
    let ms = params["ms"]
        .as_u64()
        .or_else(|| params["seconds"].as_f64().map(|s| (s * 1000.0) as u64))
        .unwrap_or(1000);

    tokio::time::sleep(Duration::from_millis(ms)).await;

    ActionOutput {
        value: Value::Null,
        logs: vec![(LogTag::Sys, format!("{step_name}: sleep {ms}ms"))],
        success: true,
        http_sample: None,
    }
}

// ---------------------------------------------------------------------------
// std/log@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   message – string to emit (interpolation applied before this function)

fn log_action(params: &Value, _step_name: &str) -> ActionOutput {
    let msg = params["message"].as_str().unwrap_or("").to_string();
    ActionOutput {
        value: Value::Null,
        logs: vec![(LogTag::Out, msg)],
        success: true,
        http_sample: None,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn err(step_name: &str, msg: &str) -> ActionOutput {
    ActionOutput {
        value: Value::Null,
        logs: vec![(LogTag::Err, format!("{step_name}: {msg}"))],
        success: false,
        http_sample: None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration as StdDuration;

    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::step::context::Context;

    // -----------------------------------------------------------------
    // std/http@v1
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn http_action_success_returns_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
            .mount(&server)
            .await;

        let ctx = Context::new();
        let params = json!({ "method": "GET", "url": format!("{}/ok", server.uri()) });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;

        assert!(out.success);
        assert_eq!(out.value["status"], 200);
        assert_eq!(out.value["body"], "hello");
        let sample = out.http_sample.unwrap();
        assert_eq!(sample.status, 200);
        assert!(!sample.failed);
    }

    /// Regression: `duration_ms` used to be truncated to whole milliseconds
    /// via `Duration::as_millis() as u64`, so a fast in-process/loopback
    /// target would round every sample down to exactly 0, flattening
    /// avg/p50/p90/p95 to "0.00ms". A real round trip always takes a
    /// strictly positive amount of wall time — with the bug, that could
    /// still surface as an integer 0.
    #[tokio::test]
    async fn http_action_records_submillisecond_duration() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/fast"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let ctx = Context::new();
        let params = json!({ "url": format!("{}/fast", server.uri()) });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;

        let sample = out.http_sample.unwrap();
        assert!(
            sample.duration_ms > 0.0,
            "expected a positive sub-ms-precision duration, got {}",
            sample.duration_ms
        );
        assert_eq!(
            out.value["duration_ms"].as_f64().unwrap(),
            sample.duration_ms
        );
    }

    #[tokio::test]
    async fn http_action_4xx_marks_failed_but_returns_value() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let ctx = Context::new();
        let params = json!({ "url": format!("{}/missing", server.uri()) });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;

        assert!(!out.success);
        assert_eq!(out.value["status"], 404);
        assert!(out.http_sample.unwrap().failed);
    }

    #[tokio::test]
    async fn http_action_defaults_to_get_when_method_omitted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/default"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let ctx = Context::new();
        let params = json!({ "url": format!("{}/default", server.uri()) });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;
        assert!(out.success);
    }

    #[tokio::test]
    async fn http_action_sends_custom_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/headers"))
            .and(header("x-api-key", "secret"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let ctx = Context::new();
        let params = json!({
            "url": format!("{}/headers", server.uri()),
            "headers": { "x-api-key": "secret" },
        });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;
        assert!(out.success);
    }

    #[tokio::test]
    async fn http_action_sends_json_body_for_object() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/echo"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let ctx = Context::new();
        let params = json!({
            "method": "POST",
            "url": format!("{}/echo", server.uri()),
            "body": { "hello": "world" },
        });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;
        assert!(out.success);
    }

    #[tokio::test]
    async fn http_action_sends_string_body_as_text_plain() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/text"))
            .and(header("content-type", "text/plain"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let ctx = Context::new();
        let params = json!({
            "method": "POST",
            "url": format!("{}/text", server.uri()),
            "body": "raw text",
        });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;
        assert!(out.success);
    }

    /// QUERY (draft-ietf-httpbis-safe-method-w-body) is an extension method:
    /// safe like GET, but carries a request body. `Method::from_bytes`
    /// accepts any valid token, so this pins that QUERY — and by extension
    /// other non-registered methods — keeps working end to end with a body.
    #[tokio::test]
    async fn http_action_supports_query_method_with_body() {
        let server = MockServer::start().await;
        Mock::given(method("QUERY"))
            .and(path("/search"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"hits":3}"#))
            .mount(&server)
            .await;

        let ctx = Context::new();
        let params = json!({
            "method": "QUERY",
            "url": format!("{}/search", server.uri()),
            "body": { "q": "load testing" },
        });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;

        assert!(out.success);
        assert_eq!(out.value["status"], 200);
        assert_eq!(out.value["body"], r#"{"hits":3}"#);
        let sample = out.http_sample.unwrap();
        assert!(!sample.failed);
    }

    #[tokio::test]
    async fn http_action_insecure_flag_uses_dedicated_client() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/insecure"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // Plain-HTTP target: `insecure` only relaxes TLS verification, so the
        // request must behave identically — this pins the param wiring.
        let ctx = Context::new();
        let params = json!({ "url": format!("{}/insecure", server.uri()), "insecure": true });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;
        assert!(out.success);
        assert_eq!(out.value["status"], 200);
    }

    #[tokio::test]
    async fn http_action_missing_url_errors_without_network_call() {
        let ctx = Context::new();
        let out = execute_action("std/http@v1", &json!({}), &ctx, "step").await;
        assert!(!out.success);
        assert!(out.http_sample.is_none());
        assert!(out.logs[0].1.contains("'url' is required"));
    }

    #[tokio::test]
    async fn http_action_invalid_method_errors() {
        let ctx = Context::new();
        let params = json!({ "method": "NOT A METHOD", "url": "http://localhost/" });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("invalid HTTP method"));
    }

    #[tokio::test]
    async fn http_action_connection_refused_is_reported_as_error() {
        let ctx = Context::new();
        // Port 0 is never listening; connection should fail fast, not hang.
        let params = json!({ "url": "http://127.0.0.1:0/", "timeout": 2000 });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;
        assert!(!out.success);
        assert!(out.http_sample.unwrap().failed);
    }

    #[tokio::test]
    async fn http_action_timeout_is_reported_distinctly() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(ResponseTemplate::new(200).set_delay(StdDuration::from_millis(300)))
            .mount(&server)
            .await;

        let ctx = Context::new();
        let params = json!({ "url": format!("{}/slow", server.uri()), "timeout": 50 });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("TIMEOUT"));
    }

    // -----------------------------------------------------------------
    // std/check@v1
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn check_action_status_pass() {
        let mut ctx = Context::new();
        ctx.set("__last__", json!({ "status": 200 }));
        let out = execute_action("std/check@v1", &json!({ "status": 200 }), &ctx, "step").await;
        assert!(out.success);
        assert_eq!(out.value["passed"], true);
    }

    #[tokio::test]
    async fn check_action_status_fail() {
        let mut ctx = Context::new();
        ctx.set("__last__", json!({ "status": 500 }));
        let out = execute_action("std/check@v1", &json!({ "status": 200 }), &ctx, "step").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("FAIL"));
    }

    #[tokio::test]
    async fn check_action_duration_ms_lt_pass_and_fail() {
        let mut ctx = Context::new();
        ctx.set("__last__", json!({ "duration_ms": 50 }));

        let pass = execute_action(
            "std/check@v1",
            &json!({ "duration_ms_lt": 100 }),
            &ctx,
            "step",
        )
        .await;
        assert!(pass.success);

        let fail = execute_action(
            "std/check@v1",
            &json!({ "duration_ms_lt": 10 }),
            &ctx,
            "step",
        )
        .await;
        assert!(!fail.success);
    }

    #[tokio::test]
    async fn check_action_duration_ms_lt_handles_fractional_values() {
        let mut ctx = Context::new();
        ctx.set("__last__", json!({ "duration_ms": 0.4 }));

        let pass = execute_action(
            "std/check@v1",
            &json!({ "duration_ms_lt": 1.0 }),
            &ctx,
            "step",
        )
        .await;
        assert!(pass.success);

        let fail = execute_action(
            "std/check@v1",
            &json!({ "duration_ms_lt": 0.2 }),
            &ctx,
            "step",
        )
        .await;
        assert!(!fail.success);
    }

    #[tokio::test]
    async fn check_action_body_contains_pass_and_fail() {
        let mut ctx = Context::new();
        ctx.set("__last__", json!({ "body": "hello world" }));

        let pass = execute_action(
            "std/check@v1",
            &json!({ "body_contains": "world" }),
            &ctx,
            "step",
        )
        .await;
        assert!(pass.success);

        let fail = execute_action(
            "std/check@v1",
            &json!({ "body_contains": "missing" }),
            &ctx,
            "step",
        )
        .await;
        assert!(!fail.success);
    }

    #[tokio::test]
    async fn check_action_unknown_type_fails() {
        let ctx = Context::new();
        let out = execute_action("std/check@v1", &json!({ "frobnicate": 1 }), &ctx, "step").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("unknown check type"));
    }

    #[tokio::test]
    async fn check_action_targets_named_variable_via_on() {
        let mut ctx = Context::new();
        ctx.set("resp", json!({ "status": 201 }));
        ctx.set("__last__", json!({ "status": 999 })); // should be ignored when "on" is given
        let out = execute_action(
            "std/check@v1",
            &json!({ "on": "resp", "status": 201 }),
            &ctx,
            "step",
        )
        .await;
        assert!(out.success);
    }

    #[tokio::test]
    async fn check_action_missing_target_fails_gracefully() {
        let ctx = Context::new(); // no __last__ set at all
        let out = execute_action("std/check@v1", &json!({ "status": 200 }), &ctx, "step").await;
        assert!(!out.success);
    }

    // -----------------------------------------------------------------
    // std/sleep@v1
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn sleep_action_uses_ms_param() {
        let ctx = Context::new();
        let start = std::time::Instant::now();
        let out = execute_action("std/sleep@v1", &json!({ "ms": 10 }), &ctx, "step").await;
        assert!(start.elapsed() >= StdDuration::from_millis(10));
        assert!(out.success);
        assert!(out.logs[0].1.contains("sleep 10ms"));
    }

    #[tokio::test]
    async fn sleep_action_uses_seconds_param() {
        let ctx = Context::new();
        let out = execute_action("std/sleep@v1", &json!({ "seconds": 0.01 }), &ctx, "step").await;
        assert!(out.logs[0].1.contains("sleep 10ms"));
    }

    // -----------------------------------------------------------------
    // std/log@v1
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn log_action_emits_message() {
        let ctx = Context::new();
        let out = execute_action(
            "std/log@v1",
            &json!({ "message": "hi there" }),
            &ctx,
            "step",
        )
        .await;
        assert!(out.success);
        assert_eq!(out.logs[0].1, "hi there");
    }

    #[tokio::test]
    async fn log_action_defaults_to_empty_message() {
        let ctx = Context::new();
        let out = execute_action("std/log@v1", &json!({}), &ctx, "step").await;
        assert_eq!(out.logs[0].1, "");
    }

    // -----------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn execute_action_supports_short_aliases() {
        let ctx = Context::new();
        let out = execute_action("log", &json!({ "message": "via alias" }), &ctx, "step").await;
        assert_eq!(out.logs[0].1, "via alias");
    }

    #[tokio::test]
    async fn execute_action_unknown_action_fails() {
        let ctx = Context::new();
        let out = execute_action("does/not@exist", &json!({}), &ctx, "step").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("unknown action"));
    }

    #[tokio::test]
    async fn execute_action_interpolates_params_before_dispatch() {
        let mut ctx = Context::new();
        ctx.set("name", json!("world"));
        let out = execute_action(
            "std/log@v1",
            &json!({ "message": "hello ${{ name }}" }),
            &ctx,
            "step",
        )
        .await;
        assert_eq!(out.logs[0].1, "hello world");
    }
}
