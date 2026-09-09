//! HTTP transport layer — the `std/http@v1` action plus the shared plumbing
//! every HTTP-based protocol family builds on (`std/graphql@v1` today, more
//! later).
//!
//! # Shared pieces
//!
//! - [`ClientPool`] / [`client`] — connection-pool selection: the VU-pinned
//!   shards or one process-global client, secure or `insecure`.
//! - [`timed_exchange`] — send a request, time connect→body-read, decode the
//!   body, and hand back an [`HttpOutcome`]; transport failures come back
//!   with the elapsed time so the caller can log and sample uniformly.
//! - [`request_line`] / [`transport_error`] — the canonical
//!   `METHOD url → status reason (Nms)` log line and the TIMEOUT/ERROR
//!   failure output, so every HTTP-based action reports identically.
//!
//! The action itself ([`http_action`]) is one consumer of that plumbing;
//! `step::graphql` is the other in-tree consumer. The shared pieces are
//! public so downstream crates (proprietary action families such as
//! `pro/soap`) can report and measure HTTP exchanges identically to
//! `std/http@v1`; the shard constructors stay crate-private — downstream
//! code selects a client through [`client`] and [`Context::http_client_shard`].

use std::sync::OnceLock;
use std::time::Instant;

use serde_json::{json, Value};
use tokio::time::Duration;

use super::actions::{confine_fs_path, err, ActionOutput, HttpSample, LogTag};
use super::context::Context;
use super::ws::{bool_param, u64_param};

// ---------------------------------------------------------------------------
// Client pools
// ---------------------------------------------------------------------------

/// HTTP client shards: a fixed set of `reqwest::Client`s, one per available
/// CPU (capped at 16). Each VU is pinned to one shard via
/// [`Context::http_client_shard`], so connection pooling / keep-alive across
/// iterations and VUs is preserved exactly as with a single client — but a
/// *single* process-global client made hundreds of VUs contend on one hyper
/// pool mutex (the top lock in CPU profiles under load). Sharding by VU
/// rather than by calling thread matters: tokio work-stealing migrates VU
/// tasks across workers, and thread-keyed shards let every VU accumulate an
/// idle connection in every shard pool (400 VUs × N shards sockets), which
/// stalls the target under load. The per-request `timeout` parameter is
/// applied on the request builder, so the shared clients themselves carry no
/// default timeout.
pub(crate) fn shared_client(shard: usize) -> &'static reqwest::Client {
    static CLIENTS: OnceLock<Vec<reqwest::Client>> = OnceLock::new();
    let clients = CLIENTS.get_or_init(|| {
        (0..client_shard_count())
            .map(|_| reqwest::Client::new())
            .collect()
    });
    &clients[shard % clients.len()]
}

/// Like [`shared_client`], but skips TLS certificate verification — used only
/// when a step opts in with `insecure: true`. A separate shard set so secure
/// requests never share a connection pool with unverified ones.
pub(crate) fn shared_insecure_client(shard: usize) -> &'static reqwest::Client {
    static CLIENTS: OnceLock<Vec<reqwest::Client>> = OnceLock::new();
    let clients = CLIENTS.get_or_init(|| {
        (0..client_shard_count())
            .map(|_| {
                reqwest::Client::builder()
                    .danger_accept_invalid_certs(true)
                    .build()
                    .expect("insecure client construction cannot fail")
            })
            .collect()
    });
    &clients[shard % clients.len()]
}

/// Number of client shards: available parallelism, capped — more shards than
/// worker threads only fragments the connection pools.
pub(crate) fn client_shard_count() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 16)
    })
}

/// Connection-pool mode of an HTTP-based action (`pool` parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientPool {
    /// VU-pinned client shard (default) — every VU keeps one warm pool.
    PerVu,
    /// One process-global client shared by every VU: maximal connection
    /// reuse against a single endpoint, at the cost of pool-lock contention
    /// under very high VU counts.
    Shared,
}

impl ClientPool {
    /// Parse the optional `pool` parameter: `per-vu` (default) or `shared`.
    pub fn from_params(params: &Value) -> Result<Self, String> {
        match params["pool"].as_str().unwrap_or("per-vu") {
            "per-vu" => Ok(ClientPool::PerVu),
            "shared" => Ok(ClientPool::Shared),
            other => Err(format!(
                "invalid pool '{other}' — expected per-vu or shared"
            )),
        }
    }
}

/// The HTTP client for a step execution. The insecure variants skip TLS
/// verification and never share a pool with verified requests.
pub fn client(pool: ClientPool, insecure: bool, shard: usize) -> &'static reqwest::Client {
    match (pool, insecure) {
        (ClientPool::PerVu, false) => shared_client(shard),
        (ClientPool::PerVu, true) => shared_insecure_client(shard),
        (ClientPool::Shared, false) => {
            static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
            CLIENT.get_or_init(reqwest::Client::new)
        }
        (ClientPool::Shared, true) => {
            static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
            CLIENT.get_or_init(|| {
                reqwest::Client::builder()
                    .danger_accept_invalid_certs(true)
                    .build()
                    .expect("insecure client construction cannot fail")
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Timed exchange — send, read, decode, measure
// ---------------------------------------------------------------------------

/// How [`timed_exchange`] decodes the response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// `std/http@v1` semantics: textual content types decode as text,
    /// anything else surfaces as `body_base64`; a missing content type is
    /// sniffed (valid UTF-8 → text, else binary).
    Auto,
    /// Always decode as text — for JSON APIs whose bodies are UTF-8 by
    /// contract (GraphQL).
    Text,
}

/// One finished HTTP exchange, body already read and decoded.
pub struct HttpOutcome {
    pub status: u16,
    /// Canonical reason phrase (`"OK"`, `""` when unknown).
    pub reason: String,
    /// Lowercase-name JSON map for `${{ resp.headers.* }}` interpolation.
    pub headers: serde_json::Map<String, Value>,
    pub duration_ms: f64,
    pub body: String,
    /// Binary body (base64) when the payload was not textual.
    pub body_base64: Option<String>,
}

/// Send `req`, timing the whole exchange (connect → body read). A transport
/// error returns `(elapsed_ms, error)` so the caller reports timing and the
/// failure from one code path.
pub async fn timed_exchange(
    req: reqwest::RequestBuilder,
    body_kind: BodyKind,
) -> Result<HttpOutcome, (f64, reqwest::Error)> {
    let t0 = Instant::now();
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return Err((t0.elapsed().as_secs_f64() * 1000.0, e)),
    };
    let status = resp.status().as_u16();
    let reason = resp.status().canonical_reason().unwrap_or("").to_string();
    let headers = header_map_to_json(resp.headers());

    let (body, body_base64) = match body_kind {
        BodyKind::Text => (resp.text().await.unwrap_or_default(), None),
        BodyKind::Auto => {
            // Textual payloads keep the historic `body` string (reqwest's
            // charset-aware decoding); binary payloads surface as
            // `body_base64` so protobuf descriptor sets, images etc. can flow
            // into later steps (`descriptor_set: "${{ fetch.body_base64 }}"`).
            let textual = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(is_textual_content_type);
            // Headers were snapshotted above; re-reading them off `resp` is
            // not possible after consuming the body, so decode from here on.
            match textual {
                Some(true) => (resp.text().await.unwrap_or_default(), None),
                Some(false) => {
                    let bytes = resp.bytes().await.unwrap_or_default();
                    (String::new(), Some(base64_encode(&bytes)))
                }
                None => {
                    let bytes = resp.bytes().await.unwrap_or_default();
                    match std::str::from_utf8(&bytes) {
                        Ok(_) => (String::from_utf8_lossy(&bytes).into_owned(), None),
                        Err(_) => (String::new(), Some(base64_encode(&bytes))),
                    }
                }
            }
        }
    };
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
    Ok(HttpOutcome {
        status,
        reason,
        headers,
        duration_ms,
        body,
        body_base64,
    })
}

// ---------------------------------------------------------------------------
// Canonical reporting — log line and transport-failure output
// ---------------------------------------------------------------------------

/// The shared `METHOD url → status reason (Nms<extra>)` log line. `extra`
/// lets protocol families append their own fields inside the parentheses
/// (GraphQL adds `, op=…, graphql_errors=…`).
pub fn request_line(
    method: &str,
    url: &str,
    status: u16,
    reason: &str,
    duration_ms: f64,
    extra: &str,
) -> String {
    format!("{method} {url} → {status} {reason} ({duration_ms:.2}ms{extra})")
}

/// Uniform transport-failure output: a TIMEOUT/ERROR log line, the flattened
/// error chain as the value, and a failed timing sample.
pub fn transport_error(
    step_name: &str,
    method: &str,
    url: &str,
    e: &reqwest::Error,
    duration_ms: f64,
) -> ActionOutput {
    let _ = step_name;
    let detail = error_chain(e);
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

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

/// Response headers as a JSON object: lowercase names → string values, so
/// later steps can reference `${{ resp.headers.x-request-id }}`. Repeated
/// headers are joined with ", " (fine for everything except `set-cookie`,
/// where only the combined string is available). Non-UTF-8 values are
/// skipped.
pub(crate) fn header_map_to_json(
    headers: &reqwest::header::HeaderMap,
) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::with_capacity(headers.len());
    for (name, value) in headers {
        let Ok(v) = value.to_str() else { continue };
        match map.get_mut(name.as_str()) {
            Some(Value::String(existing)) => {
                existing.push_str(", ");
                existing.push_str(v);
            }
            _ => {
                map.insert(name.as_str().to_owned(), Value::String(v.to_owned()));
            }
        }
    }
    map
}

/// True for content types whose payload is meant to be read as text; anything
/// else (octet-stream, protobuf, images, …) is surfaced as `body_base64`.
fn is_textual_content_type(ct: &str) -> bool {
    let mime = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("text/")
        || mime == "application/json"
        || mime.ends_with("+json")
        || mime == "application/xml"
        || mime.ends_with("+xml")
        || mime == "application/javascript"
        || mime == "application/x-javascript"
        || mime == "application/x-www-form-urlencoded"
        || mime == "application/yaml"
        || mime == "application/x-yaml"
}

/// Standard base64 for binary response bodies.
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Flatten an error and its source chain into one line — reqwest's `Display`
/// alone is just "error sending request for url (...)", which hides the actual
/// cause (connection refused, reset, dns, ...).
pub fn error_chain(e: &dyn std::error::Error) -> String {
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
// std/http@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   method    – HTTP method, default "GET". Any valid token is accepted,
//               including extension methods like QUERY (safe method with a
//               body, draft-ietf-httpbis-safe-method-w-body)
//   url       – required
//   headers   – optional JSON object { "Name": "Value" }
//   body      – optional: JSON object → application/json, string → text/plain
//   multipart – optional array of multipart/form-data parts (mutually
//               exclusive with body). Each part: `name` plus either `value`
//               (text field) or `file` (path on disk); optional `filename`
//               (defaults to the file's basename) and `content_type`.
//               Files are read from disk each iteration — the OS page cache
//               keeps repeats cheap, and edits between runs are picked up.
//               `file` parts are filesystem access: they require
//               `allow_file_actions` and honour `fs_root` confinement.
//   timeout   – optional timeout in ms, default 10000
//   insecure  – optional bool: skip TLS certificate verification (self-signed
//               targets like `perfscale serve --tls`), default false
//   pool      – optional "per-vu" (default) or "shared" (one process-global
//               client for every VU)
//
// Output:
//   { "status": <u16>, "body": <string>, "duration_ms": <f64> }
//   Binary responses (non-textual Content-Type) instead return an empty
//   `body` plus `body_base64` — e.g. a fetched protobuf FileDescriptorSet
//   can flow into a grpc step via `${{ fetch.body_base64 }}`.

pub(crate) async fn http_action(params: &Value, step_name: &str, ctx: &Context) -> ActionOutput {
    let method = params["method"].as_str().unwrap_or("GET").to_uppercase();
    let url = match params["url"].as_str() {
        Some(u) => u.to_string(),
        None => return err(step_name, "'url' is required"),
    };
    let timeout_ms = u64_param(&params["timeout"], 10_000);
    let insecure = bool_param(&params["insecure"]);
    let pool = match ClientPool::from_params(params) {
        Ok(p) => p,
        Err(msg) => return err(step_name, &msg),
    };

    let reqwest_method = match reqwest::Method::from_bytes(method.as_bytes()) {
        Ok(m) => m,
        Err(_) => return err(step_name, &format!("invalid HTTP method '{method}'")),
    };

    let client = client(pool, insecure, ctx.http_client_shard);
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

    if !params["multipart"].is_null() {
        if !params["body"].is_null() {
            return err(step_name, "'body' and 'multipart' are mutually exclusive");
        }
        match build_multipart(&params["multipart"], step_name, ctx).await {
            Ok(form) => req = req.multipart(form),
            Err(out) => return out,
        }
    } else if !params["body"].is_null() {
        match &params["body"] {
            Value::String(s) => req = req.header("content-type", "text/plain").body(s.clone()),
            other => {
                req = req
                    .header("content-type", "application/json")
                    .body(other.to_string())
            }
        }
    }

    match timed_exchange(req, BodyKind::Auto).await {
        Ok(out) => {
            let failed = out.status >= 400;
            let mut value = json!({
                "status": out.status,
                "body": out.body,
                "duration_ms": out.duration_ms,
                "headers": out.headers,
            });
            if let Some(b64) = out.body_base64 {
                value["body_base64"] = Value::String(b64);
            }

            ActionOutput {
                value,
                logs: vec![(
                    if failed { LogTag::Err } else { LogTag::Out },
                    request_line(&method, &url, out.status, &out.reason, out.duration_ms, ""),
                )],
                success: !failed,
                http_sample: Some(HttpSample {
                    duration_ms: out.duration_ms,
                    status: out.status,
                    failed,
                }),
            }
        }
        Err((duration_ms, e)) => transport_error(step_name, &method, &url, &e, duration_ms),
    }
}

/// Build a `multipart/form-data` form from the `multipart` parameter — an
/// array of parts, each `{ name, value }` (text field) or
/// `{ name, file[, filename][, content_type] }` (file upload). Files are read
/// per call: no process-level cache, so a file edited between runs is picked
/// up (the agent is long-lived), and the OS page cache keeps per-iteration
/// reads cheap. The Content-Type header with its boundary is set by reqwest.
///
/// `file` parts are filesystem access: they require `allow_file_actions`
/// and honour the context's `fs_root` confinement (see [`confine_fs_path`]).
async fn build_multipart(
    spec: &Value,
    step_name: &str,
    ctx: &Context,
) -> Result<reqwest::multipart::Form, ActionOutput> {
    let Some(parts) = spec.as_array() else {
        return Err(err(step_name, "'multipart' must be an array of parts"));
    };
    if parts.is_empty() {
        return Err(err(step_name, "'multipart' must not be empty"));
    }

    let mut form = reqwest::multipart::Form::new();
    for (i, p) in parts.iter().enumerate() {
        let Some(name) = p["name"].as_str() else {
            return Err(err(
                step_name,
                &format!("multipart part #{i}: 'name' is required"),
            ));
        };

        if let Some(text) = p["value"].as_str() {
            form = form.text(name.to_owned(), text.to_owned());
            continue;
        }

        let Some(path) = p["file"].as_str() else {
            return Err(err(
                step_name,
                &format!("multipart part '{name}': needs 'value' (text) or 'file' (path)"),
            ));
        };
        let path = match confine_fs_path(ctx, path, false) {
            Ok(p) => p,
            Err(msg) => return Err(err(step_name, &format!("multipart part '{name}': {msg}"))),
        };
        let data = match tokio::fs::read(&path).await {
            Ok(d) => d,
            Err(e) => {
                return Err(err(
                    step_name,
                    &format!(
                        "multipart part '{name}': cannot read file '{}': {e}",
                        path.display()
                    ),
                ));
            }
        };

        let filename = p["filename"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| path.file_name().map(|f| f.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "file".to_owned());

        let mut part = reqwest::multipart::Part::bytes(data).file_name(filename);
        if let Some(ct) = p["content_type"].as_str() {
            part = match part.mime_str(ct) {
                Ok(p) => p,
                Err(_) => {
                    return Err(err(
                        step_name,
                        &format!("multipart part '{name}': invalid content_type '{ct}'"),
                    ));
                }
            };
        }
        form = form.part(name.to_owned(), part);
    }
    Ok(form)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------
    // ClientPool parsing
    // -----------------------------------------------------------------

    #[test]
    fn client_pool_defaults_to_per_vu() {
        assert_eq!(
            ClientPool::from_params(&json!({})).unwrap(),
            ClientPool::PerVu
        );
    }

    #[test]
    fn client_pool_parses_both_modes() {
        assert_eq!(
            ClientPool::from_params(&json!({ "pool": "per-vu" })).unwrap(),
            ClientPool::PerVu
        );
        assert_eq!(
            ClientPool::from_params(&json!({ "pool": "shared" })).unwrap(),
            ClientPool::Shared
        );
    }

    #[test]
    fn client_pool_rejects_unknown_mode_with_actionable_error() {
        let e = ClientPool::from_params(&json!({ "pool": "global" })).unwrap_err();
        assert!(e.contains("invalid pool 'global'"), "{e}");
        assert!(e.contains("per-vu"), "{e} lists the valid modes");
        assert!(e.contains("shared"), "{e} lists the valid modes");
    }

    // -----------------------------------------------------------------
    // Shard count
    // -----------------------------------------------------------------

    #[test]
    fn client_shard_count_is_within_bounds() {
        let n = client_shard_count();
        assert!(
            (1..=16).contains(&n),
            "shard count {n} must be clamped to 1..=16"
        );
    }

    // -----------------------------------------------------------------
    // request_line
    // -----------------------------------------------------------------

    #[test]
    fn request_line_renders_the_canonical_shape() {
        let line = request_line("GET", "http://x/health", 200, "OK", 1.234, "");
        assert_eq!(line, "GET http://x/health → 200 OK (1.23ms)");

        let with_extra = request_line(
            "POST",
            "http://x/graphql",
            200,
            "OK",
            2.0,
            ", op=Q, graphql_errors=1",
        );
        assert_eq!(
            with_extra,
            "POST http://x/graphql → 200 OK (2.00ms, op=Q, graphql_errors=1)"
        );

        // Unknown status → empty reason, no double space beyond the shape.
        let unknown = request_line("GET", "http://x/", 599, "", 0.5, "");
        assert_eq!(unknown, "GET http://x/ → 599  (0.50ms)");
    }

    // -----------------------------------------------------------------
    // header_map_to_json
    // -----------------------------------------------------------------

    #[test]
    fn header_map_to_json_lowercases_and_joins_repeats() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("X-Request-Id", "abc".parse().unwrap());
        headers.append("x-forwarded-for", "1.1.1.1".parse().unwrap());
        headers.append("x-forwarded-for", "2.2.2.2".parse().unwrap());

        let map = header_map_to_json(&headers);
        assert_eq!(map["x-request-id"], json!("abc"));
        assert_eq!(map["x-forwarded-for"], json!("1.1.1.1, 2.2.2.2"));
    }

    #[test]
    fn header_map_to_json_skips_non_utf8_values() {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        headers.insert("x-bin", HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap());
        headers.insert("x-ok", HeaderValue::from_static("yes"));

        let map = header_map_to_json(&headers);
        assert!(!map.contains_key("x-bin"));
        assert_eq!(map["x-ok"], json!("yes"));
    }

    // -----------------------------------------------------------------
    // is_textual_content_type
    // -----------------------------------------------------------------

    #[test]
    fn textual_content_types_are_recognized() {
        for ct in [
            "text/plain",
            "text/html; charset=utf-8",
            "application/json",
            "application/problem+json",
            "application/xml",
            "application/atom+xml",
            "application/javascript",
            "application/x-javascript",
            "application/x-www-form-urlencoded",
            "application/yaml",
            "application/x-yaml",
            "Application/JSON; charset=UTF-8", // case + parameter tolerance
        ] {
            assert!(is_textual_content_type(ct), "{ct} should be textual");
        }
    }

    #[test]
    fn binary_content_types_are_not_textual() {
        for ct in [
            "application/octet-stream",
            "application/protobuf",
            "image/png",
            "",
        ] {
            assert!(!is_textual_content_type(ct), "{ct} should be binary");
        }
    }

    // -----------------------------------------------------------------
    // base64_encode
    // -----------------------------------------------------------------

    #[test]
    fn base64_encode_round_trips_binary() {
        use base64::Engine as _;
        let bytes: Vec<u8> = (0u8..=255).collect();
        let encoded = base64_encode(&bytes);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        assert_eq!(decoded, bytes);
        assert_eq!(base64_encode(b""), "");
    }

    // -----------------------------------------------------------------
    // error_chain
    // -----------------------------------------------------------------

    #[test]
    fn error_chain_flattens_sources() {
        use std::error::Error;
        use std::fmt;

        #[derive(Debug)]
        struct Inner;
        impl fmt::Display for Inner {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "connection refused")
            }
        }
        impl Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl fmt::Display for Outer {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "error sending request")
            }
        }
        impl Error for Outer {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&self.0)
            }
        }

        assert_eq!(
            error_chain(&Outer(Inner)),
            "error sending request: connection refused"
        );

        // Source-less error renders as itself.
        assert_eq!(error_chain(&Inner), "connection refused");
    }

    // -----------------------------------------------------------------
    // transport_error (needs a real reqwest::Error — hit a closed port)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn transport_error_reports_failed_sample_and_error_chain() {
        // A listener that is immediately dropped leaves a guaranteed-closed
        // port → fast connection-refused error.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let url = format!("http://127.0.0.1:{port}/");
        let e = reqwest::get(&url).await.unwrap_err();
        let out = transport_error("step", "GET", &url, &e, 12.5);

        assert!(!out.success);
        let sample = out.http_sample.expect("a failed timing sample is recorded");
        assert!(sample.failed);
        assert_eq!(sample.status, 0);
        assert_eq!(sample.duration_ms, 12.5);
        // The value carries the flattened chain, not just reqwest's Display.
        let detail = out.value["error"].as_str().unwrap();
        assert!(detail.contains("error sending request"), "{detail}");
        assert_eq!(out.value["duration_ms"], json!(12.5));
        let (tag, log) = &out.logs[0];
        assert!(matches!(tag, LogTag::Err));
        assert!(log.contains("GET") && log.contains("ERROR"), "{log}");
    }
}
