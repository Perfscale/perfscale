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

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde_json::{json, Value};
use tokio::time::Duration;

use super::actions::{confine_fs_path, err, ActionOutput, HttpSample, LogTag};
use super::context::Context;
use super::ws::{bool_param, u64_param};

// ---------------------------------------------------------------------------
// Client pools
// ---------------------------------------------------------------------------

/// Build a client with bounded idle pools. Uncapped idle pools (reqwest's
/// default) mean VUs × shards × hosts lingering sockets for the whole run;
/// under high VU counts against several hosts that is thousands of idle
/// connections the target has to keep open.
///
/// The numbers keep keep-alive useful for a load tool: 60s idle timeout —
/// any iteration cadence below a minute keeps its warm connection (a slower
/// cadence doesn't care about keep-alive anyway); 64 idle per host per
/// shard — enough for a burst within one shard, far below VU×host fan-out.
fn build_client(insecure: bool) -> reqwest::Client {
    build_client_inner(insecure, None)
}

fn build_client_inner(insecure: bool, local_address: Option<IpAddr>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(60))
        // codeql[rust/disabled-certificate-check] — deliberate feature of a
        // load-testing tool: steps opt in per-request with `insecure: true`
        // (testing targets with self-signed certs, like k6's
        // insecureSkipTLSVerify). Insecure clients live in a separate shard
        // set so verified requests never share a pool with them.
        .danger_accept_invalid_certs(insecure);
    if let Some(ip) = local_address {
        builder = builder.local_address(ip);
    }
    builder.build().expect("client construction cannot fail")
}

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
            .map(|_| build_client(false))
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
            .map(|_| build_client(true))
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
    /// VU-pinned client shard (default) — every VU keeps one warm pool in
    /// its shard's client.
    PerVu,
    /// One process-global client shared by every VU: maximal connection
    /// reuse against a single endpoint, at the cost of pool-lock contention
    /// under very high VU counts.
    Shared,
    /// One client per VU (`pool: dedicated`): the VU's keep-alive
    /// connections are never evicted by other VUs — the mode for
    /// "N VUs = N standing connections" scenarios. See
    /// [`dedicated_client`].
    Dedicated,
}

impl ClientPool {
    /// Parse the optional `pool` parameter: `per-vu` (default), `shared`, or
    /// `dedicated`.
    pub fn from_params(params: &Value) -> Result<Self, String> {
        match params["pool"].as_str().unwrap_or("per-vu") {
            "per-vu" => Ok(ClientPool::PerVu),
            "shared" => Ok(ClientPool::Shared),
            "dedicated" => Ok(ClientPool::Dedicated),
            other => Err(format!(
                "invalid pool '{other}' — expected per-vu, shared, or dedicated"
            )),
        }
    }
}

/// The HTTP client for a step execution. The insecure variants skip TLS
/// verification and never share a pool with verified requests. `Dedicated`
/// cannot be resolved here (it keys on the VU id) — callers that accept it
/// must go through [`dedicated_client`]; the defensive fallback is the
/// per-VU shard.
pub fn client(pool: ClientPool, insecure: bool, shard: usize) -> &'static reqwest::Client {
    match (pool, insecure) {
        (ClientPool::PerVu | ClientPool::Dedicated, false) => shared_client(shard),
        (ClientPool::PerVu | ClientPool::Dedicated, true) => shared_insecure_client(shard),
        (ClientPool::Shared, false) => {
            static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
            CLIENT.get_or_init(|| build_client(false))
        }
        (ClientPool::Shared, true) => {
            static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
            CLIENT.get_or_init(|| build_client(true))
        }
    }
}

// ---------------------------------------------------------------------------
// Source IPs — multi-source-IP load generation (`source_ips` parameter)
// ---------------------------------------------------------------------------
//
// One (src ip, dst ip, dst port) tuple has ~28k ephemeral source ports, so a
// single machine caps out below ~64k concurrent connections per target
// endpoint. Binding sockets to several local addresses multiplies that
// budget. `std/http@v1` accepts `source_ips: [<ip-or-cidr>, ...]`; each CIDR
// expands to every address of the prefix (capped, see [`MAX_SOURCE_IPS`]).
// VU i is pinned to `ips[(i - 1) % len]` — stable across iterations, which
// is what keeps keep-alive meaningful: a VU reuses the same (shard, source
// IP) client and its warm pool.

/// Maximum number of source addresses one step's `source_ips` may expand to.
/// Beyond this the per-step connection bookkeeping (one lazily-built client
/// per used (shard, address) pair) stops paying for itself — narrow the
/// prefix instead.
pub const MAX_SOURCE_IPS: usize = 4096;

/// Shard key used for `pool: shared` source-IP clients — pool mode `shared`
/// ignores the VU shard, so the address alone keys the client.
const SHARED_POOL_SHARD: usize = usize::MAX;

/// The source address a VU is pinned to from an expanded `source_ips` list:
/// `ips[(vu_id - 1) % len]` (vu_id is 1-based; 0 in hand-built contexts pins
/// the first address). Stable per VU so keep-alive pools stay warm.
pub fn source_ip_for_vu(ips: &[IpAddr], vu_id: u64) -> IpAddr {
    ips[(vu_id.saturating_sub(1) as usize) % ips.len()]
}

/// Expand one `source_ips` entry — a single IP (`"10.0.0.9"`, `"fd00::5"`)
/// or a CIDR prefix (`"10.0.0.0/28"`, `"fd00::/120"`) — appending every
/// address into `out`. The network/broadcast addresses of a prefix are
/// included: which addresses are actually bindable is a host question, and a
/// wrong one fails loudly at connect time. Errors name the entry.
fn expand_source_ip_entry(entry: &str, out: &mut Vec<IpAddr>) -> Result<(), String> {
    let entry = entry.trim();
    let Some((base, prefix)) = entry.split_once('/') else {
        return entry.parse::<IpAddr>().map(|ip| out.push(ip)).map_err(|_| {
            format!("invalid source_ips entry '{entry}' — expected an IP address or CIDR prefix")
        });
    };
    let base: IpAddr = base.trim().parse().map_err(|_| {
        format!("invalid source_ips entry '{entry}' — '{base}' is not an IP address")
    })?;
    let prefix: u8 = prefix.trim().parse().map_err(|_| {
        format!("invalid source_ips entry '{entry}' — the prefix length after '/' must be a number")
    })?;
    let (base_bits, width) = match base {
        IpAddr::V4(v4) => (u32::from(v4) as u128, 32),
        IpAddr::V6(v6) => (u128::from(v6), 128),
    };
    if prefix > width {
        return Err(format!(
            "invalid source_ips entry '{entry}' — prefix length /{prefix} exceeds the address size (/{width})"
        ));
    }
    let host_bits = width - prefix;
    // A /0 spans 2^128 IPv6 addresses — never materialize that; report the
    // exponent instead of shifting by the full width.
    let count = if host_bits >= 13 {
        None
    } else {
        Some(1u128 << host_bits)
    };
    let count = match count {
        Some(count) if count <= MAX_SOURCE_IPS as u128 => count,
        Some(count) => {
            return Err(format!(
                "source_ips entry '{entry}' expands to {count} addresses, over the {MAX_SOURCE_IPS} cap — narrow the prefix (e.g. /28, /120)"
            ));
        }
        None => {
            return Err(format!(
                "source_ips entry '{entry}' expands to 2^{host_bits} addresses, over the {MAX_SOURCE_IPS} cap — narrow the prefix (e.g. /28, /120)"
            ));
        }
    };
    let mask = if host_bits == 0 {
        u128::MAX
    } else {
        u128::MAX << host_bits
    };
    let network = base_bits & mask;
    for i in 0..count {
        let bits = network + i;
        match base {
            IpAddr::V4(_) => out.push(IpAddr::V4((bits as u32).into())),
            IpAddr::V6(_) => out.push(IpAddr::V6(bits.into())),
        }
    }
    Ok(())
}

/// Parse and expand the `source_ips` step parameter into the address list:
/// an array of strings (a single string is accepted as a one-element list),
/// each a bare IP or a CIDR prefix. The total expanded size is capped at
/// [`MAX_SOURCE_IPS`]. Errors are actionable (name the entry, suggest the
/// fix) — lint runs this same parse so a bad list fails before the run.
pub fn parse_source_ips(value: &Value) -> Result<Vec<IpAddr>, String> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let entries: Vec<&str> = match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item.as_str() {
                    Some(s) => out.push(s),
                    None => {
                        return Err(format!(
                            "source_ips entry #{i} must be a string (an IP address or CIDR prefix)"
                        ))
                    }
                }
            }
            out
        }
        _ => {
            return Err(
                "source_ips must be an array of strings (IP addresses or CIDR prefixes)".into(),
            )
        }
    };
    if entries.is_empty() {
        return Err(
            "source_ips must not be empty — drop the key or list at least one address".into(),
        );
    }
    let mut out = Vec::new();
    for entry in entries {
        expand_source_ip_entry(entry, &mut out)?;
        if out.len() > MAX_SOURCE_IPS {
            return Err(format!(
                "source_ips expands to over {MAX_SOURCE_IPS} addresses — narrow the prefixes"
            ));
        }
    }
    Ok(out)
}

/// Resolve the step's `source_ips` parameter, caching the expanded list per
/// distinct parameter value — a hot step executes per iteration and
/// re-expanding a CIDR each time would be pure overhead. Errors are not
/// cached (they fail the step before any network call, and lint catches them
/// first anyway).
fn resolve_source_ips(value: &Value) -> Result<Option<Arc<Vec<IpAddr>>>, String> {
    if value.is_null() {
        return Ok(None);
    }
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<IpAddr>>>>> = OnceLock::new();
    let key = value.to_string();
    if let Some(hit) = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(&key)
    {
        return Ok(Some(Arc::clone(hit)));
    }
    let ips = Arc::new(parse_source_ips(value)?);
    CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(key, Arc::clone(&ips));
    Ok(Some(ips))
}

/// Log-once announce: the first execution that uses a given expanded list
/// adds a `source_ips: N address(es) active …` line to its step output, so
/// the run log records how many source addresses are in play without every
/// iteration repeating it.
fn announce_source_ips(key: &str, ips: &[IpAddr]) -> Option<String> {
    static ANNOUNCED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let mut seen = ANNOUNCED
        .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap();
    if !seen.insert(key.to_string()) {
        return None;
    }
    Some(format!(
        "source_ips: {} address(es) active ({} … {}), VU i pinned to address[(i-1) % {}]",
        ips.len(),
        ips[0],
        ips[ips.len() - 1],
        ips.len()
    ))
}

/// Key of the lazily-built source-IP client set: (shard, insecure, address).
type SourceClientKey = (usize, bool, IpAddr);

/// A client bound to a local source address, built lazily on first use per
/// (shard, insecure, address) and leaked for the process lifetime — the
/// static-shard pattern of [`shared_client`] cannot apply to an unbounded
/// key set. Laziness keeps memory honest: a 4096-address list does not mean
/// 4096 clients, because a VU only ever touches its own (shard, address)
/// pair, so a run builds at most min(VUs, shards × addresses) clients.
fn source_client(shard: usize, insecure: bool, ip: IpAddr) -> &'static reqwest::Client {
    static CLIENTS: OnceLock<Mutex<HashMap<SourceClientKey, &'static reqwest::Client>>> =
        OnceLock::new();
    let key = (shard, insecure, ip);
    let mut map = CLIENTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(client) = map.get(&key) {
        return client;
    }
    let client: &'static reqwest::Client =
        Box::leak(Box::new(build_client_inner(insecure, Some(ip))));
    map.insert(key, client);
    client
}

/// The HTTP client for a step execution, optionally bound to a source
/// address. `None` selects exactly the pre-`source_ips` behaviour
/// ([`client`]); with an address the client is keyed by (pool mode, shard,
/// insecure, address) — reqwest pools connections per client, so per-address
/// clients give per-address pools for free.
pub fn client_with_source(
    pool: ClientPool,
    insecure: bool,
    shard: usize,
    source_ip: Option<IpAddr>,
) -> &'static reqwest::Client {
    match source_ip {
        None => client(pool, insecure, shard),
        Some(ip) => {
            let shard = match pool {
                ClientPool::PerVu | ClientPool::Dedicated => shard % client_shard_count(),
                ClientPool::Shared => SHARED_POOL_SHARD,
            };
            source_client(shard, insecure, ip)
        }
    }
}

// ---------------------------------------------------------------------------
// Dedicated per-VU clients (`pool: dedicated`)
// ---------------------------------------------------------------------------
//
// The shard pools cap idle connections per host at 64 per shard — at 25k
// VUs, ~1500 VUs share one 64-socket idle pool, so a VU's connection is
// evicted between its requests and every request reconnects: connect churn
// and TIME_WAIT storms instead of N standing sockets. `pool: dedicated`
// gives each VU its own client, created on the VU's first request and held
// until the VU exits, so a VU's keep-alive connection survives think time
// (`std/sleep@v1`) untouched. Composes with `source_ips`: the key carries
// the VU's pinned source address, so both pinnings hold simultaneously.

/// Key of the dedicated per-VU client set: (vu_id, insecure, source
/// address). vu_id 0 (hand-built contexts) shares one entry.
type DedicatedClientKey = (u64, bool, Option<IpAddr>);

fn dedicated_clients() -> &'static Mutex<HashMap<DedicatedClientKey, reqwest::Client>> {
    static CLIENTS: OnceLock<Mutex<HashMap<DedicatedClientKey, reqwest::Client>>> = OnceLock::new();
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The VU's dedicated client, created on first use. `reqwest::Client` is a
/// cheap Arc handle, so handing out a clone per request shares the
/// underlying pool; the registry's copy is dropped by
/// [`drain_dedicated_vu_clients`] when the VU exits, and with it the last
/// strong reference — the pool (and its idle sockets) close there, not at
/// iteration end and not at process exit.
fn dedicated_client(vu_id: u64, insecure: bool, source_ip: Option<IpAddr>) -> reqwest::Client {
    let key = (vu_id, insecure, source_ip);
    let mut map = dedicated_clients().lock().unwrap();
    map.entry(key)
        .or_insert_with(|| build_client_inner(insecure, source_ip))
        .clone()
}

/// Drop every dedicated client of an exited VU. Called by the runner at VU
/// exit (fixed, staged, and arrival-rate workers alike) — a ramp-down must
/// close the VU's standing connection, not leak it to the next VU id
/// collision or process end.
pub(crate) fn drain_dedicated_vu_clients(vu_id: u64) {
    dedicated_clients()
        .lock()
        .unwrap()
        .retain(|key, _| key.0 != vu_id);
}

/// Log-once announce for dedicated mode, same pattern as
/// [`announce_source_ips`]: the first dedicated execution of the process
/// reports the mode, later ones stay quiet.
fn announce_dedicated() -> Option<String> {
    static ANNOUNCED: OnceLock<Mutex<bool>> = OnceLock::new();
    let mut seen = ANNOUNCED.get_or_init(|| Mutex::new(false)).lock().unwrap();
    if *seen {
        return None;
    }
    *seen = true;
    Some(
        "pool: dedicated — one HTTP client (keep-alive connection) per VU, held until the VU exits"
            .to_string(),
    )
}

#[cfg(test)]
pub(crate) fn dedicated_registry_len() -> usize {
    dedicated_clients().lock().unwrap().len()
}

#[cfg(test)]
pub(crate) fn dedicated_registry_has_vu(vu_id: u64) -> bool {
    dedicated_clients()
        .lock()
        .unwrap()
        .keys()
        .any(|k| k.0 == vu_id)
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

/// Default response body cap for HTTP-family steps (the
/// `max_response_body_bytes` run config key). Matches the WASM `net`
/// capability's 16 MiB body cap — one size for "a response we hold in
/// memory".
pub const DEFAULT_MAX_RESPONSE_BODY_BYTES: u64 = 16 * 1024 * 1024;

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

/// Why an exchange failed. Transport covers connect/TLS/read errors (the
/// reqwest error chain is reported); `BodyTooLarge` is a client-side cap
/// breach — the response body exceeded `max_response_body_bytes`. Both fail
/// the step; the cap breach gets its own variant so the error names the
/// config key and a truncated body can never silently feed a `check:`.
pub enum ExchangeError {
    Transport(reqwest::Error),
    BodyTooLarge { limit: u64, status: u16 },
}

/// Read the whole response body, rejecting anything over `limit` bytes. A
/// declared over-cap `Content-Length` fails before a single body byte is
/// read; chunked responses are accumulated with the same cap. A mid-body
/// read error is a transport failure — the previous behavior
/// (`unwrap_or_default`, empty body on error) could let a truncated
/// payload pass a `check:`.
async fn read_body_capped(
    resp: &mut reqwest::Response,
    limit: u64,
    status: u16,
) -> Result<Vec<u8>, ExchangeError> {
    if resp.content_length().is_some_and(|len| len > limit) {
        return Err(ExchangeError::BodyTooLarge { limit, status });
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(ExchangeError::Transport)? {
        if buf.len() + chunk.len() > limit as usize {
            return Err(ExchangeError::BodyTooLarge { limit, status });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Send `req`, timing the whole exchange (connect → body read). A transport
/// error returns `(elapsed_ms, error)` so the caller reports timing and the
/// failure from one code path. Bodies over `max_body_bytes` fail with
/// [`ExchangeError::BodyTooLarge`] — never silently truncated.
pub async fn timed_exchange(
    req: reqwest::RequestBuilder,
    body_kind: BodyKind,
    max_body_bytes: u64,
) -> Result<HttpOutcome, (f64, ExchangeError)> {
    let t0 = Instant::now();
    let mut resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return Err((
                t0.elapsed().as_secs_f64() * 1000.0,
                ExchangeError::Transport(e),
            ))
        }
    };
    let status = resp.status().as_u16();
    let reason = resp.status().canonical_reason().unwrap_or("").to_string();
    let headers = header_map_to_json(resp.headers());

    let bytes = match read_body_capped(&mut resp, max_body_bytes, status).await {
        Ok(b) => b,
        Err(e) => return Err((t0.elapsed().as_secs_f64() * 1000.0, e)),
    };
    // `text()` on this reqwest build (default-features off, no `charset`) is
    // exactly `from_utf8_lossy(bytes)` — decoding the capped buffer is the
    // byte-identical decode.
    let (body, body_base64) = match body_kind {
        BodyKind::Text => (String::from_utf8_lossy(&bytes).into_owned(), None),
        BodyKind::Auto => {
            // Textual payloads keep the historic `body` string; binary
            // payloads surface as `body_base64` so protobuf descriptor sets,
            // images etc. can flow into later steps
            // (`descriptor_set: "${{ fetch.body_base64 }}"`).
            let textual = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(is_textual_content_type);
            // Headers were snapshotted above; the body is already read, so
            // decide from the snapshot + the buffer itself.
            match textual {
                Some(true) => (String::from_utf8_lossy(&bytes).into_owned(), None),
                Some(false) => (String::new(), Some(base64_encode(&bytes))),
                None => match std::str::from_utf8(&bytes) {
                    Ok(_) => (String::from_utf8_lossy(&bytes).into_owned(), None),
                    Err(_) => (String::new(), Some(base64_encode(&bytes))),
                },
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
//   url       – required; `${…}` generator tokens (`${uuid}`, `${rand}`,
//               `${alias.fn(…)}`) expand per execution — in the URL, header
//               values, and the body
//   headers   – optional JSON object { "Name": "Value" }; values expand
//   body      – optional: JSON object → application/json, string → text/plain;
//               string leaves expand
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
//   pool      – optional "per-vu" (default), "shared" (one process-global
//               client for every VU), or "dedicated" (one client per VU,
//               held until the VU exits — N VUs hold N keep-alive
//               connections across think time)
//   source_ips – optional string or array of strings: local source addresses
//               to bind outgoing connections to. Entries are bare IPs
//               ("10.0.0.9", "fd00::5") or CIDR prefixes ("10.0.0.0/28",
//               "fd00::/120") expanded to every address of the prefix
//               (≤ 4096 total). VU i is pinned to ips[(i-1) % n] — stable
//               per VU so the (shard, source IP) client's keep-alive pool
//               stays warm. Multiplies the ~64k-concurrent-connections-per-
//               endpoint budget of a single source address. No `${…}`
//               expansion: the list is validated at lint and cached.
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
    let source_ips = match resolve_source_ips(&params["source_ips"]) {
        Ok(ips) => ips,
        Err(msg) => return err(step_name, &msg),
    };
    let source_ip = source_ips
        .as_ref()
        .map(|ips| source_ip_for_vu(ips, ctx.vu_id));
    let mut announce = source_ips
        .as_ref()
        .and_then(|ips| announce_source_ips(&params["source_ips"].to_string(), ips));
    if pool == ClientPool::Dedicated {
        announce = announce.or_else(announce_dedicated);
    }

    // `${…}` generator tokens expand per execution — one request is one
    // message: the URL, header values, and body share one generator context
    // (see `Context::token_expander`). An expansion error fails the step
    // before any network call, like ws/grpc.
    let mut expander = ctx.token_expander();
    let url = match expander.expand(&url) {
        Ok(u) => u,
        Err(msg) => return err(step_name, &msg),
    };

    let reqwest_method = match reqwest::Method::from_bytes(method.as_bytes()) {
        Ok(m) => m,
        Err(_) => return err(step_name, &format!("invalid HTTP method '{method}'")),
    };

    let client = match pool {
        ClientPool::Dedicated => dedicated_client(ctx.vu_id, insecure, source_ip),
        // Cheap Arc clone of a 'static shard/global client — one type for
        // both pool paths below.
        _ => client_with_source(pool, insecure, ctx.http_client_shard, source_ip).clone(),
    };
    let mut req = client
        .request(reqwest_method, &url)
        .timeout(Duration::from_millis(timeout_ms));

    if let Some(headers) = params["headers"].as_object() {
        for (k, v) in headers {
            if let Some(val) = v.as_str() {
                let val = match expander.expand(val) {
                    Ok(val) => val,
                    Err(msg) => return err(step_name, &msg),
                };
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
            Value::String(s) => {
                let body = match expander.expand(s) {
                    Ok(b) => b,
                    Err(msg) => return err(step_name, &msg),
                };
                req = req.header("content-type", "text/plain").body(body);
            }
            other => {
                let body = match expander.expand_value(other) {
                    Ok(b) => b,
                    Err(msg) => return err(step_name, &msg),
                };
                req = req
                    .header("content-type", "application/json")
                    .body(body.to_string());
            }
        }
    }

    let mut out = match timed_exchange(req, BodyKind::Auto, ctx.max_response_body_bytes).await {
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
        Err((duration_ms, e)) => exchange_error(step_name, &method, &url, &e, duration_ms),
    };
    if let Some(line) = announce {
        out.logs.insert(0, (LogTag::Out, line));
    }
    out
}

/// Map an exchange failure to the action output: transport errors report
/// the reqwest error chain (timeouts in their historic shape); a
/// `max_response_body_bytes` breach fails the step with the cap named — the
/// real status and timing are recorded, so the summary shows it as a failed
/// request, not a green one with a silently missing body.
pub fn exchange_error(
    step_name: &str,
    method: &str,
    url: &str,
    e: &ExchangeError,
    duration_ms: f64,
) -> ActionOutput {
    match e {
        ExchangeError::Transport(e) => transport_error(step_name, method, url, e, duration_ms),
        ExchangeError::BodyTooLarge { limit, status } => {
            let msg = format!(
                "{method} {url} → response body exceeds max_response_body_bytes ({limit} bytes) after {duration_ms:.2}ms"
            );
            ActionOutput {
                value: json!({
                    "error": msg,
                    "status": status,
                    "duration_ms": duration_ms,
                }),
                logs: vec![(LogTag::Err, msg.clone())],
                success: false,
                http_sample: Some(HttpSample {
                    duration_ms,
                    status: *status,
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
        assert_eq!(
            ClientPool::from_params(&json!({ "pool": "dedicated" })).unwrap(),
            ClientPool::Dedicated
        );
    }

    #[test]
    fn client_pool_rejects_unknown_mode_with_actionable_error() {
        let e = ClientPool::from_params(&json!({ "pool": "global" })).unwrap_err();
        assert!(e.contains("invalid pool 'global'"), "{e}");
        assert!(e.contains("per-vu"), "{e} lists the valid modes");
        assert!(e.contains("shared"), "{e} lists the valid modes");
        assert!(e.contains("dedicated"), "{e} lists the valid modes");
    }

    // -----------------------------------------------------------------
    // Dedicated per-VU clients (pool: dedicated)
    // -----------------------------------------------------------------

    #[test]
    fn dedicated_client_is_reused_per_vu_and_keyed_by_source_ip() {
        // Key-presence assertions only — the registry is process-global and
        // tests run in parallel.
        let _a1 = dedicated_client(9_001, false, None);
        let _a2 = dedicated_client(9_001, false, None);
        let _b = dedicated_client(9_002, false, None);
        let _a_src = dedicated_client(9_001, false, Some(IpAddr::from([127, 0, 0, 1])));
        assert!(dedicated_registry_has_vu(9_001));
        assert!(dedicated_registry_has_vu(9_002));
        // Drain removes exactly the exited VU's clients (both keys of 9001).
        drain_dedicated_vu_clients(9_001);
        assert!(!dedicated_registry_has_vu(9_001));
        assert!(dedicated_registry_has_vu(9_002), "other VUs untouched");
        drain_dedicated_vu_clients(9_002);
        assert!(!dedicated_registry_has_vu(9_002));
    }

    /// A keep-alive echo server: responds to every request with the peer's
    /// port as the body, holds every connection open, and tracks how many
    /// sockets are currently established. Returns (port, established gauge,
    /// accept counter).
    async fn peer_port_server() -> (
        u16,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let established = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::new(AtomicUsize::new(0));
        let (est, acc) = (Arc::clone(&established), Arc::clone(&accepted));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, peer)) = listener.accept().await else {
                    break;
                };
                acc.fetch_add(1, Ordering::SeqCst);
                est.fetch_add(1, Ordering::SeqCst);
                let est = Arc::clone(&est);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let body = peer.port().to_string();
                    let mut buf = [0u8; 4096];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let resp = format!(
                                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                                    body.len()
                                );
                                if sock.write_all(resp.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    est.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        (port, established, accepted)
    }

    async fn get_peer_port(url: &str, vu_id: u64, dedicated: bool) -> u64 {
        let mut ctx = Context::new();
        ctx.vu_id = vu_id;
        let params = if dedicated {
            json!({ "method": "GET", "url": url, "pool": "dedicated" })
        } else {
            json!({ "method": "GET", "url": url })
        };
        let out = http_action(&params, "fetch", &ctx).await;
        assert!(out.success, "vu {vu_id}: {:?}", out.logs);
        out.value["body"].as_str().unwrap().parse().unwrap()
    }

    /// N dedicated VUs hold N distinct sockets simultaneously across think
    /// time: a second round after a sleep reuses exactly the same ports, and
    /// the server counted no new connections. Draining the VUs closes every
    /// socket. Unique vu_id band — the registry is process-global and tests
    /// run in parallel.
    #[tokio::test]
    async fn dedicated_pool_holds_one_connection_per_vu_across_think_time() {
        const N: u64 = 100;
        const VU_BASE: u64 = 5_000;
        let (port, established, accepted) = peer_port_server().await;
        let url = format!("http://127.0.0.1:{port}/");

        let mut first = Vec::new();
        for vu in VU_BASE..VU_BASE + N {
            first.push(get_peer_port(&url, vu, true).await);
        }
        // Think time: the sockets must stay open with no traffic.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            established.load(std::sync::atomic::Ordering::SeqCst),
            N as usize,
            "every dedicated VU keeps its socket open across think time"
        );

        let mut second = Vec::new();
        for vu in VU_BASE..VU_BASE + N {
            second.push(get_peer_port(&url, vu, true).await);
        }
        assert_eq!(first, second, "each VU reuses its own connection");
        let distinct: std::collections::HashSet<_> = first.iter().collect();
        assert_eq!(distinct.len(), N as usize, "N VUs = N distinct sockets");
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            N as usize,
            "no reconnects between rounds"
        );

        // VU exit (the runner's drain hook) closes every standing socket.
        for vu in VU_BASE..VU_BASE + N {
            drain_dedicated_vu_clients(vu);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(established.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// Contrast: the default shard pool shares one client across VUs pinned
    /// to the same shard (all hand-built contexts are shard 0), so 30 VUs
    /// reuse a handful of sockets instead of holding 30.
    #[tokio::test]
    async fn default_pool_does_not_hold_one_socket_per_vu() {
        const N: u64 = 30;
        const VU_BASE: u64 = 6_000;
        let (port, _established, _accepted) = peer_port_server().await;
        let url = format!("http://127.0.0.1:{port}/");

        let mut ports = Vec::new();
        for vu in VU_BASE..VU_BASE + N {
            ports.push(get_peer_port(&url, vu, false).await);
        }
        let distinct: std::collections::HashSet<_> = ports.iter().collect();
        assert!(
            distinct.len() < N as usize,
            "shard pool reuses sockets across VUs, got {} distinct for {N} VUs",
            distinct.len()
        );
    }

    /// Dedicated mode composes with source_ips: the client is keyed
    /// (vu_id, source address), so both pinnings hold at once. Gated like
    /// the other loopback test — needs 127.0.0.2/.3 assigned.
    #[tokio::test]
    async fn dedicated_pool_composes_with_source_ips() {
        if std::env::var("PERFSCALE_TEST_SOURCE_IPS").is_err() {
            eprintln!("skipping: set PERFSCALE_TEST_SOURCE_IPS=1 to run");
            return;
        }
        for ip in ["127.0.0.2", "127.0.0.3"] {
            if std::net::TcpListener::bind((ip, 0)).is_err() {
                eprintln!("skipping: {ip} is not assigned on this host");
                return;
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let peers = Arc::new(Mutex::new(Vec::<IpAddr>::new()));
        let peers_task = Arc::clone(&peers);
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut sock, peer) = listener.accept().await.unwrap();
                peers_task.lock().unwrap().push(peer.ip());
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                    .await
                    .unwrap();
            }
        });

        for vu_id in [7_001u64, 7_002] {
            let mut ctx = Context::new();
            ctx.vu_id = vu_id;
            let out = http_action(
                &json!({
                    "method": "GET",
                    "url": format!("http://127.0.0.1:{port}/"),
                    "pool": "dedicated",
                    "source_ips": ["127.0.0.2", "127.0.0.3"],
                }),
                "fetch",
                &ctx,
            )
            .await;
            assert!(out.success, "vu {vu_id}: {:?}", out.logs);
        }
        server.await.unwrap();

        let peers = peers.lock().unwrap();
        assert_eq!(peers[0], IpAddr::from([127, 0, 0, 2]));
        assert_eq!(peers[1], IpAddr::from([127, 0, 0, 3]));
        drop(peers);
        drain_dedicated_vu_clients(7_001);
        drain_dedicated_vu_clients(7_002);
    }

    /// Scale smoke for the "thousands of VU clients" claim — gated behind
    /// PERFSCALE_TEST_DEDICATED_SCALE=<n> because it allocates n clients.
    /// No sockets are opened: this measures the client bookkeeping itself
    /// (a reqwest::Client is an Arc handle; n entries must cost megabytes,
    /// not gigabytes, and build fast).
    #[tokio::test]
    async fn dedicated_registry_scales_to_thousands_of_vus() {
        let Ok(n) = std::env::var("PERFSCALE_TEST_DEDICATED_SCALE") else {
            eprintln!("skipping: set PERFSCALE_TEST_DEDICATED_SCALE=5000 to run");
            return;
        };
        let n: u64 = n.parse().unwrap();
        let base = dedicated_registry_len();
        let t0 = Instant::now();
        for vu in 10_000..10_000 + n {
            let _ = dedicated_client(vu, false, None);
        }
        let elapsed = t0.elapsed();
        assert_eq!(
            dedicated_registry_len(),
            base + n as usize,
            "one registry entry per VU"
        );
        eprintln!(
            "dedicated_registry: {n} clients built in {elapsed:?} ({:.0} clients/s)",
            n as f64 / elapsed.as_secs_f64()
        );
        for vu in 10_000..10_000 + n {
            drain_dedicated_vu_clients(vu);
        }
        assert_eq!(dedicated_registry_len(), base);
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

    // -----------------------------------------------------------------
    // Response body cap (max_response_body_bytes)
    // -----------------------------------------------------------------

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A declared over-cap Content-Length fails the step: the error names
    /// the config key, the real status is recorded, and the request counts
    /// as failed — never a green step with a silently truncated body.
    #[tokio::test]
    async fn response_body_over_cap_fails_the_step() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(4096)))
            .mount(&server)
            .await;

        let mut ctx = Context::new();
        ctx.max_response_body_bytes = 1024;
        let out = http_action(
            &json!({ "method": "GET", "url": format!("{}/big", server.uri()) }),
            "fetch",
            &ctx,
        )
        .await;

        assert!(!out.success, "over-cap body must fail the step");
        let line = &out.logs[0].1;
        assert!(line.contains("max_response_body_bytes"), "{line}");
        assert!(line.contains("1024"), "{line}");
        let sample = out.http_sample.expect("failed sample recorded");
        assert!(sample.failed);
        assert_eq!(sample.status, 200, "the real status is kept");
    }

    /// A body under the cap decodes exactly as before (textual path,
    /// interpolation into the next step intact).
    #[tokio::test]
    async fn response_body_under_cap_decodes_normally() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/small"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("hello body")
                    .insert_header("content-type", "text/plain"),
            )
            .mount(&server)
            .await;

        let mut ctx = Context::new();
        ctx.max_response_body_bytes = 1024;
        let out = http_action(
            &json!({ "method": "GET", "url": format!("{}/small", server.uri()) }),
            "fetch",
            &ctx,
        )
        .await;

        assert!(out.success);
        assert_eq!(out.value["body"], "hello body");
        // The stored output interpolates as before.
        ctx.set("resp", out.value);
        assert_eq!(ctx.interpolate("got ${{ resp.body }}"), "got hello body");
    }

    // -----------------------------------------------------------------
    // gzip response decoding (reqwest `gzip` feature)
    // -----------------------------------------------------------------

    /// A server that answers `Accept-Encoding: gzip` with a gzipped body gets
    /// its response transparently decoded by reqwest: the step succeeds, the
    /// decoded JSON is valid, and `std/pick@v1` can select from
    /// `${{ feed.body.posts }}` — the million-users feed flow.
    #[tokio::test]
    async fn gzip_encoded_json_feed_decodes_and_feeds_pick() {
        use std::io::Write as _;
        let body = r#"{"posts":[{"id":1,"title":"hello"},{"id":2,"title":"world"}]}"#;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(body.as_bytes()).unwrap();
        let gzipped = enc.finish().unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/feed"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("content-encoding", "gzip")
                    .set_body_bytes(gzipped),
            )
            .mount(&server)
            .await;

        let mut ctx = Context::new();
        let out = http_action(
            &json!({
                "method": "GET",
                "url": format!("{}/feed", server.uri()),
                "headers": { "Accept-Encoding": "gzip" },
            }),
            "feed",
            &ctx,
        )
        .await;

        assert!(out.success, "gzipped feed must succeed: {:?}", out.logs);
        assert_eq!(out.value["body"], body);
        assert!(
            out.value.get("body_base64").is_none(),
            "decoded JSON is textual, not binary"
        );

        // The decoded body drives the JSON-path interpolation and std/pick.
        ctx.set("feed", out.value);
        let posts = ctx.interpolate("${{ feed.body.posts }}");
        let picked = super::super::actions::execute_action(
            "std/pick@v1",
            &json!({ "from": posts, "index": 1 }),
            &ctx,
            "pick a post",
        )
        .await;
        assert!(picked.success, "pick failed: {:?}", picked.logs);
        assert_eq!(picked.value, json!({"id": 2, "title": "world"}));
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
    // source_ips parsing / validation
    // -----------------------------------------------------------------

    #[test]
    fn source_ips_absent_and_single_ip() {
        assert_eq!(
            parse_source_ips(&Value::Null).unwrap(),
            Vec::<IpAddr>::new()
        );
        let ips = parse_source_ips(&json!(["127.0.0.2", "fd00::5"])).unwrap();
        assert_eq!(
            ips,
            vec![
                IpAddr::from([127, 0, 0, 2]),
                "fd00::5".parse::<IpAddr>().unwrap()
            ]
        );
        // A bare string is a one-element list.
        assert_eq!(
            parse_source_ips(&json!("10.0.0.9")).unwrap(),
            vec![IpAddr::from([10, 0, 0, 9])]
        );
    }

    #[test]
    fn source_ips_cidr_expands_every_address_v4_and_v6() {
        let v4 = parse_source_ips(&json!(["192.168.0.0/30"])).unwrap();
        assert_eq!(
            v4,
            (0..=3)
                .map(|i| IpAddr::from([192, 168, 0, i]))
                .collect::<Vec<_>>()
        );
        let v6 = parse_source_ips(&json!(["fd00::/126"])).unwrap();
        assert_eq!(v6.len(), 4);
        assert_eq!(v6[0], "fd00::".parse::<IpAddr>().unwrap());
        assert_eq!(v6[3], "fd00::3".parse::<IpAddr>().unwrap());
        // A /32 is exactly one address.
        assert_eq!(
            parse_source_ips(&json!(["10.1.2.3/32"])).unwrap(),
            vec![IpAddr::from([10, 1, 2, 3])]
        );
    }

    #[test]
    fn source_ips_rejects_bad_entries_with_actionable_errors() {
        let e = parse_source_ips(&json!(["not-an-ip"])).unwrap_err();
        assert!(e.contains("not-an-ip"), "{e}");
        assert!(e.contains("IP address or CIDR"), "{e}");

        let e = parse_source_ips(&json!(["10.0.0.0/33"])).unwrap_err();
        assert!(e.contains("/33"), "{e}");

        let e = parse_source_ips(&json!(["fd00::/129"])).unwrap_err();
        assert!(e.contains("/129"), "{e}");

        let e = parse_source_ips(&json!(["10.0.0.0/x"])).unwrap_err();
        assert!(e.contains("prefix length"), "{e}");

        let e = parse_source_ips(&json!([])).unwrap_err();
        assert!(e.contains("must not be empty"), "{e}");

        let e = parse_source_ips(&json!(42)).unwrap_err();
        assert!(e.contains("array of strings"), "{e}");

        let e = parse_source_ips(&json!([42])).unwrap_err();
        assert!(e.contains("must be a string"), "{e}");
    }

    #[test]
    fn source_ips_cidr_over_cap_is_rejected_with_narrowing_advice() {
        let e = parse_source_ips(&json!(["10.0.0.0/8"])).unwrap_err();
        assert!(e.contains("4096"), "{e}");
        assert!(e.contains("narrow"), "{e}");
        // IPv6 /0 must never try to materialize.
        let e = parse_source_ips(&json!(["fd00::/64"])).unwrap_err();
        assert!(e.contains("4096"), "{e}");
        // ...and a list whose combined expansion overflows is rejected too.
        let e = parse_source_ips(&json!(["10.0.0.0/20", "10.0.16.0/20"])).unwrap_err();
        assert!(e.contains("4096"), "{e}");
    }

    #[test]
    fn source_ip_pinning_is_stable_per_vu() {
        let ips: Vec<IpAddr> = vec![
            IpAddr::from([10, 0, 0, 1]),
            IpAddr::from([10, 0, 0, 2]),
            IpAddr::from([10, 0, 0, 3]),
        ];
        assert_eq!(source_ip_for_vu(&ips, 1), ips[0]);
        assert_eq!(source_ip_for_vu(&ips, 3), ips[2]);
        assert_eq!(source_ip_for_vu(&ips, 4), ips[0], "wraps around");
        assert_eq!(source_ip_for_vu(&ips, 0), ips[0], "hand-built contexts");
    }

    // -----------------------------------------------------------------
    // source_ips integration — two loopback sources, distinct peer addrs
    // -----------------------------------------------------------------
    //
    // Gated behind PERFSCALE_TEST_SOURCE_IPS=1 plus a bind preflight: Linux
    // routes all of 127/8, but macOS only answers 127.0.0.1 unless the
    // addresses are aliased onto lo0 first
    // (`sudo ifconfig lo0 alias 127.0.0.2 up` etc.), so a machine without
    // the aliases skips instead of failing.

    #[tokio::test]
    async fn source_ips_distinct_vus_use_distinct_loopback_sources() {
        if std::env::var("PERFSCALE_TEST_SOURCE_IPS").is_err() {
            eprintln!("skipping: set PERFSCALE_TEST_SOURCE_IPS=1 to run");
            return;
        }
        for ip in ["127.0.0.2", "127.0.0.3"] {
            if std::net::TcpListener::bind((ip, 0)).is_err() {
                eprintln!("skipping: {ip} is not assigned on this host");
                return;
            }
        }

        // Minimal HTTP echo: accept, record the peer address, respond 200.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let peers = Arc::new(Mutex::new(Vec::<IpAddr>::new()));
        let peers_task = Arc::clone(&peers);
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut sock, peer) = listener.accept().await.unwrap();
                peers_task.lock().unwrap().push(peer.ip());
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                    .await
                    .unwrap();
            }
        });

        for vu_id in [1u64, 2] {
            let mut ctx = Context::new();
            ctx.vu_id = vu_id;
            let out = http_action(
                &json!({
                    "method": "GET",
                    "url": format!("http://127.0.0.1:{port}/"),
                    "source_ips": ["127.0.0.2", "127.0.0.3"],
                }),
                "fetch",
                &ctx,
            )
            .await;
            assert!(out.success, "vu {vu_id}: {:?}", out.logs);
        }
        server.await.unwrap();

        let peers = peers.lock().unwrap();
        assert_eq!(peers[0], IpAddr::from([127, 0, 0, 2]), "vu 1 → first IP");
        assert_eq!(peers[1], IpAddr::from([127, 0, 0, 3]), "vu 2 → second IP");
    }

    #[test]
    fn source_ips_announcement_is_emitted_once_per_list() {
        let ips = vec![IpAddr::from([10, 9, 0, 1])];
        let key = "announce-test-10.9.0.1";
        let first = announce_source_ips(key, &ips);
        let second = announce_source_ips(key, &ips);
        assert!(first.is_some(), "first use announces");
        assert!(first.unwrap().contains("1 address(es) active"));
        assert!(second.is_none(), "repeat use stays quiet");
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
