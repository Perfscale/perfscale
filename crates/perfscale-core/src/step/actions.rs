//! Built-in action implementations.
//!
//! | Action ID        | What it does                                     |
//! |------------------|--------------------------------------------------|
//! | `std/http@v1`    | HTTP request (any method) with timing            |
//! | `std/tcp@v1`     | Raw TCP connect / send / receive with timing     |
//! | `std/udp@v1`     | Raw UDP send / receive with timing               |
//! | `std/check@v1`   | Assert properties of a previous step output      |
//! | `std/sleep@v1`   | Wait N milliseconds                              |
//! | `std/log@v1`     | Emit a log line                                  |
//! | `std/file-read@v1`  | Read a file (process-wide cached)            |
//! | `std/file-write@v1` | Write content to a file                      |
//!
//! # Extending with custom actions
//!
//! Downstream crates (e.g. proprietary `pro/*` actions such as `pro/fix@v1`)
//! can plug in their own actions without living in this OSS crate by
//! implementing [`ActionHandler`] and calling [`register_action`] once at
//! process start. Registered handlers are consulted only when no built-in
//! `std/*` action matches, so built-ins never pay a lookup.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};
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

/// True when any string leaf contains a `${{ ... }}` placeholder. Keys are
/// never interpolated, so only values are scanned.
fn has_placeholder(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains("${{"),
        Value::Object(m) => m.values().any(has_placeholder),
        Value::Array(a) => a.iter().any(has_placeholder),
        _ => false,
    }
}

/// Execute a step action by its ID, with interpolation already resolved.
pub async fn execute_action(
    action_id: &str,
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    // Interpolation deep-clones the whole params tree; most steps have no
    // placeholders, and this runs once per step per iteration — a cheap
    // borrow-only scan skips the clone on the hot path.
    let resolved: std::borrow::Cow<'_, Value> = if has_placeholder(params) {
        std::borrow::Cow::Owned(ctx.interpolate_value(params))
    } else {
        std::borrow::Cow::Borrowed(params)
    };

    match action_id {
        "std/http@v1" | "http" => http_action(&resolved, step_name).await,
        "std/tcp@v1" | "tcp" => tcp_action(&resolved, step_name).await,
        "std/udp@v1" | "udp" => udp_action(&resolved, step_name).await,
        "std/check@v1" | "check" => check_action(&resolved, ctx, step_name),
        "std/sleep@v1" | "sleep" => sleep_action(&resolved, step_name).await,
        "std/log@v1" | "log" => log_action(&resolved, step_name),
        "std/file-read@v1" | "file-read" => file_read_action(&resolved, step_name).await,
        "std/file-write@v1" | "file-write" => file_write_action(&resolved, step_name).await,
        unknown => {
            // No built-in match — hand off to a downstream-registered handler
            // (proprietary `pro/*` actions live outside this OSS crate). The
            // Arc is cloned out before the lock is released so no guard is held
            // across the handler's `.await`.
            let handler = {
                let reg = action_registry().read().unwrap();
                reg.iter().find(|h| h.matches(unknown)).cloned()
            };
            match handler {
                Some(h) => h.call(unknown, &resolved, ctx, step_name).await,
                None => ActionOutput {
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
    }
}

// ---------------------------------------------------------------------------
// Extension registry — pluggable custom actions
// ---------------------------------------------------------------------------

/// Boxed future returned by a registered [`ActionHandler`].
pub type ActionFuture<'a> = Pin<Box<dyn Future<Output = ActionOutput> + Send + 'a>>;

/// A pluggable action implementation supplied by a downstream crate.
///
/// Perfscale is open source; higher-tier actions (e.g. the proprietary FIX
/// protocol action `pro/fix@v1`) live in closed crates and register
/// themselves here at process start via [`register_action`]. Handlers are
/// only consulted for action IDs no built-in `std/*` action matches.
pub trait ActionHandler: Send + Sync {
    /// True when this handler serves `action_id` (e.g. `"pro/fix@v1"`).
    fn matches(&self, action_id: &str) -> bool;

    /// Execute the action. `params` already has `${{ }}` interpolation applied.
    fn call<'a>(
        &'a self,
        action_id: &'a str,
        params: &'a Value,
        ctx: &'a Context,
        step_name: &'a str,
    ) -> ActionFuture<'a>;
}

fn action_registry() -> &'static RwLock<Vec<Arc<dyn ActionHandler>>> {
    static REGISTRY: OnceLock<RwLock<Vec<Arc<dyn ActionHandler>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a custom [`ActionHandler`]. Handlers are consulted in registration
/// order. Typically called once at startup (e.g. a paid agent build enabling
/// `pro/fix@v1`); registering the same action twice just shadows the later one.
pub fn register_action(handler: Arc<dyn ActionHandler>) {
    action_registry().write().unwrap().push(handler);
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
//   timeout   – optional timeout in ms, default 10000
//   insecure  – optional bool: skip TLS certificate verification (self-signed
//               targets like `perfscale serve --tls`), default false
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

    if !params["multipart"].is_null() {
        if !params["body"].is_null() {
            return err(step_name, "'body' and 'multipart' are mutually exclusive");
        }
        match build_multipart(&params["multipart"], step_name).await {
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

    let t0 = Instant::now();
    match req.send().await {
        Ok(resp) => {
            let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let status = resp.status().as_u16();
            let reason = resp.status().canonical_reason().unwrap_or("");
            let headers = header_map_to_json(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            let failed = status >= 400;

            ActionOutput {
                value: json!({
                    "status": status,
                    "body": body,
                    "duration_ms": duration_ms,
                    "headers": headers,
                }),
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

/// Build a `multipart/form-data` form from the `multipart` parameter — an
/// array of parts, each `{ name, value }` (text field) or
/// `{ name, file[, filename][, content_type] }` (file upload). Files are read
/// per call: no process-level cache, so a file edited between runs is picked
/// up (the agent is long-lived), and the OS page cache keeps per-iteration
/// reads cheap. The Content-Type header with its boundary is set by reqwest.
async fn build_multipart(
    spec: &Value,
    step_name: &str,
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
        let data = match tokio::fs::read(path).await {
            Ok(d) => d,
            Err(e) => {
                return Err(err(
                    step_name,
                    &format!("multipart part '{name}': cannot read file '{path}': {e}"),
                ));
            }
        };

        let filename = p["filename"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                std::path::Path::new(path)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
            })
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

/// Response headers as a JSON object: lowercase names → string values, so
/// later steps can reference `${{ resp.headers.x-request-id }}`. Repeated
/// headers are joined with ", " (fine for everything except `set-cookie`,
/// where only the combined string is available). Non-UTF-8 values are
/// skipped.
fn header_map_to_json(headers: &reqwest::header::HeaderMap) -> serde_json::Map<String, Value> {
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
// std/tcp@v1
// ---------------------------------------------------------------------------
//
// Raw TCP: open a connection, optionally send a payload, optionally read a
// response, and time the whole exchange. No protocol framing — this is the
// building block for probing arbitrary line/binary services (Redis, SMTP,
// custom gateways) under load.
//
// Parameters:
//   host / port  – target; alternatively `address: "host:port"`
//   send         – optional string payload to write after connecting
//   send_base64  – optional base64 payload (mutually exclusive with `send`);
//                  use for binary protocols
//   read         – optional bool (default: true when the target is expected to
//                  reply — i.e. whenever `expect` is set, otherwise false).
//                  When true, read one chunk of the response.
//   read_bytes   – optional cap on bytes to read, default 65536
//   expect       – optional substring the response must contain (implies read)
//   timeout      – optional ms for connect + exchange, default 10000
//
// Output:
//   { "connected": <bool>, "sent": <u64>, "received": <u64>,
//     "response": <string>, "duration_ms": <f64> }
//
// The `response` is UTF-8 lossy; binary services should assert on length via a
// later step rather than the string. Timing feeds the same latency histogram
// as HTTP (reported under `http_req_duration`), so percentiles are comparable
// across transports.

async fn tcp_action(params: &Value, step_name: &str) -> ActionOutput {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpStream;

    let addr = match resolve_address(params) {
        Ok(a) => a,
        Err(msg) => return err(step_name, &msg),
    };
    let timeout_ms = params["timeout"].as_u64().unwrap_or(10_000);
    let payload = match resolve_payload(params, step_name) {
        Ok(p) => p,
        Err(out) => return out,
    };
    let expect = params["expect"].as_str();
    let want_read = params["read"].as_bool().unwrap_or(expect.is_some());
    let read_cap = params["read_bytes"].as_u64().unwrap_or(65536) as usize;

    let t0 = Instant::now();
    let exchange = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let mut stream = TcpStream::connect(&addr).await?;
        let mut sent = 0u64;
        if let Some(bytes) = &payload {
            stream.write_all(bytes).await?;
            stream.flush().await?;
            sent = bytes.len() as u64;
        }
        let mut buf = Vec::new();
        if want_read {
            let mut chunk = vec![0u8; read_cap.min(65536)];
            let n = stream.read(&mut chunk).await?;
            buf.extend_from_slice(&chunk[..n]);
        }
        Ok::<_, std::io::Error>((sent, buf))
    })
    .await;

    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    match exchange {
        Ok(Ok((sent, buf))) => {
            let response = String::from_utf8_lossy(&buf).into_owned();
            let received = buf.len() as u64;
            let matched = expect.map(|e| response.contains(e));
            let failed = matched == Some(false);
            let mut logs = vec![(
                if failed { LogTag::Err } else { LogTag::Out },
                format!(
                    "TCP {addr} → connected, sent {sent}B, recv {received}B ({duration_ms:.2}ms)"
                ),
            )];
            if failed {
                logs.push((
                    LogTag::Err,
                    format!("TCP {addr}: response did not contain {:?}", expect.unwrap()),
                ));
            }
            ActionOutput {
                value: json!({
                    "connected": true,
                    "sent": sent,
                    "received": received,
                    "response": response,
                    "duration_ms": duration_ms,
                }),
                logs,
                success: !failed,
                http_sample: Some(HttpSample {
                    duration_ms,
                    status: 0,
                    failed,
                }),
            }
        }
        Ok(Err(e)) => tcp_udp_err("TCP", &addr, &error_chain(&e), duration_ms),
        Err(_) => tcp_udp_err(
            "TCP",
            &addr,
            &format!("TIMEOUT after {duration_ms:.2}ms"),
            duration_ms,
        ),
    }
}

// ---------------------------------------------------------------------------
// std/udp@v1
// ---------------------------------------------------------------------------
//
// Raw UDP: bind an ephemeral local socket, send a datagram to the target, and
// optionally wait for a reply. Round-trip latency is measured from send to the
// reply datagram (or just the send when no reply is expected).
//
// Parameters:
//   host / port  – target; alternatively `address: "host:port"`
//   send         – string payload (or `send_base64` for binary); required
//   read         – optional bool: wait for a reply datagram
//                  (default: true when `expect` is set, otherwise false)
//   read_bytes   – optional cap on the reply size, default 65536
//   expect       – optional substring the reply must contain (implies read)
//   timeout      – optional ms for the exchange, default 10000
//
// Output:
//   { "sent": <u64>, "received": <u64>, "response": <string>,
//     "duration_ms": <f64> }
//
// UDP is connectionless: a "successful" send only means the datagram left the
// host. Set `read`/`expect` to actually validate a response.

async fn udp_action(params: &Value, step_name: &str) -> ActionOutput {
    use tokio::net::UdpSocket;

    let addr = match resolve_address(params) {
        Ok(a) => a,
        Err(msg) => return err(step_name, &msg),
    };
    let timeout_ms = params["timeout"].as_u64().unwrap_or(10_000);
    let payload = match resolve_payload(params, step_name) {
        Ok(Some(p)) => p,
        Ok(None) => return err(step_name, "'send' (or 'send_base64') is required for UDP"),
        Err(out) => return out,
    };
    let expect = params["expect"].as_str();
    let want_read = params["read"].as_bool().unwrap_or(expect.is_some());
    let read_cap = params["read_bytes"].as_u64().unwrap_or(65536) as usize;

    let t0 = Instant::now();
    let exchange = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        // ":0" lets the OS pick an ephemeral port on the matching family. UDP
        // over IPv4 targets binds 0.0.0.0; a v6 literal target would need a v6
        // bind, but host:port targets here are resolved by `connect`.
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(&addr).await?;
        socket.send(&payload).await?;
        let sent = payload.len() as u64;
        let mut buf = Vec::new();
        if want_read {
            let mut chunk = vec![0u8; read_cap.min(65536)];
            let n = socket.recv(&mut chunk).await?;
            buf.extend_from_slice(&chunk[..n]);
        }
        Ok::<_, std::io::Error>((sent, buf))
    })
    .await;

    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    match exchange {
        Ok(Ok((sent, buf))) => {
            let response = String::from_utf8_lossy(&buf).into_owned();
            let received = buf.len() as u64;
            let matched = expect.map(|e| response.contains(e));
            let failed = matched == Some(false);
            let mut logs = vec![(
                if failed { LogTag::Err } else { LogTag::Out },
                format!("UDP {addr} → sent {sent}B, recv {received}B ({duration_ms:.2}ms)"),
            )];
            if failed {
                logs.push((
                    LogTag::Err,
                    format!("UDP {addr}: reply did not contain {:?}", expect.unwrap()),
                ));
            }
            ActionOutput {
                value: json!({
                    "sent": sent,
                    "received": received,
                    "response": response,
                    "duration_ms": duration_ms,
                }),
                logs,
                success: !failed,
                http_sample: Some(HttpSample {
                    duration_ms,
                    status: 0,
                    failed,
                }),
            }
        }
        Ok(Err(e)) => tcp_udp_err("UDP", &addr, &error_chain(&e), duration_ms),
        Err(_) => tcp_udp_err(
            "UDP",
            &addr,
            &format!("TIMEOUT after {duration_ms:.2}ms"),
            duration_ms,
        ),
    }
}

/// Build a `host:port` string from either an `address` param or separate
/// `host` + `port` params.
fn resolve_address(params: &Value) -> Result<String, String> {
    if let Some(addr) = params["address"].as_str() {
        return Ok(addr.to_string());
    }
    let host = params["host"]
        .as_str()
        .ok_or_else(|| "'host' (with 'port') or 'address' is required".to_string())?;
    let port = params["port"]
        .as_u64()
        .ok_or_else(|| "'port' is required (a number)".to_string())?;
    Ok(format!("{host}:{port}"))
}

/// Resolve an outbound payload from `send` (text) or `send_base64` (binary).
/// Returns `Ok(None)` when neither is present. The two are mutually exclusive.
fn resolve_payload(params: &Value, step_name: &str) -> Result<Option<Vec<u8>>, ActionOutput> {
    let text = params["send"].as_str();
    let b64 = params["send_base64"].as_str();
    match (text, b64) {
        (Some(_), Some(_)) => Err(err(
            step_name,
            "'send' and 'send_base64' are mutually exclusive",
        )),
        (Some(s), None) => Ok(Some(s.as_bytes().to_vec())),
        (None, Some(b)) => {
            use base64::Engine as _;
            match base64::engine::general_purpose::STANDARD.decode(b) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) => Err(err(
                    step_name,
                    &format!("invalid base64 in 'send_base64': {e}"),
                )),
            }
        }
        (None, None) => Ok(None),
    }
}

/// Shared error shape for the raw transport actions: a failed sample so the
/// error still counts toward the failure rate, plus a single stderr line.
fn tcp_udp_err(proto: &str, addr: &str, detail: &str, duration_ms: f64) -> ActionOutput {
    ActionOutput {
        value: json!({ "connected": false, "error": detail, "duration_ms": duration_ms }),
        logs: vec![(LogTag::Err, format!("{proto} {addr} → ERROR: {detail}"))],
        success: false,
        http_sample: Some(HttpSample {
            duration_ms,
            status: 0,
            failed: true,
        }),
    }
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
// std/file-read@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   path     – required, file to read
//   encoding – "text" (default; file must be valid UTF-8) or "base64"
//
// Output:
//   { "content": <string>, "size": <u64>, "path": <string> }
//
// Content is cached process-wide, keyed by (path, encoding) and validated
// against the file's (mtime, len) on every access: the first iteration pays
// the disk read, subsequent iterations across all VUs hit RAM, and a file
// edited between runs of a long-lived agent is picked up via a cheap stat.

/// Actual disk reads performed by `std/file-read@v1` — observable cache behaviour
/// for tests; costs one relaxed increment per miss.
static FILE_DISK_READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

type FileCacheKey = (String, String); // (path, encoding)
type FileCacheEntry = (Option<std::time::SystemTime>, u64, std::sync::Arc<String>);

fn file_cache() -> &'static std::sync::Mutex<std::collections::HashMap<FileCacheKey, FileCacheEntry>>
{
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<FileCacheKey, FileCacheEntry>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

async fn file_read_action(params: &Value, step_name: &str) -> ActionOutput {
    let Some(path) = params["path"].as_str() else {
        return err(step_name, "'path' is required");
    };
    let encoding = params["encoding"].as_str().unwrap_or("text");
    if !matches!(encoding, "text" | "base64") {
        return err(
            step_name,
            &format!("invalid encoding '{encoding}' — use \"text\" or \"base64\""),
        );
    }

    let meta = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) => return err(step_name, &format!("cannot read file '{path}': {e}")),
    };
    let (mtime, len) = (meta.modified().ok(), meta.len());
    let key = (path.to_owned(), encoding.to_owned());

    // Never hold the lock across an await: check-release, read, insert.
    let cached = {
        let cache = file_cache().lock().unwrap();
        cache.get(&key).and_then(|(m, l, content)| {
            (*m == mtime && *l == len).then(|| std::sync::Arc::clone(content))
        })
    };

    let content = match cached {
        Some(c) => c,
        None => {
            FILE_DISK_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let bytes = match tokio::fs::read(path).await {
                Ok(b) => b,
                Err(e) => return err(step_name, &format!("cannot read file '{path}': {e}")),
            };
            let encoded = match encoding {
                "base64" => {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                }
                _ => match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => {
                        return err(
                            step_name,
                            &format!(
                                "file '{path}' is not valid UTF-8 — use `encoding: base64` for binary content"
                            ),
                        );
                    }
                },
            };
            let arc = std::sync::Arc::new(encoded);
            file_cache()
                .lock()
                .unwrap()
                .insert(key, (mtime, len, std::sync::Arc::clone(&arc)));
            arc
        }
    };

    ActionOutput {
        value: json!({ "content": content.as_str(), "size": len, "path": path }),
        // No per-iteration log line: cache hits are the hot path and a line
        // per request would spam the stream (see the yaml quiet story).
        logs: Vec::new(),
        success: true,
        http_sample: None,
    }
}

// ---------------------------------------------------------------------------
// std/file-write@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   path     – required, file to write
//   content  – required string; interpolation makes `${{ resp.body }}` the
//              typical payload
//   encoding – "text" (default: write the string as-is) or "base64"
//              (decode before writing — the inverse of file-read's base64)
//   append   – optional bool, default false (overwrite)
//
// Output:
//   { "path": <string>, "size": <u64> }   // bytes written this call
//
// Writing revalidates any `std/file-read@v1` cache entry for the same path
// automatically — the read cache is keyed by (mtime, len), which the write
// changes. With `append: true` and multiple VUs the per-call write is a
// single O_APPEND syscall, so calls do not interleave mid-content.

async fn file_write_action(params: &Value, step_name: &str) -> ActionOutput {
    let Some(path) = params["path"].as_str() else {
        return err(step_name, "'path' is required");
    };
    let Some(content) = params["content"].as_str() else {
        return err(step_name, "'content' is required (a string)");
    };
    let encoding = params["encoding"].as_str().unwrap_or("text");
    let append = params["append"].as_bool().unwrap_or(false);

    let bytes: Vec<u8> = match encoding {
        "text" => content.as_bytes().to_vec(),
        "base64" => {
            use base64::Engine as _;
            match base64::engine::general_purpose::STANDARD.decode(content) {
                Ok(b) => b,
                Err(e) => return err(step_name, &format!("invalid base64 content: {e}")),
            }
        }
        other => {
            return err(
                step_name,
                &format!("invalid encoding '{other}' — use \"text\" or \"base64\""),
            );
        }
    };

    let result = if append {
        use tokio::io::AsyncWriteExt as _;
        match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
        {
            // flush() before drop is load-bearing: tokio buffers writes and
            // closes files asynchronously on drop, so an unflushed write_all
            // can be silently lost. (tokio::fs::write in the other branch
            // flushes internally.)
            Ok(mut f) => match f.write_all(&bytes).await {
                Ok(()) => f.flush().await,
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        }
    } else {
        tokio::fs::write(path, &bytes).await
    };

    if let Err(e) = result {
        return err(step_name, &format!("cannot write file '{path}': {e}"));
    }

    ActionOutput {
        value: json!({ "path": path, "size": bytes.len() }),
        // No per-iteration log line — same hot-path reasoning as file-read.
        logs: Vec::new(),
        success: true,
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
    async fn http_action_exposes_response_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/hdr"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-request-id", "req-42")
                    .insert_header("x-multi", "a")
                    .append_header("x-multi", "b"),
            )
            .mount(&server)
            .await;

        let ctx = Context::new();
        let params = json!({ "url": format!("{}/hdr", server.uri()) });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;

        assert!(out.success);
        assert_eq!(out.value["headers"]["x-request-id"], "req-42");
        // Repeated headers are joined with ", ".
        assert_eq!(out.value["headers"]["x-multi"], "a, b");
    }

    /// The user-facing chain: request 1 → response headers → request 2
    /// reuses one of them via `${{ r1.headers.<name> }}`.
    #[tokio::test]
    async fn http_action_response_header_flows_into_next_request() {
        use wiremock::matchers::header;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/first"))
            .respond_with(ResponseTemplate::new(200).insert_header("x-session", "sess-777"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/second"))
            .and(header("x-session", "sess-777"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut ctx = Context::new();
        let first = execute_action(
            "std/http@v1",
            &json!({ "url": format!("{}/first", server.uri()) }),
            &ctx,
            "first",
        )
        .await;
        ctx.set("r1", first.value.clone());

        let second = execute_action(
            "std/http@v1",
            &json!({
                "url": format!("{}/second", server.uri()),
                "headers": { "x-session": "${{ r1.headers.x-session }}" },
            }),
            &ctx,
            "second",
        )
        .await;

        assert!(second.success, "logs: {:?}", second.logs);
        server.verify().await;
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

    // -----------------------------------------------------------------
    // std/http@v1 — multipart/form-data
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn http_action_multipart_uploads_file_and_text_fields() {
        use wiremock::matchers::body_string_contains;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("payload.bin");
        std::fs::write(&file_path, b"file-bytes-123").unwrap();

        let server = MockServer::start().await;
        // The multipart body carries each part with its content-disposition;
        // matching on those fragments pins names, filename, and contents.
        Mock::given(method("POST"))
            .and(path("/upload"))
            .and(body_string_contains("name=\"file\""))
            .and(body_string_contains("filename=\"payload.bin\""))
            .and(body_string_contains("file-bytes-123"))
            .and(body_string_contains("name=\"description\""))
            .and(body_string_contains("load test upload"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let ctx = Context::new();
        let params = json!({
            "method": "POST",
            "url": format!("{}/upload", server.uri()),
            "multipart": [
                { "name": "file", "file": file_path.to_str().unwrap(),
                  "content_type": "application/octet-stream" },
                { "name": "description", "value": "load test upload" },
            ],
        });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;

        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["status"], 201);
        server.verify().await;

        // Content-Type must be multipart/form-data with a boundary.
        let reqs = server.received_requests().await.unwrap();
        let ct = reqs[0]
            .headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.starts_with("multipart/form-data; boundary="),
            "content-type: {ct}"
        );
    }

    #[tokio::test]
    async fn http_action_multipart_custom_filename_and_interpolation() {
        use wiremock::matchers::body_string_contains;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("data.tmp");
        std::fs::write(&file_path, b"x").unwrap();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/upload"))
            .and(body_string_contains("filename=\"report.csv\""))
            .and(body_string_contains("run-77")) // interpolated field value
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut ctx = Context::new();
        ctx.set("run", json!({ "id": "run-77" }));
        let params = json!({
            "method": "POST",
            "url": format!("{}/upload", server.uri()),
            "multipart": [
                { "name": "file", "file": file_path.to_str().unwrap(),
                  "filename": "report.csv" },
                { "name": "run_id", "value": "${{ run.id }}" },
            ],
        });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;
        assert!(out.success, "logs: {:?}", out.logs);
        server.verify().await;
    }

    #[tokio::test]
    async fn http_action_multipart_missing_file_errors_without_network_call() {
        let ctx = Context::new();
        let params = json!({
            "method": "POST",
            "url": "http://127.0.0.1:1/upload",
            "multipart": [ { "name": "file", "file": "/nonexistent/nope.bin" } ],
        });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;
        assert!(!out.success);
        assert!(out.http_sample.is_none(), "no request must be attempted");
        assert!(out.logs[0].1.contains("cannot read file"), "{:?}", out.logs);
    }

    #[tokio::test]
    async fn http_action_multipart_and_body_are_mutually_exclusive() {
        let ctx = Context::new();
        let params = json!({
            "method": "POST",
            "url": "http://127.0.0.1:1/upload",
            "body": "text",
            "multipart": [ { "name": "f", "value": "v" } ],
        });
        let out = execute_action("std/http@v1", &params, &ctx, "step").await;
        assert!(!out.success);
        assert!(
            out.logs[0].1.contains("mutually exclusive"),
            "{:?}",
            out.logs
        );
    }

    #[tokio::test]
    async fn http_action_multipart_rejects_malformed_parts() {
        let ctx = Context::new();
        for (params, needle) in [
            (
                json!({ "url": "http://x/", "multipart": {} }),
                "must be an array",
            ),
            (
                json!({ "url": "http://x/", "multipart": [] }),
                "must not be empty",
            ),
            (
                json!({ "url": "http://x/", "multipart": [{ "value": "no-name" }] }),
                "'name' is required",
            ),
            (
                json!({ "url": "http://x/", "multipart": [{ "name": "f" }] }),
                "needs 'value' (text) or 'file' (path)",
            ),
        ] {
            let out = execute_action("std/http@v1", &params, &ctx, "step").await;
            assert!(!out.success);
            assert!(out.logs[0].1.contains(needle), "{:?}", out.logs);
        }
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
    // std/tcp@v1
    // -----------------------------------------------------------------

    /// Spawn a one-shot TCP echo server; returns its bound address.
    async fn spawn_tcp_echo() -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    if let Ok(n) = sock.read(&mut buf).await {
                        let _ = sock.write_all(&buf[..n]).await;
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn tcp_action_sends_and_reads_echo() {
        let addr = spawn_tcp_echo().await;
        let ctx = Context::new();
        let params = json!({ "address": addr, "send": "ping", "read": true });
        let out = execute_action("std/tcp@v1", &params, &ctx, "step").await;

        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["connected"], true);
        assert_eq!(out.value["sent"], 4);
        assert_eq!(out.value["response"], "ping");
        assert!(!out.http_sample.unwrap().failed);
    }

    #[tokio::test]
    async fn tcp_action_expect_mismatch_fails() {
        let addr = spawn_tcp_echo().await;
        let ctx = Context::new();
        let params = json!({ "address": addr, "send": "ping", "expect": "pong" });
        let out = execute_action("std/tcp@v1", &params, &ctx, "step").await;

        assert!(!out.success);
        assert!(out.http_sample.unwrap().failed);
        assert!(out.logs.iter().any(|(_, m)| m.contains("did not contain")));
    }

    #[tokio::test]
    async fn tcp_action_host_port_form_and_base64_payload() {
        let addr = spawn_tcp_echo().await;
        let (host, port) = addr.rsplit_once(':').unwrap();
        let ctx = Context::new();
        // "AQID" == bytes [1,2,3]
        let params = json!({
            "host": host, "port": port.parse::<u16>().unwrap(),
            "send_base64": "AQID", "read": true,
        });
        let out = execute_action("std/tcp@v1", &params, &ctx, "step").await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["sent"], 3);
        assert_eq!(out.value["received"], 3);
    }

    #[tokio::test]
    async fn tcp_action_connection_refused_is_failed_sample() {
        let ctx = Context::new();
        // Port 1 on loopback is not listening; connect fails fast.
        let params = json!({ "host": "127.0.0.1", "port": 1, "timeout": 2000 });
        let out = execute_action("std/tcp@v1", &params, &ctx, "step").await;
        assert!(!out.success);
        assert_eq!(out.value["connected"], false);
        assert!(out.http_sample.unwrap().failed);
    }

    #[tokio::test]
    async fn tcp_action_missing_target_errors() {
        let ctx = Context::new();
        let out = execute_action("std/tcp@v1", &json!({}), &ctx, "step").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("'host'"));
        assert!(out.http_sample.is_none());
    }

    #[tokio::test]
    async fn tcp_action_send_and_send_base64_mutually_exclusive() {
        let ctx = Context::new();
        let params = json!({ "address": "127.0.0.1:1", "send": "x", "send_base64": "eA==" });
        let out = execute_action("std/tcp@v1", &params, &ctx, "step").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("mutually exclusive"));
    }

    // -----------------------------------------------------------------
    // std/udp@v1
    // -----------------------------------------------------------------

    /// Spawn a one-shot UDP echo server; returns its bound address.
    async fn spawn_udp_echo() -> String {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok((n, peer)) = socket.recv_from(&mut buf).await {
                let _ = socket.send_to(&buf[..n], peer).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn udp_action_sends_and_reads_echo() {
        let addr = spawn_udp_echo().await;
        let ctx = Context::new();
        let params = json!({ "address": addr, "send": "ping", "read": true, "timeout": 2000 });
        let out = execute_action("std/udp@v1", &params, &ctx, "step").await;

        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["sent"], 4);
        assert_eq!(out.value["response"], "ping");
        assert!(!out.http_sample.unwrap().failed);
    }

    #[tokio::test]
    async fn udp_action_send_only_succeeds_without_reply() {
        let addr = spawn_udp_echo().await;
        let ctx = Context::new();
        // No `read`/`expect` → fire-and-forget; success once the datagram is sent.
        let params = json!({ "address": addr, "send": "fire" });
        let out = execute_action("std/udp@v1", &params, &ctx, "step").await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["received"], 0);
    }

    #[tokio::test]
    async fn udp_action_requires_payload() {
        let ctx = Context::new();
        let out = execute_action(
            "std/udp@v1",
            &json!({ "address": "127.0.0.1:9" }),
            &ctx,
            "step",
        )
        .await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("required"));
    }

    #[tokio::test]
    async fn udp_action_expect_reply_timeout_is_failed_sample() {
        let ctx = Context::new();
        // A likely-dead port with no listener: expect a reply that never comes.
        let params = json!({
            "host": "127.0.0.1", "port": 9,
            "send": "x", "expect": "y", "timeout": 200,
        });
        let out = execute_action("std/udp@v1", &params, &ctx, "step").await;
        assert!(!out.success);
        assert!(out.http_sample.unwrap().failed);
    }

    // -----------------------------------------------------------------
    // Extension registry — custom action handlers
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn registered_handler_serves_custom_action() {
        struct Echo;
        impl ActionHandler for Echo {
            fn matches(&self, id: &str) -> bool {
                id == "pro/echo@v1"
            }
            fn call<'a>(
                &'a self,
                _id: &'a str,
                params: &'a Value,
                _ctx: &'a Context,
                _step: &'a str,
            ) -> ActionFuture<'a> {
                Box::pin(async move {
                    ActionOutput {
                        value: json!({ "echoed": params["msg"].clone() }),
                        logs: Vec::new(),
                        success: true,
                        http_sample: None,
                    }
                })
            }
        }
        register_action(Arc::new(Echo));

        let ctx = Context::new();
        let out = execute_action("pro/echo@v1", &json!({ "msg": "hi" }), &ctx, "step").await;
        assert!(out.success);
        assert_eq!(out.value["echoed"], "hi");

        // An action no handler matches still reports the clear built-in error.
        let miss = execute_action("pro/nope@v1", &json!({}), &ctx, "step").await;
        assert!(!miss.success);
        assert!(miss.logs[0].1.contains("unknown action"));
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
    // std/file-read@v1 / std/file-write@v1
    // -----------------------------------------------------------------

    fn disk_reads() -> u64 {
        FILE_DISK_READS.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_read_reads_content_and_reports_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.txt");
        std::fs::write(&path, "hello fixture").unwrap();

        let ctx = Context::new();
        let params = json!({ "path": path.to_str().unwrap() });
        let out = execute_action("std/file-read@v1", &params, &ctx, "step").await;

        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["content"], "hello fixture");
        assert_eq!(out.value["size"], 13);
        assert_eq!(out.value["path"], path.to_str().unwrap());
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_read_output_interpolates_into_later_steps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.json");
        std::fs::write(&path, r#"{"kind":"fixture"}"#).unwrap();

        let mut ctx = Context::new();
        let params = json!({ "path": path.to_str().unwrap() });
        let out = execute_action("std/file-read@v1", &params, &ctx, "load").await;
        ctx.set("payload", out.value.clone());

        // The whole point: file content flows into other steps as a variable.
        let log = execute_action(
            "std/log@v1",
            &json!({ "message": "body=${{ payload.content }}" }),
            &ctx,
            "use",
        )
        .await;
        assert_eq!(log.logs[0].1, r#"body={"kind":"fixture"}"#);
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_read_caches_across_calls_and_revalidates_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cached.txt");
        std::fs::write(&path, "version-one").unwrap();
        let ctx = Context::new();
        let params = json!({ "path": path.to_str().unwrap() });

        let before = disk_reads();
        let first = execute_action("std/file-read@v1", &params, &ctx, "step").await;
        let second = execute_action("std/file-read@v1", &params, &ctx, "step").await;
        assert_eq!(first.value["content"], "version-one");
        assert_eq!(second.value["content"], "version-one");
        assert_eq!(
            disk_reads() - before,
            1,
            "second access must be served from the cache"
        );

        // Different length → (mtime, len) validation forces a re-read even
        // if the filesystem's mtime granularity hides the update.
        std::fs::write(&path, "version-two-longer").unwrap();
        let third = execute_action("std/file-read@v1", &params, &ctx, "step").await;
        assert_eq!(third.value["content"], "version-two-longer");
        assert_eq!(disk_reads() - before, 2, "changed file must be re-read");
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_read_base64_encodes_binary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        std::fs::write(&path, [0xFFu8, 0x00, 0x7F]).unwrap();

        let ctx = Context::new();
        let params = json!({ "path": path.to_str().unwrap(), "encoding": "base64" });
        let out = execute_action("std/file-read@v1", &params, &ctx, "step").await;
        assert!(out.success);
        assert_eq!(out.value["content"], "/wB/");
        assert_eq!(out.value["size"], 3);
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_read_non_utf8_text_suggests_base64() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary.bin");
        std::fs::write(&path, [0xFFu8, 0xFE]).unwrap();

        let ctx = Context::new();
        let params = json!({ "path": path.to_str().unwrap() });
        let out = execute_action("std/file-read@v1", &params, &ctx, "step").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("base64"), "{:?}", out.logs);
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_read_missing_path_and_missing_file_error() {
        let ctx = Context::new();

        let out = execute_action("std/file-read@v1", &json!({}), &ctx, "step").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("'path' is required"));

        let out = execute_action(
            "std/file-read@v1",
            &json!({ "path": "/nonexistent/nope.txt" }),
            &ctx,
            "step",
        )
        .await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("cannot read file"));

        let out = execute_action(
            "std/file-read@v1",
            &json!({ "path": "x", "encoding": "hex" }),
            &ctx,
            "step",
        )
        .await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("invalid encoding"));
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_read_alias_works() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alias.txt");
        std::fs::write(&path, "x").unwrap();
        let ctx = Context::new();
        let out = execute_action(
            "file-read",
            &json!({ "path": path.to_str().unwrap() }),
            &ctx,
            "s",
        )
        .await;
        assert!(out.success);
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_write_writes_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let ctx = Context::new();

        let out = execute_action(
            "std/file-write@v1",
            &json!({ "path": path.to_str().unwrap(), "content": "first" }),
            &ctx,
            "step",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["size"], 5);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");

        // Default mode overwrites.
        execute_action(
            "std/file-write@v1",
            &json!({ "path": path.to_str().unwrap(), "content": "second" }),
            &ctx,
            "step",
        )
        .await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_write_append_mode_accumulates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.txt");
        let ctx = Context::new();
        for line in ["one\n", "two\n"] {
            execute_action(
                "std/file-write@v1",
                &json!({ "path": path.to_str().unwrap(), "content": line, "append": true }),
                &ctx,
                "step",
            )
            .await;
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_write_base64_decodes_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        let ctx = Context::new();
        let out = execute_action(
            "std/file-write@v1",
            &json!({ "path": path.to_str().unwrap(), "content": "/wB/", "encoding": "base64" }),
            &ctx,
            "step",
        )
        .await;
        assert!(out.success);
        assert_eq!(std::fs::read(&path).unwrap(), vec![0xFFu8, 0x00, 0x7F]);
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_write_interpolates_content_from_previous_step() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resp.json");
        let mut ctx = Context::new();
        ctx.set("resp", json!({ "body": "{\"ok\":true}" }));

        // The killer use case: persist a previous step's response body.
        let out = execute_action(
            "std/file-write@v1",
            &json!({ "path": path.to_str().unwrap(), "content": "${{ resp.body }}" }),
            &ctx,
            "step",
        )
        .await;
        assert!(out.success);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"ok\":true}");
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_write_then_read_revalidates_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roundtrip.txt");
        let ctx = Context::new();
        let p = path.to_str().unwrap();

        std::fs::write(&path, "old-content").unwrap();
        let read1 = execute_action("std/file-read@v1", &json!({ "path": p }), &ctx, "r").await;
        assert_eq!(read1.value["content"], "old-content");

        // A write changes (mtime, len) → the read cache must not serve stale.
        execute_action(
            "std/file-write@v1",
            &json!({ "path": p, "content": "new-content!" }),
            &ctx,
            "w",
        )
        .await;
        let read2 = execute_action("std/file-read@v1", &json!({ "path": p }), &ctx, "r").await;
        assert_eq!(read2.value["content"], "new-content!");
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn file_write_rejects_bad_params() {
        let ctx = Context::new();
        for (params, needle) in [
            (json!({ "content": "x" }), "'path' is required"),
            (json!({ "path": "/tmp/x" }), "'content' is required"),
            (
                json!({ "path": "/tmp/x", "content": "x", "encoding": "hex" }),
                "invalid encoding",
            ),
            (
                json!({ "path": "/tmp/x", "content": "not base64!!!", "encoding": "base64" }),
                "invalid base64",
            ),
            (
                json!({ "path": "/nonexistent-dir/x", "content": "x" }),
                "cannot write file",
            ),
        ] {
            let out = execute_action("std/file-write@v1", &params, &ctx, "step").await;
            assert!(!out.success, "params should fail: {params}");
            assert!(out.logs[0].1.contains(needle), "{:?}", out.logs);
        }
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

    #[tokio::test]
    async fn execute_action_interpolates_nested_objects_and_arrays() {
        let mut ctx = Context::new();
        ctx.set("resp", json!({ "status": 201, "body": "tok-42" }));
        // GitHub-Actions-style placeholders anywhere in the value tree —
        // nested objects (headers) and array elements alike.
        let out = execute_action(
            "std/log@v1",
            &json!({
                "message": "status=${{ resp.status }}",
                "extra": { "auth": "Bearer ${{ resp.body }}" },
                "list": ["${{ resp.status }}", "plain"],
            }),
            &ctx,
            "step",
        )
        .await;
        assert_eq!(out.logs[0].1, "status=201");
    }

    // -----------------------------------------------------------------
    // has_placeholder — the hot-path gate that skips interpolation
    // -----------------------------------------------------------------

    #[test]
    fn has_placeholder_finds_placeholders_at_any_depth() {
        assert!(has_placeholder(&json!("${{ x }}")));
        assert!(has_placeholder(&json!({ "a": { "b": "${{ x.y }}" } })));
        assert!(has_placeholder(&json!({ "a": ["plain", "${{ x }}"] })));
        // Unterminated opener still counts — interpolate() decides what to
        // do with it; the gate must never skip a candidate.
        assert!(has_placeholder(&json!("broken ${{ oops")));
    }

    #[test]
    fn has_placeholder_false_for_plain_params() {
        assert!(!has_placeholder(&json!({
            "method": "POST",
            "url": "https://api.example.com/items?x=1",
            "headers": { "x-api-key": "secret" },
            "body": { "n": 3, "flag": true, "note": "no vars here, ${ not enough }" },
            "list": [1, "two", null],
        })));
    }

    #[tokio::test]
    async fn plain_params_pass_through_unchanged() {
        // No placeholders → the borrow-only fast path; output must be
        // byte-identical to what interpolation would have produced.
        let ctx = Context::new();
        let out = execute_action(
            "std/log@v1",
            &json!({ "message": "plain text with $ and { braces }" }),
            &ctx,
            "step",
        )
        .await;
        assert_eq!(out.logs[0].1, "plain text with $ and { braces }");
    }
}
