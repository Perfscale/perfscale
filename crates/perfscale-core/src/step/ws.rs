//! WebSocket actions.
//!
//! | Action ID           | What it does                                         |
//! |---------------------|------------------------------------------------------|
//! | `std/ws@v1`         | One-shot session: connect → messages → close         |
//! | `std/ws-connect@v1` | Open a Live Connection, return its Connection ID     |
//! | `std/ws-send@v1`    | Send message(s) on a Live Connection                 |
//! | `std/ws-recv@v1`    | Read from a Live Connection until a stopping rule    |
//! | `std/ws-ping@v1`    | Ping → pong round-trip on a Live Connection          |
//! | `std/ws-close@v1`   | Gracefully close a Live Connection                   |
//!
//! # Connection Profile vs Live Connection
//!
//! A **Connection Profile** is plain data — `url`, `headers`, `subprotocols`,
//! `skipTLSVerify` — passed as the `connection` parameter (an object, or the
//! JSON string a `${{ config.x }}` interpolation yields). Profile fields can
//! also be given inline; inline fields override the profile.
//!
//! A **Live Connection** is an open socket held across steps within one VU
//! iteration. `std/ws-connect@v1` returns `{ "id": "ws-1", ... }`; later steps
//! address it via `id: "${{ feed.id }}"`. Whatever a scenario leaves open is
//! dropped at iteration end *without* a Close handshake — `std/ws-close@v1`
//! is the graceful path. Connect inside `before:` setup is not useful: the
//! setup context (and its sockets) is gone before any VU starts.
//!
//! # Reading and asserting
//!
//! `std/ws-recv@v1` stops on an **until-condition** — `until_contains`
//! (substring), `until_json` (JSON-subset), or plain `count` — bounded by
//! `timeout`. Not reaching the condition fails the step. Content assertions
//! beyond the stopping rule belong to `std/check@v1` over the step's
//! `messages` output (see `message_contains` / `message_matches`).
//!
//! Received text frames appear in `messages` as strings; binary frames as
//! base64 strings (also concatenated into `body`, newline-joined, for
//! `body_contains` checks).
//!
//! # Dynamic messages
//!
//! Text payloads may embed single-brace `${…}` tokens ([`crate::generate`]),
//! expanded per send, with `repeat` + `interval_ms` emitting a stream from one
//! template:
//!
//! ```yaml
//! - uses: std/ws-send@v1
//!   with:
//!     id: "${{ feed.id }}"
//!     send: '{"op":"order","id":"ord-${seq}","px":${randf(1.05,1.15,5)}}'
//!     repeat: 100
//!     interval_ms: 50
//! ```
//!
//! # Metrics
//!
//! The connect handshake and the one-shot session feed `http_req_duration`
//! (comparable to http/tcp/udp). Waiting on a server-push stream is *not*
//! target latency, so `ws-recv` deliberately reports only its own
//! `duration_ms`. Custom metrics: `ws_msgs_sent` / `ws_msgs_received`
//! (rates) and `ws_msg_rtt` (histogram; time from a send to the first
//! message matching the user's until-condition). Ping→pong time is in
//! `ws-ping`'s `duration_ms` output and deliberately not aggregated.

use std::sync::Arc;
use std::time::Instant;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::Connector;

use super::actions::{err, error_chain, ActionOutput, HttpSample, LogTag};
use super::context::Context;
use super::resources::{WsConn, WsStream};
use crate::generate::Gen;

// ---------------------------------------------------------------------------
// Connection Profile
// ---------------------------------------------------------------------------

/// Resolved connection parameters — the profile merged with inline fields.
struct Profile {
    url: String,
    headers: Vec<(String, String)>,
    subprotocols: Vec<String>,
    skip_tls_verify: bool,
}

/// Merge the `connection` profile (object or JSON string) with inline
/// parameters; inline wins. `url` is required and must be `ws://` or `wss://`.
fn resolve_profile(params: &Value) -> Result<Profile, String> {
    // The profile arrives as an object, or as the JSON string that
    // `connection: "${{ config.x }}"` interpolation yields.
    let profile: Value = match params.get("connection") {
        Some(Value::Object(m)) => Value::Object(m.clone()),
        Some(Value::String(s)) => {
            serde_json::from_str(s).map_err(|_| "'connection' string is not valid JSON")?
        }
        Some(_) => return Err("'connection' must be an object or a JSON string".into()),
        None => Value::Object(Default::default()),
    };

    let field = |key: &str| -> Option<Value> {
        params
            .get(key)
            .filter(|v| !v.is_null())
            .or_else(|| profile.get(key).filter(|v| !v.is_null()))
            .cloned()
    };

    let url = field("url")
        .as_ref()
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or("'url' is required (inline or via 'connection')")?;
    if !url.starts_with("ws://") && !url.starts_with("wss://") {
        return Err(format!("'url' must be ws:// or wss://, got '{url}'"));
    }

    let mut headers = Vec::new();
    if let Some(Value::Object(m)) = field("headers") {
        for (k, v) in m {
            let v = match v {
                Value::String(s) => s,
                other => other.to_string(),
            };
            headers.push((k, v));
        }
    }

    let subprotocols = match field("subprotocols") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(s)) => vec![s],
        _ => Vec::new(),
    };

    Ok(Profile {
        url,
        headers,
        subprotocols,
        skip_tls_verify: field("skipTLSVerify").is_some_and(|v| bool_param(&v)),
    })
}

/// Accept `true` / `"true"` — interpolated `${{ … }}` values are always
/// strings, so bool params must take both forms.
fn bool_param(v: &Value) -> bool {
    v.as_bool()
        .or_else(|| v.as_str().map(|s| s.eq_ignore_ascii_case("true")))
        .unwrap_or(false)
}

/// Accept `10000` / `"10000"` (same rationale as [`bool_param`]).
fn u64_param(v: &Value, default: u64) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// Open the WebSocket: TCP + (for `wss://`) TLS + upgrade handshake. Returns
/// the stream and the server-negotiated subprotocol, if any.
async fn ws_handshake(profile: &Profile) -> Result<(WsStream, Option<String>), String> {
    let mut request = profile
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("invalid url: {e}"))?;

    for (name, value) in &profile.headers {
        let name: tokio_tungstenite::tungstenite::http::HeaderName = name
            .parse()
            .map_err(|_| format!("invalid header name '{name}'"))?;
        let value = value
            .parse()
            .map_err(|_| format!("invalid header value for '{name}'"))?;
        request.headers_mut().insert(name, value);
    }
    if !profile.subprotocols.is_empty() {
        let joined = profile.subprotocols.join(", ");
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            joined
                .parse()
                .map_err(|_| format!("invalid subprotocols '{joined}'"))?,
        );
    }

    // `skipTLSVerify` swaps in a verifier that accepts any certificate — for
    // self-signed staging gateways only. Otherwise tungstenite's default
    // rustls connector (webpki roots) applies.
    let connector = if profile.skip_tls_verify {
        Some(Connector::Rustls(Arc::new(no_verify_tls_config()?)))
    } else {
        None
    };

    let (stream, response) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector)
            .await
            .map_err(|e| error_chain(&e))?;

    let subprotocol = response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    Ok((stream, subprotocol))
}

/// A rustls client config whose verifier accepts any server certificate.
/// Signature checks still run (the handshake stays well-formed); the chain
/// and hostname are not validated. Opt-in via `skipTLSVerify: true` only —
/// never point it at production you don't control.
fn no_verify_tls_config() -> Result<rustls::ClientConfig, String> {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    #[derive(Debug)]
    struct NoVerify(Arc<CryptoProvider>);

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    let provider = Arc::new(ring::default_provider());
    Ok(
        rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
            .with_no_client_auth(),
    )
}

// ---------------------------------------------------------------------------
// Message plumbing shared by recv / ping / one-shot
// ---------------------------------------------------------------------------

/// A received data frame as it appears in the `messages` output: text frames
/// as strings, binary frames as base64 strings.
fn frame_to_value(msg: &Message) -> Option<Value> {
    use base64::Engine as _;
    match msg {
        Message::Text(t) => Some(Value::String(t.to_string())),
        Message::Binary(b) => Some(Value::String(
            base64::engine::general_purpose::STANDARD.encode(b),
        )),
        _ => None,
    }
}

/// The stopping rule of a receive: read until a message matches, or until
/// `count` data messages arrived (when no match rule is given).
enum Until {
    Count(u64),
    Contains(String),
    Json(Value),
}

impl Until {
    fn from_params(params: &Value) -> Result<Until, String> {
        match (params.get("until_contains"), params.get("until_json")) {
            (Some(_), Some(_)) => {
                Err("'until_contains' and 'until_json' are mutually exclusive".into())
            }
            (Some(v), None) => match v.as_str() {
                Some(s) => Ok(Until::Contains(s.to_string())),
                None => Err("'until_contains' must be a string".into()),
            },
            (None, Some(v)) => match v {
                Value::Object(_) => Ok(Until::Json(v.clone())),
                _ => Err("'until_json' must be an object".into()),
            },
            (None, None) => Ok(Until::Count(
                params.get("count").map(|v| u64_param(v, 1)).unwrap_or(1),
            )),
        }
    }

    /// Does this received message satisfy the rule? (`Count` never matches a
    /// single message — the caller counts.)
    fn matches(&self, received: &Value) -> bool {
        match self {
            Until::Count(_) => false,
            Until::Contains(needle) => received.as_str().is_some_and(|s| s.contains(needle)),
            Until::Json(pattern) => received
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .is_some_and(|parsed| json_subset_match(pattern, &parsed)),
        }
    }
}

/// True when every field of `pattern` equals the corresponding field of
/// `actual` (recursively for nested objects); extra fields in `actual` are
/// ignored. Non-object patterns fall back to strict equality.
pub(crate) fn json_subset_match(pattern: &Value, actual: &Value) -> bool {
    match (pattern, actual) {
        (Value::Object(p), Value::Object(a)) => p
            .iter()
            .all(|(k, pv)| a.get(k).is_some_and(|av| json_subset_match(pv, av))),
        (p, a) => p == a,
    }
}

/// What a read loop ended with.
struct RecvOutcome {
    messages: Vec<Value>,
    /// True when the until-condition matched (`Count` rules: count reached).
    satisfied: bool,
    /// Send→match Message RTT, when a match happened and a send preceded it.
    rtt_ms: Option<f64>,
    /// The peer closed (or the transport died) during the read.
    closed: bool,
    error: Option<String>,
}

/// Read from `stream` until `until` is satisfied or `deadline` passes.
/// `pending` (messages a prior `ws-ping` buffered) is consumed first;
/// `sent_at` anchors the Message RTT measurement.
async fn read_until(
    stream: &mut WsStream,
    pending: &mut std::collections::VecDeque<Value>,
    until: &Until,
    deadline: Instant,
    sent_at: Option<Instant>,
) -> RecvOutcome {
    let mut messages = Vec::new();
    let out = |messages: Vec<Value>, satisfied: bool, rtt_ms, closed, error| RecvOutcome {
        messages,
        satisfied,
        rtt_ms,
        closed,
        error,
    };

    let target = match until {
        Until::Count(n) => *n,
        _ => u64::MAX,
    };

    // Pending first — a ping must not swallow data messages.
    while let Some(v) = pending.pop_front() {
        let matched = until.matches(&v);
        messages.push(v);
        if matched {
            let rtt = sent_at.map(|t| t.elapsed().as_secs_f64() * 1000.0);
            return out(messages, true, rtt, false, None);
        }
        if messages.len() as u64 >= target {
            return out(messages, true, None, false, None);
        }
    }

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return out(messages, false, None, false, None);
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Err(_) => return out(messages, false, None, false, None),
            Ok(None) => return out(messages, false, None, true, None),
            Ok(Some(Err(e))) => {
                return out(messages, false, None, true, Some(error_chain(&e)));
            }
            Ok(Some(Ok(frame))) => {
                if matches!(frame, Message::Close(_)) {
                    return out(messages, false, None, true, None);
                }
                let Some(v) = frame_to_value(&frame) else {
                    continue; // ping/pong — transport noise, not a Message
                };
                let matched = until.matches(&v);
                messages.push(v);
                if matched {
                    let rtt = sent_at.map(|t| t.elapsed().as_secs_f64() * 1000.0);
                    return out(messages, true, rtt, false, None);
                }
                if messages.len() as u64 >= target {
                    return out(messages, true, None, false, None);
                }
            }
        }
    }
}

/// Newline-join the text form of received messages, for `body_contains`.
fn joined_body(messages: &[Value]) -> String {
    messages
        .iter()
        .map(|m| m.as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decode the send payload: `send` (a `${…}` template, expanded per send) or
/// `send_base64` (binary, decoded once, no expansion).
enum Payload {
    Template(String),
    Binary(Vec<u8>),
}

impl Payload {
    fn from_params(params: &Value) -> Result<Payload, String> {
        use base64::Engine as _;
        match (params.get("send"), params.get("send_base64")) {
            (Some(_), Some(_)) => Err("'send' and 'send_base64' are mutually exclusive".into()),
            (Some(v), None) => match v {
                Value::String(s) => Ok(Payload::Template(s.clone())),
                other => Ok(Payload::Template(other.to_string())),
            },
            (None, Some(v)) => {
                let s = v.as_str().ok_or("'send_base64' must be a string")?;
                base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map(Payload::Binary)
                    .map_err(|e| format!("invalid base64 in 'send_base64': {e}"))
            }
            (None, None) => Err("'send' (or 'send_base64') is required".into()),
        }
    }

    /// Materialize one wire message, expanding `${…}` for text templates.
    fn render(&self, generator: &mut Gen) -> Message {
        match self {
            Payload::Template(t) => {
                generator.begin_message();
                Message::Text(generator.expand(t).into())
            }
            Payload::Binary(b) => Message::Binary(b.clone().into()),
        }
    }
}

/// Send `payload` `repeat` times, `interval_ms` apart. Returns (count, bytes).
async fn send_repeated(
    stream: &mut WsStream,
    generator: &mut Gen,
    payload: &Payload,
    repeat: u64,
    interval_ms: u64,
    deadline: Instant,
) -> Result<(u64, u64), String> {
    let mut sent = 0u64;
    let mut bytes = 0u64;
    for i in 0..repeat.max(1) {
        if i > 0 && interval_ms > 0 {
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
        if Instant::now() >= deadline {
            return Err(format!("timeout after {sent} of {repeat} sends"));
        }
        let msg = payload.render(generator);
        bytes += msg.len() as u64;
        stream.send(msg).await.map_err(|e| error_chain(&e))?;
        sent += 1;
    }
    Ok((sent, bytes))
}

// ---------------------------------------------------------------------------
// std/ws-connect@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   connection    – Connection Profile (object, or `${{ config.x }}` JSON string)
//   url           – ws:// or wss:// target (inline; overrides the profile)
//   headers       – map of extra handshake headers (auth tokens etc.)
//   subprotocols  – list (or single string) for Sec-WebSocket-Protocol
//   skipTLSVerify – accept any server certificate (self-signed staging only)
//   timeout       – ms for TCP+TLS+upgrade, default 10000
//
// Output:
//   { "id": "ws-1", "connected": true, "subprotocol": <string|null>,
//     "duration_ms": <f64> }
//
// The handshake feeds `http_req_duration` — it has a well-defined start and
// end that the target controls, so it is comparable to http/tcp/udp timing.

pub(crate) async fn ws_connect_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    let profile = match resolve_profile(params) {
        Ok(p) => p,
        Err(msg) => return err(step_name, &msg),
    };
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);

    let t0 = Instant::now();
    let handshake =
        tokio::time::timeout(Duration::from_millis(timeout_ms), ws_handshake(&profile)).await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let (stream, subprotocol) = match handshake {
        Ok(Ok(pair)) => pair,
        Ok(Err(msg)) => return ws_err(step_name, &profile.url, &msg, duration_ms),
        Err(_) => {
            return ws_err(
                step_name,
                &profile.url,
                &format!("handshake TIMEOUT after {duration_ms:.2}ms"),
                duration_ms,
            )
        }
    };

    let url = profile.url;
    let id = ctx.resources.insert(WsConn {
        stream,
        url: url.clone(),
        generator: Gen::new(uuid::Uuid::new_v4().as_u128() as u64),
        last_send: None,
        pending: Default::default(),
    });

    ActionOutput {
        value: json!({
            "id": id,
            "connected": true,
            "subprotocol": subprotocol,
            "duration_ms": duration_ms,
        }),
        logs: vec![(
            LogTag::Out,
            format!("WS connect {url} → {id} ({duration_ms:.2}ms)"),
        )],
        success: true,
        http_sample: Some(HttpSample {
            duration_ms,
            status: 0,
            failed: false,
        }),
    }
}

/// Failure with handshake timing recorded (failed sample → error-rate metric).
fn ws_err(step_name: &str, url: &str, detail: &str, duration_ms: f64) -> ActionOutput {
    ActionOutput {
        value: json!({ "connected": false, "error": detail, "duration_ms": duration_ms }),
        logs: vec![(LogTag::Err, format!("{step_name}: WS {url}: {detail}"))],
        success: false,
        http_sample: Some(HttpSample {
            duration_ms,
            status: 0,
            failed: true,
        }),
    }
}

/// Look up the Live Connection for `params.id`, or explain what went wrong.
fn take_conn(params: &Value, ctx: &Context) -> Result<(String, WsConn), String> {
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("'id' is required (the output of std/ws-connect@v1)")?;
    let conn = ctx.resources.take(id).ok_or(format!(
        "unknown connection id '{id}' — not connected in this iteration, already closed, \
         or opened in `before:` setup (connections do not cross into VU iterations)"
    ))?;
    Ok((id.to_string(), conn))
}

// ---------------------------------------------------------------------------
// std/ws-send@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   id           – Connection ID from std/ws-connect@v1 (required)
//   send         – text payload; may embed `${…}` tokens, expanded per send
//   send_base64  – binary payload (mutually exclusive with `send`)
//   repeat       – how many messages to emit from the template, default 1
//   interval_ms  – gap between repeated sends, default 0
//   timeout      – ms for the whole send loop, default 10000
//
// Output:
//   { "sent": <u64>, "bytes": <u64>, "duration_ms": <f64>,
//     "metrics": { "ws_msgs_sent": <u64> } }
//
// No latency-histogram sample: a send only measures the local write. The
// send instant is remembered so a following ws-recv with an until-condition
// reports the send→match Message RTT.

pub(crate) async fn ws_send_action(params: &Value, ctx: &Context, step_name: &str) -> ActionOutput {
    let (id, mut conn) = match take_conn(params, ctx) {
        Ok(x) => x,
        Err(msg) => return err(step_name, &msg),
    };
    let payload = match Payload::from_params(params) {
        Ok(p) => p,
        Err(msg) => {
            ctx.resources.put_back(&id, conn);
            return err(step_name, &msg);
        }
    };
    let repeat = params.get("repeat").map(|v| u64_param(v, 1)).unwrap_or(1);
    let interval_ms = params
        .get("interval_ms")
        .map(|v| u64_param(v, 0))
        .unwrap_or(0);
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);

    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(timeout_ms);
    let result = send_repeated(
        &mut conn.stream,
        &mut conn.generator,
        &payload,
        repeat,
        interval_ms,
        deadline,
    )
    .await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    match result {
        Ok((sent, bytes)) => {
            conn.last_send = Some(Instant::now());
            let url = conn.url.clone();
            ctx.resources.put_back(&id, conn);
            ActionOutput {
                value: json!({
                    "sent": sent,
                    "bytes": bytes,
                    "duration_ms": duration_ms,
                    "metrics": { "ws_msgs_sent": sent },
                }),
                logs: vec![(
                    LogTag::Out,
                    format!("WS {url} [{id}] → sent {sent} msg(s), {bytes}B ({duration_ms:.2}ms)"),
                )],
                success: true,
                http_sample: None,
            }
        }
        // A dead transport means the connection is unusable — drop it rather
        // than parking a broken socket under a still-valid-looking id.
        Err(msg) => err(step_name, &format!("WS send [{id}]: {msg}")),
    }
}

// ---------------------------------------------------------------------------
// std/ws-recv@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   id             – Connection ID (required)
//   count          – stop after N data messages (default 1; ignored when an
//                    until rule is given)
//   until_contains – stop when a message contains this substring
//   until_json     – stop when a message JSON-subset-matches this object
//   timeout        – ms, default 10000. Not reaching the stopping rule in
//                    time FAILS the step.
//
// Output:
//   { "messages": [<string>...], "body": <joined>, "count": <u64>,
//     "matched": <bool>, "duration_ms": <f64>,
//     "metrics": { "ws_msgs_received": <u64>, "ws_msg_rtt": [<f64>]? } }
//
// `matched` is true when the until rule fired (for `count` mode: the count
// was reached). `ws_msg_rtt` appears only when an until rule matched and a
// ws-send preceded it on this connection — the send→match application RTT.

pub(crate) async fn ws_recv_action(params: &Value, ctx: &Context, step_name: &str) -> ActionOutput {
    let (id, mut conn) = match take_conn(params, ctx) {
        Ok(x) => x,
        Err(msg) => return err(step_name, &msg),
    };
    let until = match Until::from_params(params) {
        Ok(u) => u,
        Err(msg) => {
            ctx.resources.put_back(&id, conn);
            return err(step_name, &msg);
        }
    };
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);

    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(timeout_ms);
    let sent_at = conn.last_send.take();
    let mut pending = std::mem::take(&mut conn.pending);
    let outcome = read_until(&mut conn.stream, &mut pending, &until, deadline, sent_at).await;
    conn.pending = pending;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let url = conn.url.clone();
    if outcome.closed {
        // Peer is gone — a parked dead socket would just fail the next step
        // with a less honest message.
        drop(conn);
    } else {
        ctx.resources.put_back(&id, conn);
    }

    let received = outcome.messages.len() as u64;
    let mut metrics = json!({ "ws_msgs_received": received });
    if let Some(rtt) = outcome.rtt_ms {
        metrics["ws_msg_rtt"] = json!([rtt]);
    }

    let mut value = json!({
        "messages": outcome.messages,
        "body": joined_body(&outcome.messages),
        "count": received,
        "matched": outcome.satisfied,
        "duration_ms": duration_ms,
        "metrics": metrics,
    });

    let mut logs = vec![(
        if outcome.satisfied {
            LogTag::Out
        } else {
            LogTag::Err
        },
        format!("WS {url} [{id}] ← {received} msg(s) ({duration_ms:.2}ms)"),
    )];
    if let Some(e) = &outcome.error {
        value["error"] = json!(e);
        logs.push((LogTag::Err, format!("{step_name}: WS recv [{id}]: {e}")));
    } else if !outcome.satisfied {
        let why = if outcome.closed {
            "connection closed before the stopping rule was reached"
        } else {
            "timeout before the stopping rule was reached"
        };
        logs.push((LogTag::Err, format!("{step_name}: WS recv [{id}]: {why}")));
    }

    ActionOutput {
        value,
        logs,
        success: outcome.satisfied,
        http_sample: None,
    }
}

// ---------------------------------------------------------------------------
// std/ws-ping@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   id       – Connection ID (required)
//   timeout  – ms to wait for the pong, default 10000
//
// Output:
//   { "pong": <bool>, "duration_ms": <f64> }
//
// `duration_ms` is the transport-level ping→pong round trip. Deliberately
// not aggregated into any histogram — application latency is `ws_msg_rtt`'s
// job; assert an upper bound with `check: { duration_ms_lt: … }` if needed.
// Data messages arriving while waiting are buffered for the next ws-recv.

pub(crate) async fn ws_ping_action(params: &Value, ctx: &Context, step_name: &str) -> ActionOutput {
    let (id, mut conn) = match take_conn(params, ctx) {
        Ok(x) => x,
        Err(msg) => return err(step_name, &msg),
    };
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);

    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(timeout_ms);

    let result: Result<(), String> = async {
        conn.stream
            .send(Message::Ping(Vec::from(&b"perfscale"[..]).into()))
            .await
            .map_err(|e| error_chain(&e))?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("pong TIMEOUT".into());
            }
            match tokio::time::timeout(remaining, conn.stream.next()).await {
                Err(_) => return Err("pong TIMEOUT".into()),
                Ok(None) => return Err("connection closed while waiting for pong".into()),
                Ok(Some(Err(e))) => return Err(error_chain(&e)),
                Ok(Some(Ok(Message::Pong(_)))) => return Ok(()),
                Ok(Some(Ok(Message::Close(_)))) => {
                    return Err("connection closed while waiting for pong".into())
                }
                Ok(Some(Ok(frame))) => {
                    // Data arriving mid-ping belongs to the next ws-recv.
                    if let Some(v) = frame_to_value(&frame) {
                        conn.pending.push_back(v);
                    }
                }
            }
        }
    }
    .await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    match result {
        Ok(()) => {
            let url = conn.url.clone();
            ctx.resources.put_back(&id, conn);
            ActionOutput {
                value: json!({ "pong": true, "duration_ms": duration_ms }),
                logs: vec![(
                    LogTag::Out,
                    format!("WS {url} [{id}] ping → pong ({duration_ms:.2}ms)"),
                )],
                success: true,
                http_sample: None,
            }
        }
        Err(msg) => err(step_name, &format!("WS ping [{id}]: {msg}")),
    }
}

// ---------------------------------------------------------------------------
// std/ws-close@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   id       – Connection ID (required)
//   code     – close code, default 1000 (normal closure)
//   reason   – close reason string, default ""
//   timeout  – ms for the close handshake, default 10000
//
// Output:
//   { "closed": true, "duration_ms": <f64> }
//
// Sends a Close frame and drains until the peer acknowledges (or the timeout
// passes — still reported as closed; the socket is gone either way).

pub(crate) async fn ws_close_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    let (id, mut conn) = match take_conn(params, ctx) {
        Ok(x) => x,
        Err(msg) => return err(step_name, &msg),
    };
    let code = params
        .get("code")
        .map(|v| u64_param(v, 1000))
        .unwrap_or(1000) as u16;
    let reason = params
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);

    let t0 = Instant::now();
    let _ = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let _ = conn
            .stream
            .close(Some(CloseFrame {
                code: CloseCode::from(code),
                reason: reason.into(),
            }))
            .await;
        // Drain until the peer's Close (or EOF) so the handshake completes.
        while let Some(Ok(frame)) = conn.stream.next().await {
            if matches!(frame, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let url = conn.url.clone();
    drop(conn);
    ActionOutput {
        value: json!({ "closed": true, "duration_ms": duration_ms }),
        logs: vec![(
            LogTag::Out,
            format!("WS {url} [{id}] closed ({duration_ms:.2}ms)"),
        )],
        success: true,
        http_sample: None,
    }
}

// ---------------------------------------------------------------------------
// std/ws@v1 — one-shot session
// ---------------------------------------------------------------------------
//
// Connect, exchange messages, close — connection and traffic in one step,
// symmetric with `pro/fix@v1`. Profile parameters are the same as
// std/ws-connect@v1; additionally:
//
//   messages – list of entries to send in order. Each entry is a string (a
//              `${…}` template) or an object:
//                { send | send_base64, repeat, interval_ms,
//                  until_contains | until_json }
//              An until rule makes the entry wait for its reply (and yields
//              one `ws_msg_rtt` sample); entries without one just send.
//   timeout  – ms for the WHOLE session (connect → messages → close),
//              default 10000
//
// Output:
//   { "connected": true, "sent": <u64>, "received": <u64>,
//     "messages": [...], "body": <joined>, "subprotocol": <string|null>,
//     "duration_ms": <f64>,
//     "metrics": { "ws_msgs_sent": …, "ws_msgs_received": …,
//                  "ws_msg_rtt": [...]? } }
//
// The whole session feeds `http_req_duration`, like a FIX session. The step
// fails on handshake errors, transport errors, or any entry whose until rule
// did not match in time.

pub(crate) async fn ws_session_action(params: &Value, step_name: &str) -> ActionOutput {
    let profile = match resolve_profile(params) {
        Ok(p) => p,
        Err(msg) => return err(step_name, &msg),
    };
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);
    let entries = match params.get("messages") {
        None => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return err(step_name, "'messages' must be an array"),
    };

    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(timeout_ms);

    let session = run_session(&profile, &entries, deadline).await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    match session {
        Ok(s) => {
            let mut metrics = json!({
                "ws_msgs_sent": s.sent,
                "ws_msgs_received": s.received.len() as u64,
            });
            if !s.rtts.is_empty() {
                metrics["ws_msg_rtt"] = json!(s.rtts);
            }
            let ok = s.error.is_none();
            let mut value = json!({
                "connected": true,
                "sent": s.sent,
                "received": s.received.len() as u64,
                "messages": s.received,
                "body": joined_body(&s.received),
                "subprotocol": s.subprotocol,
                "duration_ms": duration_ms,
                "metrics": metrics,
            });
            let mut logs = vec![(
                if ok { LogTag::Out } else { LogTag::Err },
                format!(
                    "WS {} → sent {}, recv {} ({duration_ms:.2}ms)",
                    profile.url,
                    s.sent,
                    s.received.len()
                ),
            )];
            if let Some(e) = &s.error {
                value["error"] = json!(e);
                logs.push((LogTag::Err, format!("{step_name}: WS session: {e}")));
            }
            ActionOutput {
                value,
                logs,
                success: ok,
                http_sample: Some(HttpSample {
                    duration_ms,
                    status: 0,
                    failed: !ok,
                }),
            }
        }
        Err(msg) => ws_err(step_name, &profile.url, &msg, duration_ms),
    }
}

struct SessionOutcome {
    subprotocol: Option<String>,
    sent: u64,
    received: Vec<Value>,
    rtts: Vec<f64>,
    /// First per-entry failure (unmatched until, transport error) — the
    /// session still reports everything exchanged up to that point.
    error: Option<String>,
}

/// Handshake errors are `Err` (nothing exchanged); anything after that is a
/// `SessionOutcome`, possibly with `error` set.
async fn run_session(
    profile: &Profile,
    entries: &[Value],
    deadline: Instant,
) -> Result<SessionOutcome, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let (mut stream, subprotocol) = tokio::time::timeout(remaining, ws_handshake(profile))
        .await
        .map_err(|_| "handshake TIMEOUT".to_string())??;

    let mut generator = Gen::new(uuid::Uuid::new_v4().as_u128() as u64);
    let mut outcome = SessionOutcome {
        subprotocol,
        sent: 0,
        received: Vec::new(),
        rtts: Vec::new(),
        error: None,
    };
    let mut pending = std::collections::VecDeque::new();

    for (i, entry) in entries.iter().enumerate() {
        // String entry == { send: <string> }.
        let entry_obj = match entry {
            Value::String(s) => json!({ "send": s }),
            Value::Object(_) => entry.clone(),
            _ => {
                outcome.error = Some(format!("message[{i}] must be a string or an object"));
                break;
            }
        };

        let payload = match Payload::from_params(&entry_obj) {
            Ok(p) => p,
            Err(msg) => {
                outcome.error = Some(format!("message[{i}]: {msg}"));
                break;
            }
        };
        let repeat = entry_obj
            .get("repeat")
            .map(|v| u64_param(v, 1))
            .unwrap_or(1);
        let interval_ms = entry_obj
            .get("interval_ms")
            .map(|v| u64_param(v, 0))
            .unwrap_or(0);

        match send_repeated(
            &mut stream,
            &mut generator,
            &payload,
            repeat,
            interval_ms,
            deadline,
        )
        .await
        {
            Ok((sent, _)) => outcome.sent += sent,
            Err(msg) => {
                outcome.error = Some(format!("message[{i}]: {msg}"));
                break;
            }
        }

        // An until rule makes this entry wait for its reply.
        let has_until =
            entry_obj.get("until_contains").is_some() || entry_obj.get("until_json").is_some();
        if has_until {
            let until = match Until::from_params(&entry_obj) {
                Ok(u) => u,
                Err(msg) => {
                    outcome.error = Some(format!("message[{i}]: {msg}"));
                    break;
                }
            };
            let sent_at = Some(Instant::now());
            let read = read_until(&mut stream, &mut pending, &until, deadline, sent_at).await;
            outcome.received.extend(read.messages);
            if let Some(rtt) = read.rtt_ms {
                outcome.rtts.push(rtt);
            }
            if !read.satisfied {
                outcome.error = Some(match &read.error {
                    Some(e) => format!("message[{i}]: {e}"),
                    None if read.closed => {
                        format!("message[{i}]: connection closed before the reply matched")
                    }
                    None => format!("message[{i}]: timeout before the reply matched"),
                });
                break;
            }
        }
    }

    // Graceful close, best effort within whatever time is left.
    let remaining = deadline.saturating_duration_since(Instant::now());
    let _ = tokio::time::timeout(remaining.max(Duration::from_millis(50)), async {
        let _ = stream.close(None).await;
        while let Some(Ok(frame)) = stream.next().await {
            if matches!(frame, Message::Close(_)) {
                break;
            }
        }
    })
    .await;

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::actions::execute_action;

    // -----------------------------------------------------------------
    // Test server: accepts WebSocket connections and echoes text frames.
    // `prefix` lets tests distinguish echo replies from what they sent.
    // -----------------------------------------------------------------
    async fn echo_server(prefix: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                    while let Some(Ok(msg)) = ws.next().await {
                        match msg {
                            Message::Text(t) => {
                                let reply = format!("{prefix}{t}");
                                if ws.send(Message::Text(reply.into())).await.is_err() {
                                    break;
                                }
                            }
                            Message::Binary(b) => {
                                if ws.send(Message::Binary(b)).await.is_err() {
                                    break;
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });
        format!("ws://{addr}")
    }

    async fn connect_id(ctx: &Context, url: &str) -> String {
        let out = execute_action("std/ws-connect@v1", &json!({ "url": url }), ctx, "connect").await;
        assert!(out.success, "{:?}", out.logs);
        out.value["id"].as_str().unwrap().to_string()
    }

    // -----------------------------------------------------------------
    // Profile resolution
    // -----------------------------------------------------------------

    #[test]
    fn profile_requires_ws_url() {
        assert!(resolve_profile(&json!({})).is_err());
        assert!(resolve_profile(&json!({ "url": "http://x" })).is_err());
        assert!(resolve_profile(&json!({ "url": "ws://x" })).is_ok());
        assert!(resolve_profile(&json!({ "url": "wss://x" })).is_ok());
    }

    #[test]
    fn profile_inline_overrides_connection_object() {
        let p = resolve_profile(&json!({
            "connection": { "url": "ws://from-profile", "skipTLSVerify": true },
            "url": "ws://inline",
        }))
        .unwrap();
        assert_eq!(p.url, "ws://inline");
        assert!(
            p.skip_tls_verify,
            "profile field survives when not overridden"
        );
    }

    #[test]
    fn profile_accepts_json_string_from_interpolation() {
        let s = r#"{"url":"ws://cfg","headers":{"Authorization":"Bearer t"},"subprotocols":["graphql-ws"]}"#;
        let p = resolve_profile(&json!({ "connection": s })).unwrap();
        assert_eq!(p.url, "ws://cfg");
        assert_eq!(p.headers, vec![("Authorization".into(), "Bearer t".into())]);
        assert_eq!(p.subprotocols, vec!["graphql-ws"]);
    }

    #[test]
    fn profile_rejects_garbage_connection() {
        assert!(resolve_profile(&json!({ "connection": "not json" })).is_err());
        assert!(resolve_profile(&json!({ "connection": 42 })).is_err());
    }

    // -----------------------------------------------------------------
    // JSON subset matching
    // -----------------------------------------------------------------

    #[test]
    fn json_subset_ignores_extra_fields_and_recurses() {
        let actual = json!({ "type": "trade", "px": 1.1, "meta": { "seq": 5, "x": 1 } });
        assert!(json_subset_match(&json!({ "type": "trade" }), &actual));
        assert!(json_subset_match(&json!({ "meta": { "seq": 5 } }), &actual));
        assert!(!json_subset_match(&json!({ "type": "quote" }), &actual));
        assert!(!json_subset_match(&json!({ "missing": 1 }), &actual));
    }

    // -----------------------------------------------------------------
    // Live Connection lifecycle
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn connect_send_recv_roundtrip() {
        let url = echo_server("echo:").await;
        let ctx = Context::new();
        let id = connect_id(&ctx, &url).await;

        let out = execute_action(
            "std/ws-send@v1",
            &json!({ "id": id, "send": "hello" }),
            &ctx,
            "send",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["sent"], 1);

        let out = execute_action(
            "std/ws-recv@v1",
            &json!({ "id": id, "until_contains": "echo:hello" }),
            &ctx,
            "recv",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["matched"], true);
        assert_eq!(out.value["messages"][0], "echo:hello");
        assert!(out.value["body"].as_str().unwrap().contains("echo:hello"));
        // send→match RTT was measured and reported for aggregation.
        assert!(out.value["metrics"]["ws_msg_rtt"][0].as_f64().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn connect_reports_sample_and_id() {
        let url = echo_server("").await;
        let ctx = Context::new();
        let out = execute_action("std/ws-connect@v1", &json!({ "url": url }), &ctx, "c").await;
        assert!(out.success);
        assert_eq!(out.value["connected"], true);
        assert_eq!(out.value["id"], "ws-1");
        let sample = out.http_sample.expect("handshake feeds the histogram");
        assert!(!sample.failed);
    }

    #[tokio::test]
    async fn recv_count_mode_reads_n_messages() {
        let url = echo_server("e:").await;
        let ctx = Context::new();
        let id = connect_id(&ctx, &url).await;

        for _ in 0..3 {
            let out = execute_action(
                "std/ws-send@v1",
                &json!({ "id": id, "send": "x" }),
                &ctx,
                "send",
            )
            .await;
            assert!(out.success);
        }
        let out = execute_action(
            "std/ws-recv@v1",
            &json!({ "id": id, "count": 3 }),
            &ctx,
            "recv",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["count"], 3);
        // Plain count mode has no send→match anchor — no RTT sample.
        assert!(out.value["metrics"].get("ws_msg_rtt").is_none());
    }

    #[tokio::test]
    async fn recv_until_json_subset_matches() {
        let url = echo_server("").await;
        let ctx = Context::new();
        let id = connect_id(&ctx, &url).await;

        let out = execute_action(
            "std/ws-send@v1",
            &json!({ "id": id, "send": r#"{"type":"trade","px":1.5,"extra":true}"# }),
            &ctx,
            "send",
        )
        .await;
        assert!(out.success);

        let out = execute_action(
            "std/ws-recv@v1",
            &json!({ "id": id, "until_json": { "type": "trade" } }),
            &ctx,
            "recv",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["matched"], true);
    }

    #[tokio::test]
    async fn recv_timeout_without_match_fails_step() {
        let url = echo_server("").await;
        let ctx = Context::new();
        let id = connect_id(&ctx, &url).await;

        let out = execute_action(
            "std/ws-recv@v1",
            &json!({ "id": id, "until_contains": "never", "timeout": 100 }),
            &ctx,
            "recv",
        )
        .await;
        assert!(!out.success);
        assert_eq!(out.value["matched"], false);
        assert!(
            out.logs
                .iter()
                .any(|(t, l)| *t == LogTag::Err && l.contains("timeout")),
            "{:?}",
            out.logs
        );
        // Connection survives a timeout — only closed/errored sockets drop.
        assert!(ctx.resources.take(&id).is_some());
    }

    #[tokio::test]
    async fn send_generator_expands_per_send() {
        let url = echo_server("").await;
        let ctx = Context::new();
        let id = connect_id(&ctx, &url).await;

        let out = execute_action(
            "std/ws-send@v1",
            &json!({ "id": id, "send": "ord-${seq}", "repeat": 2 }),
            &ctx,
            "send",
        )
        .await;
        assert!(out.success);
        assert_eq!(out.value["sent"], 2);

        let out = execute_action(
            "std/ws-recv@v1",
            &json!({ "id": id, "count": 2 }),
            &ctx,
            "recv",
        )
        .await;
        assert_eq!(out.value["messages"][0], "ord-1");
        assert_eq!(out.value["messages"][1], "ord-2", "seq bumps per send");
    }

    #[tokio::test]
    async fn binary_roundtrip_uses_base64() {
        let url = echo_server("").await;
        let ctx = Context::new();
        let id = connect_id(&ctx, &url).await;

        // [0xDE, 0xAD, 0xBE, 0xEF]
        let out = execute_action(
            "std/ws-send@v1",
            &json!({ "id": id, "send_base64": "3q2+7w==" }),
            &ctx,
            "send",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);

        let out = execute_action(
            "std/ws-recv@v1",
            &json!({ "id": id, "count": 1 }),
            &ctx,
            "recv",
        )
        .await;
        assert!(out.success);
        assert_eq!(out.value["messages"][0], "3q2+7w==");
    }

    #[tokio::test]
    async fn ping_measures_rtt_and_buffers_data() {
        let url = echo_server("e:").await;
        let ctx = Context::new();
        let id = connect_id(&ctx, &url).await;

        // Queue an echo the ping loop will encounter before the pong.
        let out = execute_action(
            "std/ws-send@v1",
            &json!({ "id": id, "send": "boo" }),
            &ctx,
            "send",
        )
        .await;
        assert!(out.success);

        let out = execute_action("std/ws-ping@v1", &json!({ "id": id }), &ctx, "ping").await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["pong"], true);
        assert!(out.value["duration_ms"].as_f64().unwrap() >= 0.0);
        assert!(
            out.http_sample.is_none(),
            "ping RTT is not histogram fodder"
        );

        // The data message the ping consumed is not lost.
        let out = execute_action(
            "std/ws-recv@v1",
            &json!({ "id": id, "until_contains": "e:boo", "timeout": 2000 }),
            &ctx,
            "recv",
        )
        .await;
        assert!(
            out.success,
            "ping must buffer data messages: {:?}",
            out.logs
        );
    }

    #[tokio::test]
    async fn close_removes_connection() {
        let url = echo_server("").await;
        let ctx = Context::new();
        let id = connect_id(&ctx, &url).await;

        let out = execute_action("std/ws-close@v1", &json!({ "id": id }), &ctx, "close").await;
        assert!(out.success);
        assert_eq!(out.value["closed"], true);

        // The id is gone — a second use gets a clear error.
        let out = execute_action(
            "std/ws-send@v1",
            &json!({ "id": id, "send": "x" }),
            &ctx,
            "s",
        )
        .await;
        assert!(!out.success);
        assert!(
            out.logs[0].1.contains("unknown connection id"),
            "{:?}",
            out.logs
        );
    }

    #[tokio::test]
    async fn unknown_and_missing_id_fail_clearly() {
        let ctx = Context::new();
        for params in [json!({}), json!({ "id": "ws-99" })] {
            let out = execute_action("std/ws-recv@v1", &params, &ctx, "recv").await;
            assert!(!out.success);
        }
    }

    #[tokio::test]
    async fn connect_refused_is_failed_sample() {
        let ctx = Context::new();
        let out = execute_action(
            "std/ws-connect@v1",
            &json!({ "url": "ws://127.0.0.1:1/", "timeout": 2000 }),
            &ctx,
            "c",
        )
        .await;
        assert!(!out.success);
        assert!(out.http_sample.unwrap().failed);
    }

    #[tokio::test]
    async fn mutually_exclusive_params_are_rejected() {
        let url = echo_server("").await;
        let ctx = Context::new();
        let id = connect_id(&ctx, &url).await;

        let out = execute_action(
            "std/ws-send@v1",
            &json!({ "id": id, "send": "a", "send_base64": "YQ==" }),
            &ctx,
            "send",
        )
        .await;
        assert!(!out.success);
        // The connection survives a parameter error.
        let out = execute_action(
            "std/ws-recv@v1",
            &json!({ "id": id, "until_contains": "x", "until_json": {}, "timeout": 100 }),
            &ctx,
            "recv",
        )
        .await;
        assert!(!out.success);
        assert!(ctx.resources.take(&id).is_some());
    }

    // -----------------------------------------------------------------
    // One-shot session
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn session_exchanges_and_times_whole_step() {
        let url = echo_server("re:").await;
        let ctx = Context::new();
        let out = execute_action(
            "std/ws@v1",
            &json!({
                "url": url,
                "messages": [
                    { "send": "sub-${seq}", "until_contains": "re:sub-1" },
                    "fire-and-forget",
                ],
            }),
            &ctx,
            "session",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["sent"], 2);
        assert_eq!(out.value["messages"][0], "re:sub-1");
        assert_eq!(out.value["metrics"]["ws_msgs_sent"], 2);
        assert_eq!(
            out.value["metrics"]["ws_msg_rtt"].as_array().unwrap().len(),
            1
        );
        let sample = out.http_sample.expect("session feeds the histogram");
        assert!(!sample.failed);
    }

    #[tokio::test]
    async fn session_unmatched_until_fails_but_reports_exchange() {
        let url = echo_server("re:").await;
        let ctx = Context::new();
        let out = execute_action(
            "std/ws@v1",
            &json!({
                "url": url,
                "timeout": 500,
                "messages": [ { "send": "x", "until_contains": "never" } ],
            }),
            &ctx,
            "session",
        )
        .await;
        assert!(!out.success);
        assert!(out.http_sample.unwrap().failed);
        assert_eq!(
            out.value["sent"], 1,
            "exchange up to the failure is reported"
        );
        assert!(out.value["error"].as_str().unwrap().contains("timeout"));
    }

    #[tokio::test]
    async fn session_connect_failure_is_failed_sample() {
        let ctx = Context::new();
        let out = execute_action(
            "std/ws@v1",
            &json!({ "url": "ws://127.0.0.1:1/", "timeout": 2000 }),
            &ctx,
            "session",
        )
        .await;
        assert!(!out.success);
        assert!(out.http_sample.unwrap().failed);
    }

    // -----------------------------------------------------------------
    // Subprotocol negotiation
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn subprotocol_is_negotiated_and_reported() {
        // A server that accepts the first offered subprotocol.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
            let cb = |req: &Request, mut resp: Response| {
                let offered = req
                    .headers()
                    .get("sec-websocket-protocol")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.split(',').next())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                resp.headers_mut()
                    .insert("sec-websocket-protocol", offered.parse().unwrap());
                Ok(resp)
            };
            let mut ws = tokio_tungstenite::accept_hdr_async(tcp, cb).await.unwrap();
            while let Some(Ok(m)) = ws.next().await {
                if matches!(m, Message::Close(_)) {
                    break;
                }
            }
        });

        let ctx = Context::new();
        let out = execute_action(
            "std/ws-connect@v1",
            &json!({ "url": format!("ws://{addr}"), "subprotocols": ["graphql-ws", "other"] }),
            &ctx,
            "c",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["subprotocol"], "graphql-ws");
    }

    // -----------------------------------------------------------------
    // TLS (wss://) — self-signed server, the skipTLSVerify path
    // -----------------------------------------------------------------

    /// A one-connection `wss://` echo server with a fresh self-signed cert.
    async fn tls_echo_server() -> String {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // A rejected handshake (the no-skip test) just ends here.
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else {
                        return;
                    };
                    while let Some(Ok(msg)) = ws.next().await {
                        match msg {
                            Message::Text(t) => {
                                if ws.send(Message::Text(t)).await.is_err() {
                                    break;
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });
        format!("wss://localhost:{}", addr.port())
    }

    #[tokio::test]
    async fn wss_with_skip_tls_verify_connects_and_echoes() {
        let url = tls_echo_server().await;
        let ctx = Context::new();
        let out = execute_action(
            "std/ws@v1",
            &json!({
                "url": url,
                "skipTLSVerify": true,
                "messages": [ { "send": "tls-ping", "until_contains": "tls-ping" } ],
            }),
            &ctx,
            "session",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["messages"][0], "tls-ping");
    }

    /// Without skipTLSVerify the self-signed chain must be rejected — the
    /// dangerous flag is opt-in, never the default.
    #[tokio::test]
    async fn wss_self_signed_is_rejected_by_default() {
        let url = tls_echo_server().await;
        let ctx = Context::new();
        let out = execute_action(
            "std/ws-connect@v1",
            &json!({ "url": url, "timeout": 5000 }),
            &ctx,
            "c",
        )
        .await;
        assert!(!out.success);
        assert!(out.http_sample.unwrap().failed);
    }

    /// `skipTLSVerify` travels inside a Connection Profile too — the
    /// `${{ config.x }}` JSON-string path.
    #[tokio::test]
    async fn wss_profile_string_carries_tls_flag() {
        let url = tls_echo_server().await;
        let profile = format!(r#"{{"url":"{url}","skipTLSVerify":true}}"#);
        let ctx = Context::new();
        let out = execute_action(
            "std/ws-connect@v1",
            &json!({ "connection": profile }),
            &ctx,
            "c",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["connected"], true);
    }
}
