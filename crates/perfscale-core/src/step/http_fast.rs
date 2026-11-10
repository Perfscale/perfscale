//! Native HTTP/1.1 fast path for `std/http@v1`.
//!
//! reqwest/hyper cost ~60% of the engine's user CPU per request on a GET
//! loop: the request is handed to a separate hyper connection task over a
//! channel, passes the tower retry / redirect / decompression / timeout
//! layers, and the URI is re-stringified and re-parsed on the way. Pure load
//! generators (zrk, gcannon) do none of that — one connection, one loop,
//! bytes in and out.
//!
//! This module is that loop for `http://` and `https://` (rustls, ring,
//! webpki roots, ALPN `http/1.1` — reqwest's TLS stack) requests on the default
//! (`per-vu`, sharded) pool: keep-alive connections live in a per-shard idle
//! pool — the same sharing as the shard's `reqwest::Client`, and with the
//! thread-per-core executor one shard is one thread, so the pool lock is
//! uncontended — the VU writes the request as one pre-assembled buffer and
//! parses the response with `httparse` in its own task. It reproduces what the reqwest path puts on the wire and
//! hands back the same [`HttpOutcome`]:
//!
//! - headers: the step's headers in order, then `content-type` for a body,
//!   `accept: */*` and `accept-encoding: gzip` unless set, `host`, and a
//!   `content-length` for bodies (and `0` for an empty POST/PUT/PATCH);
//! - gzip responses are decompressed and lose `content-encoding` /
//!   `content-length` (reqwest's `gzip` feature does the same);
//! - `Content-Length`, `chunked` and read-to-EOF bodies; 1xx interim
//!   responses are skipped; `HEAD`/204/304 carry no body;
//! - the step timeout covers connect → last body byte;
//! - a reused keep-alive connection that the server closed while idle is
//!   retried once on a fresh connection when not a byte of the response
//!   arrived (hyper's behaviour for idle-closed connections);
//! - a 3xx with `Location` is reported to the caller, which continues the
//!   redirect chain through reqwest (method rewriting, the 10-hop cap,
//!   cross-host header stripping all stay reqwest's).
//!
//! Everything else — `multipart`, `source_ips`, credentials in the URL, the
//! shared/dedicated pools — stays on reqwest (see [`eligible`]). Verified
//! and `insecure` connections never share a pool entry. `PERFSCALE_HTTP_CLIENT=
//! reqwest` disables the fast path entirely.

use std::io::Read as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::http::{BodyKind, HttpOutcome};

/// Response header slots for `httparse` — responses with more headers fail
/// the step like a malformed response would.
const MAX_HEADERS: usize = 96;
/// Read granularity.
const READ_CHUNK: usize = 16 * 1024;
/// Idle connections kept per (shard, host:port) — reqwest's
/// `pool_max_idle_per_host` for the shard clients.
const MAX_IDLE_PER_HOST: usize = 64;
/// Idle connections older than this are closed instead of reused —
/// reqwest's `pool_idle_timeout`.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// A connection's transport: plain TCP or TLS over TCP.
enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Stream::Plain(_) => "Stream::Plain",
            Stream::Tls(_) => "Stream::Tls",
        })
    }
}

impl tokio::io::AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// rustls client configs (ring, ALPN `http/1.1`): `[verified, insecure]`.
/// Verified uses the webpki roots — reqwest's `rustls-tls` trust store.
fn tls_config(insecure: bool) -> Result<Arc<rustls::ClientConfig>, FastError> {
    static CONFIGS: OnceLock<Result<[Arc<rustls::ClientConfig>; 2], String>> = OnceLock::new();
    let configs = CONFIGS.get_or_init(|| {
        let alpn = |mut c: rustls::ClientConfig| {
            c.alpn_protocols = vec![b"http/1.1".to_vec()];
            Arc::new(c)
        };
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let verified = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let insecure = super::ws::no_verify_tls_config()?;
        Ok([alpn(verified), alpn(insecure)])
    });
    match configs {
        Ok(c) => Ok(Arc::clone(&c[insecure as usize])),
        Err(e) => Err(FastError::Io(format!("tls config error: {e}"))),
    }
}

/// One keep-alive connection owned by a VU.
#[derive(Debug)]
pub(crate) struct Conn {
    key: String,
    stream: Stream,
    /// Bytes read past the previous response (never expected without
    /// pipelining, kept for correctness).
    buf: Vec<u8>,
    idle_since: Instant,
}

/// Idle fast-path connections of one HTTP client shard.
pub(crate) type ConnPool = Arc<Mutex<Vec<Conn>>>;

/// The idle pool of client shard `shard` (see `http::client_shard_count`).
pub(crate) fn shard_pool(shard: usize) -> &'static ConnPool {
    static POOLS: OnceLock<Vec<ConnPool>> = OnceLock::new();
    let pools = POOLS.get_or_init(|| {
        (0..super::actions::client_shard_count())
            .map(|_| ConnPool::default())
            .collect()
    });
    &pools[shard % pools.len()]
}

/// `false` when `PERFSCALE_HTTP_CLIENT=reqwest`.
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("PERFSCALE_HTTP_CLIENT").as_deref() != Ok("reqwest"))
}

/// The fast path handles `http://` and `https://` URLs without credentials.
pub(crate) fn eligible(url: &reqwest::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some()
}

/// A request ready for the wire.
pub(crate) struct Request<'a> {
    pub method: &'a str,
    pub url: &'a reqwest::Url,
    /// The step's headers, in order (already expanded).
    pub headers: &'a [(String, String)],
    /// `(content-type, body)`.
    pub body: Option<(&'static str, &'a [u8])>,
    pub timeout: Duration,
    /// Skip TLS certificate verification (`insecure: true`).
    pub insecure: bool,
}

/// Fast-path failure, mapped by the caller like a reqwest transport error.
#[derive(Debug)]
pub enum FastError {
    /// I/O or protocol failure (`detail` is the full cause chain).
    Io(String),
    Timeout,
    BodyTooLarge {
        limit: u64,
        status: u16,
    },
}

/// What a fast exchange produced.
pub(crate) enum Exchange {
    Done(HttpOutcome),
    /// 3xx with a `Location`: the caller continues the chain via reqwest.
    Redirect {
        status: u16,
        location: String,
    },
}

/// A request assembled for the wire, reusable as-is by a static step (see
/// [`cached`]).
#[derive(Debug)]
pub(crate) struct Prepared {
    key: String,
    host: String,
    port: u16,
    /// `Some(insecure)` for https.
    tls: Option<bool>,
    wire: Vec<u8>,
    head: bool,
    pub timeout: Duration,
    /// For the request log line.
    pub method: String,
    pub url: String,
}

/// Assemble `req` (host key, wire bytes) once.
pub(crate) fn prepare(req: &Request<'_>) -> Prepared {
    let host = req.url.host_str().unwrap_or_default();
    let port = req.url.port_or_known_default().unwrap_or(80);
    let tls = (req.url.scheme() == "https").then_some(req.insecure);
    let mode = match tls {
        None => "http",
        Some(false) => "https",
        Some(true) => "https+insecure",
    };
    Prepared {
        key: format!("{mode}://{host}:{port}"),
        host: host.to_owned(),
        port,
        tls,
        wire: encode(req, host, req.url.port()),
        head: req.method == "HEAD",
        timeout: req.timeout,
        method: req.method.to_owned(),
        url: req.url.as_str().to_owned(),
    }
}

/// Prepared requests of static steps on this thread, tagged with the run
/// epoch (a step's address is only stable within one run).
type PreparedCache = (u64, std::collections::HashMap<usize, Arc<Prepared>>);

thread_local! {
    static PREPARED: std::cell::RefCell<PreparedCache> =
        std::cell::RefCell::new((0, std::collections::HashMap::new()));
}

/// The cached request of static step `step_key` in run `epoch`.
pub(crate) fn cached(epoch: u64, step_key: usize) -> Option<Arc<Prepared>> {
    PREPARED.with(|c| {
        let c = c.borrow();
        if c.0 == epoch {
            c.1.get(&step_key).cloned()
        } else {
            None
        }
    })
}

/// Remember a static step's request for the rest of run `epoch`.
pub(crate) fn remember(epoch: u64, step_key: usize, p: Arc<Prepared>) {
    PREPARED.with(|c| {
        let mut c = c.borrow_mut();
        if c.0 != epoch {
            *c = (epoch, std::collections::HashMap::new());
        }
        c.1.insert(step_key, p);
    });
}

/// How the response is read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadOpts {
    pub max_body_bytes: u64,
    pub body_kind: BodyKind,
}

/// Run one exchange on a pooled (or new) connection of `pool`.
pub(crate) async fn exchange(
    pool: &ConnPool,
    p: &Prepared,
    opts: ReadOpts,
) -> (f64, Result<Exchange, FastError>) {
    let t0 = Instant::now();
    let res = match tokio::time::timeout(p.timeout, exchange_inner(pool, p, opts)).await {
        Ok(r) => r,
        Err(_) => Err(FastError::Timeout),
    };
    (t0.elapsed().as_secs_f64() * 1000.0, res)
}

async fn exchange_inner(
    pool: &ConnPool,
    p: &Prepared,
    opts: ReadOpts,
) -> Result<Exchange, FastError> {
    let (key, host, port, wire) = (p.key.as_str(), p.host.as_str(), p.port, &p.wire);
    let tls = p.tls;

    // A pooled connection may have been closed by the server while idle;
    // that surfaces as a write error or EOF before the first response byte.
    // Retry exactly once, on a fresh connection.
    let pooled = take(pool, key);
    let reused = pooled.is_some();
    let mut conn = match pooled {
        Some(c) => c,
        None => connect(key, host, port, tls).await?,
    };
    let mut attempt = round_trip(&mut conn, wire, p.head, opts).await;
    if reused && matches!(attempt, Err(RoundTripError::Stale(_))) {
        conn = connect(key, host, port, tls).await?;
        attempt = round_trip(&mut conn, wire, p.head, opts).await;
    }
    match attempt {
        Ok((ex, keep_alive)) => {
            if keep_alive {
                give_back(pool, conn);
            }
            Ok(ex)
        }
        Err(RoundTripError::Stale(e)) | Err(RoundTripError::Fatal(e)) => Err(e),
    }
}

fn take(pool: &ConnPool, key: &str) -> Option<Conn> {
    let mut idle = pool.lock().unwrap();
    // Most recently returned first (LIFO keeps a hot set of sockets);
    // expired ones are dropped on the way.
    while let Some(i) = idle.iter().rposition(|c| c.key == key) {
        let conn = idle.swap_remove(i);
        if conn.idle_since.elapsed() < IDLE_TIMEOUT {
            return Some(conn);
        }
    }
    None
}

fn give_back(pool: &ConnPool, mut conn: Conn) {
    let mut idle = pool.lock().unwrap();
    if idle.iter().filter(|c| c.key == conn.key).count() >= MAX_IDLE_PER_HOST {
        return; // over the per-host cap: close it
    }
    conn.idle_since = Instant::now();
    idle.push(conn);
}

async fn connect(key: &str, host: &str, port: u16, tls: Option<bool>) -> Result<Conn, FastError> {
    // `host_str` brackets IPv6 literals; the resolver wants them bare.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let tcp = TcpStream::connect((bare, port))
        .await
        .map_err(|e| FastError::Io(format!("tcp connect error: {e}")))?;
    let _ = tcp.set_nodelay(true);
    let stream = match tls {
        None => Stream::Plain(tcp),
        Some(insecure) => {
            let name = rustls::pki_types::ServerName::try_from(bare.to_owned())
                .map_err(|e| FastError::Io(format!("invalid TLS server name: {e}")))?;
            let tls = tokio_rustls::TlsConnector::from(tls_config(insecure)?)
                .connect(name, tcp)
                .await
                .map_err(|e| FastError::Io(format!("tls handshake error: {e}")))?;
            Stream::Tls(Box::new(tls))
        }
    };
    Ok(Conn {
        key: key.to_owned(),
        stream,
        buf: Vec::with_capacity(READ_CHUNK),
        idle_since: Instant::now(),
    })
}

/// Assemble the request bytes (see the module docs for the header set).
pub(crate) fn encode(req: &Request<'_>, host: &str, explicit_port: Option<u16>) -> Vec<u8> {
    let body_len = req.body.map_or(0, |(_, b)| b.len());
    let mut w = Vec::with_capacity(256 + body_len);
    w.extend_from_slice(req.method.as_bytes());
    w.push(b' ');
    w.extend_from_slice(req.url.path().as_bytes());
    if let Some(q) = req.url.query() {
        w.push(b'?');
        w.extend_from_slice(q.as_bytes());
    }
    w.extend_from_slice(b" HTTP/1.1\r\n");
    let has = |name: &str| {
        req.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    };
    let mut header = |k: &str, v: &str| {
        w.extend_from_slice(k.as_bytes());
        w.extend_from_slice(b": ");
        w.extend_from_slice(v.as_bytes());
        w.extend_from_slice(b"\r\n");
    };
    for (k, v) in req.headers {
        header(k, v);
    }
    if let Some((ct, _)) = req.body {
        header("content-type", ct);
    }
    if !has("accept") {
        header("accept", "*/*");
    }
    if !has("accept-encoding") && !has("range") {
        header("accept-encoding", "gzip");
    }
    if !has("host") {
        match explicit_port {
            Some(p) => header("host", &format!("{host}:{p}")),
            None => header("host", host),
        }
    }
    if req.body.is_some() {
        header("content-length", &body_len.to_string());
    } else if matches!(req.method, "POST" | "PUT" | "PATCH") {
        header("content-length", "0");
    }
    w.extend_from_slice(b"\r\n");
    if let Some((_, b)) = req.body {
        w.extend_from_slice(b);
    }
    w
}

enum RoundTripError {
    /// Nothing of a response arrived — safe to retry on a fresh connection.
    Stale(FastError),
    Fatal(FastError),
}

/// Write `wire`, read one response. `Ok((exchange, keep_alive))`.
async fn round_trip(
    conn: &mut Conn,
    wire: &[u8],
    head: bool,
    req: ReadOpts,
) -> Result<(Exchange, bool), RoundTripError> {
    let io = |e: std::io::Error| FastError::Io(format!("connection error: {e}"));
    if let Err(e) = conn.stream.write_all(wire).await {
        return Err(RoundTripError::Stale(io(e)));
    }

    // Head (skipping 1xx interim responses).
    let (status, headers, head_len) = loop {
        let parsed = loop {
            let mut slots = [httparse::EMPTY_HEADER; MAX_HEADERS];
            let mut resp = httparse::Response::new(&mut slots);
            match resp.parse(&conn.buf) {
                Ok(httparse::Status::Complete(n)) => {
                    let status = resp.code.unwrap_or(0);
                    let headers: Vec<(String, Vec<u8>)> = resp
                        .headers
                        .iter()
                        .map(|h| (h.name.to_ascii_lowercase(), h.value.to_vec()))
                        .collect();
                    break (status, headers, n);
                }
                Ok(httparse::Status::Partial) => {}
                Err(e) => {
                    return Err(RoundTripError::Fatal(FastError::Io(format!(
                        "invalid HTTP response: {e}"
                    ))));
                }
            }
            let got_nothing = conn.buf.is_empty();
            match read_more(conn).await {
                Ok(0) | Err(_) if got_nothing => {
                    return Err(RoundTripError::Stale(FastError::Io(
                        "connection closed before message completed".into(),
                    )));
                }
                Ok(0) => {
                    return Err(RoundTripError::Fatal(FastError::Io(
                        "connection closed before message completed".into(),
                    )));
                }
                Err(e) => return Err(RoundTripError::Fatal(io(e))),
                Ok(_) => {}
            }
        };
        if (100..200).contains(&parsed.0) && parsed.0 != 101 {
            conn.buf.drain(..parsed.2);
            continue;
        }
        break parsed;
    };
    conn.buf.drain(..head_len);
    let fatal = RoundTripError::Fatal;

    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| std::str::from_utf8(v).ok())
    };
    let close = header("connection").is_some_and(|v| v.eq_ignore_ascii_case("close"));
    let chunked =
        header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let content_length = header("content-length").and_then(|v| v.trim().parse::<u64>().ok());
    let no_body = head || status == 204 || status == 304;
    let limit = req.max_body_bytes;
    if !no_body && content_length.is_some_and(|n| n > limit) {
        return Err(fatal(FastError::BodyTooLarge { limit, status }));
    }

    let keep_body = req.body_kind != BodyKind::Discard;
    let mut body = Vec::new();
    let mut eof_delimited = false;
    if !no_body {
        if chunked {
            read_chunked(conn, &mut body, keep_body, limit, status)
                .await
                .map_err(fatal)?;
        } else if let Some(n) = content_length {
            read_exact_len(conn, &mut body, n as usize, keep_body)
                .await
                .map_err(fatal)?;
        } else {
            eof_delimited = true;
            read_to_eof(conn, &mut body, keep_body, limit, status)
                .await
                .map_err(fatal)?;
        }
    }
    let keep_alive = !close && !eof_delimited;

    let location = (300..400)
        .contains(&status)
        .then(|| header("location").map(str::to_owned))
        .flatten();
    if let Some(location) = location {
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            return Ok((Exchange::Redirect { status, location }, keep_alive));
        }
    }

    let reason = reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("");
    if !keep_body {
        return Ok((
            Exchange::Done(HttpOutcome {
                status,
                reason,
                headers: serde_json::Map::new(),
                duration_ms: 0.0,
                body: String::new(),
                body_base64: None,
            }),
            keep_alive,
        ));
    }

    let gzip = header("content-encoding").is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    if gzip && !body.is_empty() {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&body[..])
            .take(limit + 1)
            .read_to_end(&mut out)
            .map_err(|e| fatal(FastError::Io(format!("error decoding response body: {e}"))))?;
        if out.len() as u64 > limit {
            return Err(fatal(FastError::BodyTooLarge { limit, status }));
        }
        body = out;
    }
    let mut map = serde_json::Map::with_capacity(headers.len());
    for (name, value) in &headers {
        if gzip && (name == "content-encoding" || name == "content-length") {
            continue;
        }
        let Ok(v) = std::str::from_utf8(value) else {
            continue;
        };
        match map.get_mut(name) {
            Some(Value::String(existing)) => {
                existing.push_str(", ");
                existing.push_str(v);
            }
            _ => {
                map.insert(name.clone(), Value::String(v.to_owned()));
            }
        }
    }
    let content_type = map
        .get("content-type")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let (text, b64) = super::http::decode_body(body, req.body_kind, content_type.as_deref());
    Ok((
        Exchange::Done(HttpOutcome {
            status,
            reason,
            headers: map,
            duration_ms: 0.0,
            body: text,
            body_base64: b64,
        }),
        keep_alive,
    ))
}

/// Read into the buffer's spare capacity — no zero-fill of a fresh chunk
/// per read.
async fn read_more(conn: &mut Conn) -> std::io::Result<usize> {
    conn.buf.reserve(READ_CHUNK);
    conn.stream.read_buf(&mut conn.buf).await
}

fn closed() -> FastError {
    FastError::Io("connection closed before message completed".into())
}

async fn read_exact_len(
    conn: &mut Conn,
    body: &mut Vec<u8>,
    mut need: usize,
    keep: bool,
) -> Result<(), FastError> {
    loop {
        let take = need.min(conn.buf.len());
        if keep {
            body.extend_from_slice(&conn.buf[..take]);
        }
        conn.buf.drain(..take);
        need -= take;
        if need == 0 {
            return Ok(());
        }
        match read_more(conn).await {
            Ok(0) => return Err(closed()),
            Ok(_) => {}
            Err(e) => return Err(FastError::Io(format!("connection error: {e}"))),
        }
    }
}

async fn read_to_eof(
    conn: &mut Conn,
    body: &mut Vec<u8>,
    keep: bool,
    limit: u64,
    status: u16,
) -> Result<(), FastError> {
    let mut seen = 0usize;
    loop {
        seen += conn.buf.len();
        if seen as u64 > limit {
            return Err(FastError::BodyTooLarge { limit, status });
        }
        if keep {
            body.extend_from_slice(&conn.buf);
        }
        conn.buf.clear();
        match read_more(conn).await {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(FastError::Io(format!("connection error: {e}"))),
        }
    }
}

/// Position of `\r\n` in the buffer, reading more until one arrives.
async fn line_end(conn: &mut Conn) -> Result<usize, FastError> {
    loop {
        if let Some(i) = conn.buf.windows(2).position(|w| w == b"\r\n") {
            return Ok(i);
        }
        match read_more(conn).await {
            Ok(0) => return Err(closed()),
            Ok(_) => {}
            Err(e) => return Err(FastError::Io(format!("connection error: {e}"))),
        }
    }
}

async fn read_chunked(
    conn: &mut Conn,
    body: &mut Vec<u8>,
    keep: bool,
    limit: u64,
    status: u16,
) -> Result<(), FastError> {
    let mut seen = 0u64;
    loop {
        let end = line_end(conn).await?;
        let size_str = std::str::from_utf8(&conn.buf[..end])
            .ok()
            .and_then(|l| l.split(';').next())
            .map(str::trim)
            .unwrap_or("");
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| FastError::Io("invalid chunk size line".into()))?;
        conn.buf.drain(..end + 2);
        if size == 0 {
            // Trailers until an empty line.
            loop {
                let end = line_end(conn).await?;
                conn.buf.drain(..end + 2);
                if end == 0 {
                    return Ok(());
                }
            }
        }
        seen += size as u64;
        if seen > limit {
            return Err(FastError::BodyTooLarge { limit, status });
        }
        read_exact_len(conn, body, size, keep).await?;
        let end = line_end(conn).await?;
        if end != 0 {
            return Err(FastError::Io("invalid chunk terminator".into()));
        }
        conn.buf.drain(..2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(
        method: &'a str,
        url: &'a reqwest::Url,
        headers: &'a [(String, String)],
        body: Option<(&'static str, &'a [u8])>,
    ) -> Request<'a> {
        Request {
            method,
            url,
            headers,
            body,
            timeout: Duration::from_secs(5),
            insecure: false,
        }
    }

    const AUTO: ReadOpts = ReadOpts {
        max_body_bytes: 1 << 20,
        body_kind: BodyKind::Auto,
    };

    #[test]
    fn encode_matches_the_reqwest_header_set() {
        let url = reqwest::Url::parse("http://example.test:8080/a/b?x=1&y=2").unwrap();
        let h = vec![("x-id".to_string(), "7".to_string())];
        let wire = encode(
            &req("POST", &url, &h, Some(("application/json", b"{}"))),
            "example.test",
            url.port(),
        );
        let text = String::from_utf8(wire).unwrap();
        assert_eq!(
            text,
            "POST /a/b?x=1&y=2 HTTP/1.1\r\nx-id: 7\r\ncontent-type: application/json\r\naccept: */*\r\naccept-encoding: gzip\r\nhost: example.test:8080\r\ncontent-length: 2\r\n\r\n{}"
        );
    }

    #[test]
    fn encode_respects_user_overrides_and_empty_posts() {
        let url = reqwest::Url::parse("http://h/").unwrap();
        let h = vec![
            ("Accept".to_string(), "text/plain".to_string()),
            ("Host".to_string(), "other".to_string()),
        ];
        let text = String::from_utf8(encode(&req("POST", &url, &h, None), "h", None)).unwrap();
        assert!(!text.contains("accept: */*"), "{text}");
        assert!(!text.contains("host: h"), "{text}");
        assert!(text.contains("content-length: 0\r\n"), "{text}");
        let get = String::from_utf8(encode(&req("GET", &url, &[], None), "h", None)).unwrap();
        assert!(!get.contains("content-length"), "{get}");
        assert!(get.contains("host: h\r\n"), "{get}");
    }

    #[test]
    fn eligibility() {
        let ok = |u: &str| eligible(&reqwest::Url::parse(u).unwrap());
        assert!(ok("http://127.0.0.1:1/x"));
        assert!(ok("https://h/"));
        assert!(!ok("ftp://h/"));
        assert!(!ok("http://user:pw@h/"));
    }

    /// Serve `responses` (raw bytes, one per accepted request) on a local
    /// port; returns the base URL.
    async fn serve(responses: Vec<&'static [u8]>, close_after_first: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut responses = responses.into_iter();
            while let Ok((mut s, _)) = listener.accept().await {
                loop {
                    let mut buf = [0u8; 4096];
                    let Ok(n) = s.read(&mut buf).await else { break };
                    if n == 0 {
                        break;
                    }
                    let Some(r) = responses.next() else { return };
                    let _ = s.write_all(r).await;
                    if close_after_first {
                        break;
                    }
                }
            }
        });
        format!("http://{addr}")
    }

    async fn run(pool: &ConnPool, url: &str, kind: BodyKind) -> HttpOutcome {
        let url = reqwest::Url::parse(url).unwrap();
        let p = prepare(&req("GET", &url, &[], None));
        let opts = ReadOpts {
            body_kind: kind,
            ..AUTO
        };
        match exchange(pool, &p, opts).await.1 {
            Ok(Exchange::Done(o)) => o,
            Ok(Exchange::Redirect { .. }) => panic!("unexpected redirect"),
            Err(e) => panic!("exchange failed: {e:?}"),
        }
    }

    #[tokio::test]
    async fn content_length_chunked_and_keep_alive_reuse() {
        let base = serve(
            vec![
                b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 5\r\n\r\nhello",
                b"HTTP/1.1 201 Created\r\ntransfer-encoding: chunked\r\nX-A: 1\r\nx-a: 2\r\n\r\n3\r\nabc\r\n2;ext\r\nde\r\n0\r\n\r\n",
            ],
            false,
        )
        .await;
        let pool = ConnPool::default();
        let a = run(&pool, &format!("{base}/a"), BodyKind::Auto).await;
        assert_eq!((a.status, a.reason, a.body.as_str()), (200, "OK", "hello"));
        assert_eq!(pool.lock().unwrap().len(), 1, "kept alive");
        let b = run(&pool, &format!("{base}/b"), BodyKind::Auto).await;
        assert_eq!((b.status, b.body.as_str()), (201, "abcde"));
        assert_eq!(b.headers["x-a"], "1, 2", "repeats joined, names lowercased");
    }

    #[tokio::test]
    async fn stale_pooled_connection_is_retried_once() {
        // The server answers one request per connection, then closes it:
        // the second request finds a dead pooled socket and must reconnect.
        let base = serve(
            vec![
                b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\na",
                b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\nb",
            ],
            true,
        )
        .await;
        let pool = ConnPool::default();
        assert_eq!(run(&pool, &base, BodyKind::Auto).await.body, "a");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(run(&pool, &base, BodyKind::Auto).await.body, "b");
    }

    #[tokio::test]
    async fn gzip_is_decoded_and_its_headers_dropped() {
        use flate2::write::GzEncoder;
        use std::io::Write as _;
        let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"{\"ok\":true}").unwrap();
        let gz = enc.finish().unwrap();
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\n\r\n",
            gz.len()
        )
        .into_bytes();
        resp.extend_from_slice(&gz);
        let base = serve(vec![Box::leak(resp.into_boxed_slice())], false).await;
        let o = run(&ConnPool::default(), &base, BodyKind::Auto).await;
        assert_eq!(o.body, "{\"ok\":true}");
        assert!(!o.headers.contains_key("content-encoding"));
        assert!(!o.headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn lean_discards_and_redirects_surface() {
        let base = serve(
            vec![
                b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\nabc",
                b"HTTP/1.1 302 Found\r\nlocation: /next\r\ncontent-length: 0\r\n\r\n",
            ],
            false,
        )
        .await;
        let pool = ConnPool::default();
        let lean = run(&pool, &base, BodyKind::Discard).await;
        assert_eq!((lean.status, lean.body.as_str()), (200, ""));
        assert!(lean.headers.is_empty());
        let url = reqwest::Url::parse(&base).unwrap();
        match exchange(&pool, &prepare(&req("GET", &url, &[], None)), AUTO)
            .await
            .1
        {
            Ok(Exchange::Redirect { status, location }) => {
                assert_eq!((status, location.as_str()), (302, "/next"));
            }
            _ => panic!("expected a redirect"),
        }
    }

    /// HTTPS over the fast path: `insecure` accepts a self-signed server and
    /// keeps the TLS connection alive across requests; the verifying config
    /// rejects it at the handshake.
    #[tokio::test]
    async fn https_insecure_reuses_and_verified_rejects_self_signed() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
            .unwrap();
        let der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
        let server_cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&accepted);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut s) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let _ = s
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                            .await;
                    }
                });
            }
        });

        let url = reqwest::Url::parse(&format!("https://localhost:{port}/")).unwrap();
        let pool = ConnPool::default();
        let mut insecure = req("GET", &url, &[], None);
        insecure.insecure = true;
        let p = prepare(&insecure);
        for _ in 0..3 {
            match exchange(&pool, &p, AUTO).await.1 {
                Ok(Exchange::Done(o)) => assert_eq!((o.status, o.body.as_str()), (200, "ok")),
                other => panic!("insecure https failed: {:?}", other.err()),
            }
        }
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one TLS connection for three requests"
        );

        let verified = prepare(&req("GET", &url, &[], None));
        match exchange(&pool, &verified, AUTO).await.1 {
            Err(FastError::Io(msg)) => assert!(msg.contains("tls handshake error"), "{msg}"),
            other => panic!("self-signed must be rejected, got ok={}", other.is_ok()),
        }
    }

    #[tokio::test]
    async fn oversized_and_refused_fail_cleanly() {
        let base = serve(
            vec![b"HTTP/1.1 200 OK\r\ncontent-length: 99999\r\n\r\n"],
            false,
        )
        .await;
        let url = reqwest::Url::parse(&base).unwrap();
        let small = ReadOpts {
            max_body_bytes: 10,
            ..AUTO
        };
        let p = prepare(&req("GET", &url, &[], None));
        assert!(matches!(
            exchange(&ConnPool::default(), &p, small).await.1,
            Err(FastError::BodyTooLarge {
                limit: 10,
                status: 200
            })
        ));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = reqwest::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        match exchange(
            &ConnPool::default(),
            &prepare(&req("GET", &url, &[], None)),
            AUTO,
        )
        .await
        .1
        {
            Err(FastError::Io(msg)) => assert!(msg.contains("tcp connect error"), "{msg}"),
            other => panic!("expected connect error, got {:?}", other.is_ok()),
        }
    }
}
