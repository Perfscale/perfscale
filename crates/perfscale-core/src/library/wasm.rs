//! WASM value-generator libraries (RFC 005 phase 2) — wasmtime host.
//!
//! A local `.wasm` component (WASI Preview 2, `perfscale:library@0.2.0` —
//! legacy `@0.1.x` components keep loading, they just never see the run
//! settings; see `wit/library.wit` at the workspace root) becomes a
//! [`WasmLibraryProvider`]: the component is compiled **once per run**
//! (shared `Arc`-able [`Component`]; a `.cwasm` burn artifact in the cache —
//! see `super::burn` — replaces that compilation with a deserialization),
//! and every `instantiate()` mints a fresh
//! `Store` + instance per generator owner (per VU / live connection),
//! matching the native library lifecycle.
//!
//! # Capability enforcement (fail-closed)
//!
//! The component's WIT *imports* declare what it needs; the YAML
//! `capabilities:` list grants what it may use. At load time the imports are
//! inspected ([`Component::component_type`]) and mapped: `wasi:filesystem/*`
//! (+ the `wasi:io/*` plumbing it needs) → `fs`, `wasi:clocks/*` → `clock`,
//! `wasi:http/*` → `net`. A component importing more than granted is
//! a hard load error naming needed vs granted capabilities. The linker then
//! connects *only* the granted interfaces: `fs` is a read-only preopen
//! confined to the run's `fs_root`, `clock` is `wasi:clocks`, `net` is
//! `wasi:http/outgoing-handler` behind a host-side [`WasiHttpHooks`] that
//! checks every request's destination against the grant's host allowlist
//! (`api.acme.com` = any port, `api.acme.com:8443` = pinned port,
//! `*.internal` = suffix wildcard; see [`net_allowlist`]). Raw
//! `wasi:sockets` is never provided (RFC 005): an allowlist is only
//! enforceable while the host mediates the connection. `wasi:random`'s real
//! entropy interfaces are never connected (determinism: libraries draw from
//! `ctx.seed`); only `insecure-seed` is linked, sunk at a fixed value because
//! std's `HashMap` imports it unconditionally on wasip2.
//!
//! # Resource limits
//!
//! - **Fuel**: the engine is built with `consume_fuel(true)` and every
//!   `call`/`init` runs under a fresh [`PER_CALL_FUEL`] budget; a runaway
//!   guest traps and fails the step. Wall-time epoch interruption is *not*
//!   wired in phase 2: it needs a dedicated ticking thread per engine, and
//!   fuel already bounds guest compute. A guest blocked in a host-mediated
//!   HTTP call consumes no fuel, so the `net` hooks bound that separately:
//!   [`NET_REQUEST_TIMEOUT`] caps every request's connect/first-byte/
//!   between-bytes waits (and the whole send), and [`NET_BODY_CAP`] caps
//!   request and response bodies.
//! - **Memory**: [`StoreLimits`] caps linear memory at [`MEMORY_CAP`].
//!
//! # Runtime requirements for `net`
//!
//! wasmtime-wasi's sync hostcalls drive tokio internally. Every guest-call
//! path here lands where that is legal: `block_in_place` on the runner's
//! multi-thread runtime, the blocking pool on single-thread runtimes, and —
//! with no ambient runtime at all (lint, sync embedders) — wasmtime-wasi's
//! own internal fallback runtime. `net`-granted libraries therefore work on
//! every call path; the multi-thread runner hot path is covered by the
//! `multi_thread_runtime_calls_work_in_place` test.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p2::bindings as wasi_bindings;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::add_only_http_to_linker_sync;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use super::{CallCtx, Capability, FunctionInfo, LibraryInstance, LibraryProvider};

/// Host-side bindings for the 0.2 ABI (`wit/v0.2/library.wit` — the first
/// version with `settings-json` in the call context).
mod bindings_v2 {
    wasmtime::component::bindgen!({
        path: "../../wit/v0.2",
        world: "perfscale-library",
    });
}

/// Host-side bindings generated from the same `wit/library.wit` the SDK uses
/// (the current ABI, 0.3.x — like 0.2, plus the optional `config-schema`
/// export of issue #10 via the `perfscale-library-config` world).
mod bindings_v3 {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "perfscale-library",
    });
}

/// Bindings for the 0.3 `perfscale-library-config` world — components that
/// additionally export `config-schema`.
mod bindings_v3_config {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "perfscale-library-config",
    });
}

/// Bindings for the legacy 0.1 ABI (`wit/v0.1/library.wit` — context without
/// `settings-json`), so components built before 0.2 keep loading.
mod bindings_v1 {
    wasmtime::component::bindgen!({
        path: "../../wit/v0.1",
        world: "perfscale-library",
    });
}

/// The WIT package versions this host implements. A component built against
/// another major fails to load with both versions named.
const SUPPORTED_WIT: &str = "perfscale:library/library@0.1.x, @0.2.x and @0.3.x";

/// Fuel budget per guest call (`init` and `call`). Fuel is the phase-2
/// execution bound: ~50M fuel is generous for value generation (string
/// formatting, JSON parsing, small corpora lookups measure in the low
/// thousands) and traps infinite loops long before they hurt tail latency.
pub(crate) const PER_CALL_FUEL: u64 = 50_000_000;

/// Per-instance linear-memory cap: one instance per (library × generator
/// owner), so this multiplies — keep it tight (RFC 005 pitfall 5).
pub(crate) const MEMORY_CAP: usize = 64 << 20; // 64 MiB

/// Per-request wall-clock cap for host-mediated HTTP (the `net` capability).
/// A guest blocked in a hostcall consumes no fuel, so network time is bounded
/// host-side: applied as the ceiling for connect, first-byte and
/// between-bytes timeouts (guest `request-options` can only tighten it) and
/// as an overall deadline on the send itself.
const NET_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-message body cap (request and response) for host-mediated HTTP.
/// 16 MiB is generous for corpus/config fetches — the expected `net` use —
/// while keeping a hostile or broken endpoint from exhausting the per-instance
/// memory budget (fuel cannot see host-side buffers).
const NET_BODY_CAP: u64 = 16 << 20; // 16 MiB

/// Load-time size cap for a component file. Compilation memory scales with
/// module size and happens before fuel/memory limits can apply, so an
/// unbounded local `.wasm` is a host-side OOM vector; the cap matches the
/// fetch cap `perfscale install` enforces for remote artifacts (64 MiB is
/// ~5× a StarlingMonkey-class TS component). `pub(crate)` so
/// `super::burn::burn_component` enforces the same bound.
pub(crate) const MAX_COMPONENT_BYTES: usize = 64 << 20;

/// Shared engine: compilation settings are identical for every provider, and
/// one engine lets `Component` compilation cache across libraries of a run.
/// Also the engine burn artifacts are produced for and deserialized against
/// (see `super::burn`); its `Config` changes must bump `burn::engine_tag`.
pub(crate) fn engine() -> Result<&'static Engine, String> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut cfg = Config::new();
            cfg.consume_fuel(true);
            // Unit tests only: run without signal-based traps. With them,
            // wasmtime installs a Mach exception-handler thread (machports) on
            // macOS that aborts the whole process when a *non*-wasmtime thread
            // faults — under the default parallel test harness (~1 in 3 runs
            // ended in SIGABRT; serial runs were stable). Explicit checks are
            // marginally slower, which is why production keeps the signal
            // fast path; test builds pay it for determinism.
            #[cfg(test)]
            cfg.signals_based_traps(false);
            Engine::new(&cfg).map_err(|e| format!("failed to create the WASM engine: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Per-instance host state. `limits` enforces [`MEMORY_CAP`]. The HTTP
/// context and hooks are always present (cheap); the `net` boundary is that
/// the linker only connects `wasi:http` when the grant exists — and the
/// hooks' allowlist then mediates every request.
///
/// `reporter_output` is set only when the same state type hosts a WASM
/// reporter (`crate::report::wasm`): the `perfscale:reporter/host.emit-file`
/// import writes under that directory. Library instances leave it `None` —
/// they never link the reporter interface.
pub(crate) struct HostState {
    pub(crate) ctx: WasiCtx,
    pub(crate) http_ctx: WasiHttpCtx,
    pub(crate) hooks: net_allowlist::NetHooks,
    pub(crate) table: ResourceTable,
    pub(crate) limits: StoreLimits,
    pub(crate) reporter_output: Option<PathBuf>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            hooks: &mut self.hooks,
            table: &mut self.table,
            ctx: &mut self.http_ctx,
        }
    }
}

/// Marker for the `wasi:io/*` plumbing (streams/poll/error) that `fs`- and
/// `clock`-granted guests import transitively.
struct HasIo;

impl wasmtime::component::HasData for HasIo {
    type Data<'a> = &'a mut ResourceTable;
}

/// The `net` capability: host allowlist matching plus the [`WasiHttpHooks`]
/// implementation that enforces it (and the timeout/body caps) on every
/// outgoing request.
pub(crate) mod net_allowlist {
    use super::{NET_BODY_CAP, NET_REQUEST_TIMEOUT};
    use bytes::Bytes;
    use http_body::{Body, Frame, SizeHint};
    use http_body_util::BodyExt;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{ready, Context, Poll};
    use wasmtime_wasi_http::{
        default_send_request, Error, RequestOptions, WasiBody, WasiHttpHooks,
    };

    /// One parsed allowlist entry. Grammar (case-insensitive):
    /// `host` (any port), `host:port` (pinned port), `*.suffix` (any host
    /// under the suffix, any port), `*.suffix:port` (wildcard + pinned port).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct AllowEntry {
        /// As written in the YAML (error messages).
        pub(super) raw: String,
        host: HostPattern,
        port: Option<u16>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum HostPattern {
        /// Exact host, lowercased, trailing dot stripped.
        Exact(String),
        /// `*.suffix` — stored as the lowercased suffix with the leading
        /// dot (`.internal`); matches `foo.internal`, not `internal` itself.
        Suffix(String),
    }

    impl AllowEntry {
        pub(super) fn parse(raw: &str) -> Result<Self, String> {
            let raw = raw.trim();
            let (host, port) = match raw.rsplit_once(':') {
                // An entry ending in ":digits" pins the port; a trailing ':'
                // or non-numeric tail is a malformed entry, said explicitly.
                Some((h, p)) if !h.is_empty() => {
                    let port = p.parse::<u16>().map_err(|_| {
                        format!(
                            "invalid net allowlist entry '{raw}' — expected host, host:port, or *.suffix"
                        )
                    })?;
                    (h, Some(port))
                }
                _ => (raw, None),
            };
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            let pattern = if let Some(suffix) = host.strip_prefix('*') {
                if !suffix.starts_with('.') || suffix.len() < 2 {
                    return Err(format!(
                        "invalid net allowlist entry '{raw}' — a wildcard must look like '*.suffix'"
                    ));
                }
                HostPattern::Suffix(suffix.to_string())
            } else {
                // No IPv6 or otherwise colon-bearing hosts in v1.
                if host.is_empty() || host.contains(['*', ':', '/', ' ']) {
                    return Err(format!(
                        "invalid net allowlist entry '{raw}' — expected host, host:port, or *.suffix"
                    ));
                }
                HostPattern::Exact(host)
            };
            Ok(Self {
                raw: raw.to_string(),
                host: pattern,
                port,
            })
        }

        fn matches(&self, host: &str, port: u16) -> bool {
            if self.port.is_some_and(|pinned| pinned != port) {
                return false;
            }
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            match &self.host {
                HostPattern::Exact(h) => *h == host,
                HostPattern::Suffix(s) => host.ends_with(s.as_str()),
            }
        }
    }

    /// Whether `host:port` is allowed by the grant. `port` must already be
    /// resolved (explicit URI port, else the scheme default).
    pub(super) fn allowed(entries: &[AllowEntry], host: &str, port: u16) -> bool {
        entries.iter().any(|e| e.matches(host, port))
    }

    /// The host-side enforcement point for `wasi:http/outgoing-handler`:
    /// every request the guest sends goes through here before any socket is
    /// opened. Wraps the crate's default hyper-based sender with (a) the
    /// allowlist check and (b) the timeout/body caps of
    /// [`NET_REQUEST_TIMEOUT`]/[`NET_BODY_CAP`].
    pub(crate) struct NetHooks {
        allowlist: Vec<AllowEntry>,
    }

    impl NetHooks {
        pub(crate) fn new(allowlist: Vec<AllowEntry>) -> Self {
            Self { allowlist }
        }
    }

    impl WasiHttpHooks for NetHooks {
        fn send_request(
            &mut self,
            request: http::Request<WasiBody>,
            options: Option<RequestOptions>,
            fut: Box<dyn Future<Output = Result<(), Error>> + Send>,
        ) -> Box<
            dyn Future<
                    Output = Result<
                        (
                            http::Response<WasiBody>,
                            Box<dyn Future<Output = Result<(), Error>> + Send>,
                        ),
                        Error,
                    >,
                > + Send,
        > {
            _ = fut; // response-processing error reporting: unchanged default
            let uri = request.uri();
            // The handler fills scheme/authority from the request before the
            // hook runs; a missing piece is the guest's own malformed input.
            let host = uri.host().unwrap_or_default().to_string();
            let https = uri.scheme() == Some(&http::uri::Scheme::HTTPS);
            let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
            if !allowed(&self.allowlist, &host, port) {
                let entries = self
                    .allowlist
                    .iter()
                    .map(|e| e.raw.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                // Drain the request body before answering: the guest streams
                // it concurrently, and erroring with it unread races the
                // guest's own body-send teardown ("connection reset") — the
                // refusal must be the deterministic, host-named outcome.
                let mut body = request.into_body();
                return Box::new(async move {
                    use http_body_util::BodyExt;
                    while let Some(frame) = body.frame().await {
                        if frame.is_err() {
                            break;
                        }
                    }
                    Err(Error::InternalError(Some(format!(
                        "net capability: destination '{host}:{port}' is not allowlisted (granted: {entries})"
                    ))))
                });
            }

            // Time bound: clamp every phase timeout the guest asked for (or
            // defaulted) to NET_REQUEST_TIMEOUT, and put the same deadline on
            // the whole send — a guest in a hostcall burns no fuel, so this
            // is the only bound on network time.
            let mut options = options.unwrap_or_default();
            for t in [
                &mut options.connect_timeout,
                &mut options.first_byte_timeout,
                &mut options.between_bytes_timeout,
            ] {
                *t = Some(t.map_or(NET_REQUEST_TIMEOUT, |v| v.min(NET_REQUEST_TIMEOUT)));
            }

            let request = request.map(|b| LimitedBody::request(b, NET_BODY_CAP).boxed_unsync());
            Box::new(async move {
                let send = default_send_request(request, Some(options));
                let (response, io) = match tokio::time::timeout(NET_REQUEST_TIMEOUT, send).await {
                    Ok(r) => r?,
                    Err(_) => return Err(Error::HttpResponseTimeout),
                };
                Ok((
                    response.map(|b| {
                        LimitedBody::response(b.boxed_unsync(), NET_BODY_CAP).boxed_unsync()
                    }),
                    Box::new(io) as Box<dyn Future<Output = Result<(), Error>> + Send>,
                ))
            })
        }
    }

    /// A body wrapper enforcing the total-bytes cap: past the cap the stream
    /// errors with the wasi:http body-size error for the direction, so the
    /// guest sees a regular failure instead of a host-side abort.
    struct LimitedBody {
        inner: WasiBody,
        remaining: u64,
        over: fn(u64) -> Error,
    }

    impl LimitedBody {
        fn request(inner: WasiBody, cap: u64) -> Self {
            Self {
                inner,
                remaining: cap,
                over: |c| Error::HttpRequestBodySize(Some(c)),
            }
        }

        fn response(inner: WasiBody, cap: u64) -> Self {
            Self {
                inner,
                remaining: cap,
                over: |c| Error::HttpResponseBodySize(Some(c)),
            }
        }
    }

    impl Body for LimitedBody {
        type Data = Bytes;
        type Error = Error;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
            let this = &mut *self;
            match ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        let len = data.len() as u64;
                        if len > this.remaining {
                            return Poll::Ready(Some(Err((this.over)(NET_BODY_CAP))));
                        }
                        this.remaining -= len;
                    }
                    Poll::Ready(Some(Ok(frame)))
                }
                other => Poll::Ready(other),
            }
        }

        fn is_end_stream(&self) -> bool {
            self.inner.is_end_stream()
        }

        fn size_hint(&self) -> SizeHint {
            self.inner.size_hint()
        }
    }
}

/// Capabilities a component's imports require — computed at load and compared
/// against the YAML grant.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Needed {
    fs: bool,
    clock: bool,
    net: bool,
}

/// Parsed YAML grant (`capabilities: [...]` of a `libraries:` entry).
#[derive(Debug, Default, Clone)]
struct Grants {
    fs: bool,
    clock: bool,
    /// Host allowlist, parsed at load (fail-fast on malformed entries).
    net: Vec<net_allowlist::AllowEntry>,
}

impl Grants {
    fn parse(caps: &[Capability]) -> Result<Self, String> {
        let mut g = Grants::default();
        for cap in caps {
            match cap {
                Capability::Simple(s) if s == "fs" => g.fs = true,
                Capability::Simple(s) if s == "clock" => g.clock = true,
                Capability::Simple(s) => {
                    return Err(format!(
                        "unknown capability '{s}' — supported grants: fs, clock, net: [hosts]"
                    ));
                }
                Capability::Net { net } => {
                    for entry in net {
                        g.net.push(net_allowlist::AllowEntry::parse(entry)?);
                    }
                }
            }
        }
        Ok(g)
    }

    fn describe(&self) -> String {
        let mut out = Vec::new();
        if self.fs {
            out.push("fs".to_string());
        }
        if self.clock {
            out.push("clock".to_string());
        }
        if !self.net.is_empty() {
            let entries = self
                .net
                .iter()
                .map(|e| e.raw.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            out.push(format!("net: [{entries}]"));
        }
        if out.is_empty() {
            "(none)".into()
        } else {
            out.join(", ")
        }
    }
}

/// Map one WIT import name (`wasi:filesystem/types@0.2.9`) to the capability
/// it requires. `wasi:io/*` is shared plumbing satisfied by `fs` or `clock`.
///
/// Note: `wasi:filesystem/*` and `wasi:clocks/wall-clock` are unconditionally
/// imported by jco/StarlingMonkey-built (TS/JS) components too — for a
/// library whose `info()` declares `"pure": true` they are toolchain noise
/// just like the Rust-std sinks below, and [`WasmLibraryProvider::assemble`]
/// sinks them the same way after probing `info()`. For non-pure libraries
/// they keep requiring the matching grant.
fn import_capability(name: &str) -> Result<Option<&'static str>, String> {
    let base = name.split('@').next().unwrap_or(name);
    if let Some(rest) = base.strip_prefix("wasi:") {
        let ns = rest.split('/').next().unwrap_or(rest);
        return Ok(match ns {
            "filesystem" => Some("fs"),
            // wasi:io/* is shared plumbing (streams/poll/error), and
            // wasi:cli/* plus wasi:clocks/monotonic-clock are Rust-std noise
            // every rustc-built wasip2 component imports unconditionally —
            // the host connects them in sink form (empty stdin, discarded
            // stdout/stderr, no args/env, exit traps; the monotonic clock is
            // host-rate but data-free), so they need no capability. The
            // *wall* clock stays granted-only: time arrives via `ctx.time_ms`
            // by design (RFC 005).
            "io" | "cli" => None,
            "clocks" if rest == "clocks/monotonic-clock" => None,
            // wasi:random/insecure-seed is toolchain noise too: std's HashMap
            // (RandomState) imports it on wasip2. The host sinks it at a fixed
            // value (deterministic — insecure-seed is documented as not for
            // sensitive data, and libraries draw randomness from ctx.seed).
            // wasi:random/random and wasi:random/insecure (real entropy) stay
            // unconnected forever.
            "random" if rest == "random/insecure-seed" => None,
            "clocks" => Some("clock"),
            "http" => Some("net"),
            other => {
                return Err(format!(
                    "wasi:{other}/* — this capability is not supported (supported: fs, clock, net; raw wasi:sockets is never provided)"
                ));
            }
        });
    }
    Err(format!(
        "'{name}' — unknown import; only wasi:filesystem, wasi:clocks, wasi:http (and their wasi:io plumbing) are supportable"
    ))
}

/// A local `.wasm` component as a [`LibraryProvider`]. Compile once, mint
/// instances per generator owner.
pub struct WasmLibraryProvider {
    /// `name@vX.Y.Z` from the component's `info()`.
    id: String,
    /// Bare library name from `info()` — the default token alias.
    name: String,
    path: PathBuf,
    component: Component,
    linker: Linker<HostState>,
    functions: Vec<FunctionInfo>,
    fs_root: Option<PathBuf>,
    grants: Grants,
    /// WIT ABI major the component was built against (1, 2 or 3) — picks
    /// the bindings used at instantiation.
    wit_major: u8,
    /// The component exports `perfscale:library/config-schema` (0.3,
    /// `perfscale-library-config` world) — it declares a config extension
    /// (issue #10).
    has_config_schema: bool,
    /// The claimed top-level config key and its JSON Schema, from the
    /// `config-schema` probe. `None` when the component has no such export.
    config_key: Option<&'static str>,
    config_schema: Option<Value>,
    /// The component's self-declared `"pure": true` from `info()`: never
    /// touches fs/clock/net, so its calls make no tokio-blocking WASI
    /// hostcalls and may run inline on a current-thread runtime.
    pure: bool,
}

impl WasmLibraryProvider {
    /// Load, compile and capability-check the component at `path` (anchored
    /// to the declaring file by import resolution). `capabilities` is the
    /// YAML grant; `fs_root` confines the `fs` preopen.
    ///
    /// Compilation goes through the burn cache (RFC 005 burn, phase 1): a
    /// `<sha256>.cwasm` artifact written by `perfscale install` deserializes
    /// in milliseconds; any miss or mismatch falls back to a full
    /// `Component::new` compile.
    pub fn load(
        path: &Path,
        capabilities: Option<&[Capability]>,
        fs_root: Option<&Path>,
    ) -> Result<Self, String> {
        let display = || path.display().to_string();
        let grants = check_grants(&display(), capabilities)?;

        let bytes = std::fs::read(path)
            .map_err(|e| format!("library '{}': failed to read: {e}", display()))?;
        if bytes.len() > MAX_COMPONENT_BYTES {
            return Err(format!(
                "library '{}': component is {} MiB — the limit is {} MiB (the same cap `perfscale install` enforces on remote artifacts)",
                display(),
                bytes.len() >> 20,
                MAX_COMPONENT_BYTES >> 20,
            ));
        }
        let engine = engine()?;

        // Burn-cache fast path. A missing or mismatched artifact is silent
        // (the cache is advisory; `perfscale install` re-burns).
        let sha = super::burn::sha256_bytes(&bytes);
        let burn_path = crate::import::library_burn_path(
            &crate::import::default_library_cache_root(),
            &super::burn::hex(&sha),
        );
        let burned = std::fs::read(&burn_path).ok().and_then(|artifact| {
            let hit = super::burn::load_burned(&artifact, &sha);
            if hit.is_none() {
                tracing::debug!(
                    "library '{}': burn artifact '{}' does not match this build — compiling from source",
                    path.display(),
                    burn_path.display()
                );
            }
            hit
        });
        let component = match burned {
            Some(component) => component,
            None => Component::new(engine, &bytes).map_err(|e| {
                format!(
                    "library '{}': failed to compile as a WASM component: {e}",
                    display()
                )
            })?,
        };
        Self::assemble(path.to_path_buf(), component, grants, fs_root)
    }

    /// Load a component that was deserialized from the binary's embedded
    /// burn payload (`perfscale burn`) — no `.wasm` file needs to exist.
    /// `use_` is the exact `use:` string (error messages, fs_root default).
    pub fn load_embedded(
        use_: &str,
        component: Component,
        capabilities: Option<&[Capability]>,
        fs_root: Option<&Path>,
    ) -> Result<Self, String> {
        let grants = check_grants(use_, capabilities)?;
        Self::assemble(PathBuf::from(use_), component, grants, fs_root)
    }

    /// Shared tail of [`Self::load`]/[`Self::load_embedded`]: WIT version
    /// check, linker construction, `info()` probe, capability enforcement.
    fn assemble(
        path: PathBuf,
        component: Component,
        grants: Grants,
        fs_root: Option<&Path>,
    ) -> Result<Self, String> {
        let display = || path.display().to_string();
        let engine = engine()?;

        // WIT version check: the component must export a supported
        // `perfscale:library/library@0.N.x`. The bindgen-generated bindings
        // would fail at instantiation anyway, but an explicit check names both
        // versions (RFC 005 "Versioning").
        let ty = component.component_type();
        let mut exported_wit: Option<String> = None;
        let mut has_config_schema = false;
        for (name, _) in ty.exports(engine) {
            if let Some(rest) = name.strip_prefix("perfscale:library/library") {
                exported_wit = Some(rest.trim_start_matches('@').to_string());
            }
            if name.starts_with("perfscale:library/config-schema") {
                has_config_schema = true;
            }
        }
        if has_config_schema
            && exported_wit
                .as_deref()
                .is_some_and(|v| !v.starts_with("0.3."))
        {
            return Err(format!(
                "library '{}': config-schema requires the 0.3 ABI — rebuild the component against wit/library.wit 0.3.0",
                display()
            ));
        }
        let wit_major: u8 = match exported_wit.as_deref() {
            Some(v) if v.starts_with("0.1.") => 1,
            Some(v) if v.starts_with("0.2.") => 2,
            Some(v) if v.starts_with("0.3.") => 3,
            other => {
                return Err(format!(
                    "library '{}': unsupported WIT interface version — this build supports {SUPPORTED_WIT}, the component exports perfscale:library/library@{}",
                    display(),
                    other.unwrap_or("<none — not a perfscale library component>")
                ));
            }
        };

        // Capability scan: map every import to its capability. Enforcement
        // against the grant happens AFTER the `info()` probe below — a
        // self-declared pure library may waive fs/clock — but imports the
        // host never provides fail right here (the linker could not satisfy
        // them anyway).
        let mut needed = Needed::default();
        let mut unsupported = Vec::new();
        for (name, _) in ty.imports(engine) {
            match import_capability(name) {
                Ok(Some("fs")) => needed.fs = true,
                Ok(Some("clock")) => needed.clock = true,
                Ok(Some("net")) => needed.net = true,
                Ok(Some(_)) | Ok(None) => {} // wasi:io/* plumbing
                Err(e) => unsupported.push(format!("{name} ({e})")),
            }
        }
        if !unsupported.is_empty() {
            return Err(format!(
                "library '{}': imports interfaces this host never provides:\n  {}",
                display(),
                unsupported.join("\n  ")
            ));
        }
        // net is enforced before the linker/probe: the `pure` marker never
        // waives it (it covers fs/clock only), and the wasi:http interfaces
        // are connected ONLY when granted — so without a grant the component
        // could not even instantiate for the info() probe.
        if needed.net && grants.net.is_empty() {
            return Err(format!(
                "library '{}': the component needs capabilities the YAML does not grant — needed: net; granted: {}. Add `- net: [\"host\", ...]` to the libraries: entry's capabilities (requires allow_library_capabilities: true)",
                display(),
                grants.describe(),
            ));
        }

        // The linker connects wasi:io + wasi:cli (sink form), wasi:clocks
        // and wasi:filesystem unconditionally — jco/StarlingMonkey-built
        // components import fs/wall-clock even when pure, and their `info()`
        // can only be probed once those imports link. The security boundary
        // is unchanged: the fs *preopen* is attached in `instantiate_store`
        // only when the YAML grants `fs`, so an ungranted guest that touches
        // the filesystem fails at runtime. wasi:http, in contrast, is linked
        // only with a net grant, and its hooks hold the allowlist.
        let linker = build_linker(engine, &display(), !grants.net.is_empty())?;

        // The fs preopen: confined to the run's fs_root; without one, the
        // directory containing the .wasm (the smallest sensible root).
        let fs_root = fs_root
            .map(Path::to_path_buf)
            .or_else(|| path.parent().map(Path::to_path_buf));

        let mut provider = Self {
            id: String::new(),
            name: String::new(),
            path: path.to_path_buf(),
            component,
            linker,
            functions: Vec::new(),
            fs_root,
            grants: grants.clone(),
            wit_major,
            has_config_schema,
            config_key: None,
            config_schema: None,
            pure: false,
        };

        // `info()` once at load, BEFORE grant enforcement: populates
        // id/name/functions for lint/validation and carries the `pure`
        // marker. The probe is sandboxed — no preopens unless granted,
        // fuel-limited like any other call. A malformed or trapping `info()`
        // is a load error.
        let info = provider.probe_info()?;
        if has_config_schema {
            provider.probe_config_schema()?;
        }

        // A library whose info() declares `"pure": true` never touches the
        // filesystem or the wall clock: its fs/wall-clock imports are
        // toolchain noise (jco/StarlingMonkey links them unconditionally),
        // sunk like the wasi:io/cli/monotonic-clock noise in
        // `import_capability`. Non-pure libraries keep the hard check.
        if info.pure {
            needed.fs = false;
            needed.clock = false;
        }
        let mut missing = Vec::new();
        if needed.fs && !grants.fs {
            missing.push("fs");
        }
        // The wall clock is dragged in by std's fs-metadata code too, so an
        // `fs` grant satisfies it; `clock` alone also satisfies it.
        if needed.clock && !grants.clock && !grants.fs {
            missing.push("clock");
        }
        if !missing.is_empty() {
            return Err(format!(
                "library '{}': the component needs capabilities the YAML does not grant — needed: {}; granted: {}. Add `capabilities: [{}]` to the libraries: entry (requires allow_library_capabilities: true)",
                display(),
                missing.join(", "),
                grants.describe(),
                missing.join(", ")
            ));
        }

        // The other direction (RFC 005): a grant wider than the imports is
        // harmless — the runtime sandbox keys on the grant, not the import
        // set — but almost always a stale/mistaken grant, so say so.
        // (The RFC calls this a lint warning; the load-time trace is the
        // implemented surface.)
        let mut unused = Vec::new();
        if grants.fs && !needed.fs {
            unused.push("fs");
        }
        if grants.clock && !needed.clock && !(needed.fs) {
            // `fs` already covers the wall clock.
            unused.push("clock");
        }
        if !grants.net.is_empty() && !needed.net {
            unused.push("net");
        }
        if !unused.is_empty() {
            tracing::warn!(
                "library '{}': capabilities granted but not imported by the component: {} — the grant is inert; drop it",
                path.display(),
                unused.join(", ")
            );
        }

        provider.pure = info.pure;
        provider.id = format!("{}@v{}", info.name, info.version);
        provider.name = info.name;
        provider.functions = info.functions;
        Ok(provider)
    }

    /// The library's own name (from `info()`) — the default token alias.
    pub fn library_name(&self) -> &str {
        &self.name
    }

    /// One fresh instance: new `Store` (fresh fuel, memory limits, WASI ctx)
    /// plus component instantiation through the bindings of the component's
    /// WIT major. Shared by `load`'s info probe and
    /// [`LibraryProvider::instantiate`].
    ///
    /// The fs capability's security boundary lives here: the fs interfaces
    /// are always linked (see [`build_linker`]), but the confined preopen is
    /// attached ONLY when the YAML grants `fs` — an ungranted guest sees the
    /// interfaces with zero preopens, so any file access fails at runtime.
    fn instantiate_store(&self) -> Result<(Store<HostState>, Instantiated), String> {
        let mut ctx = WasiCtx::builder();
        // Fixed insecure-seed: `seed:` runs stay reproducible even if the guest's
        // toolchain (std HashMap) asks for it.
        ctx.insecure_random_seed(0);
        if self.grants.fs {
            let root = self.fs_root.as_deref().ok_or_else(|| {
                format!(
                    "library '{}': fs capability granted but no fs_root could be determined",
                    self.path.display()
                )
            })?;
            // Read-only and confined: the guest sees fs_root as `/`.
            ctx.preopened_dir(root, "/", FsPerms::ReadOnly)
                .map_err(|e| {
                    format!(
                        "library '{}': failed to preopen fs_root '{}': {e}",
                        self.path.display(),
                        root.display()
                    )
                })?;
        }
        let state = HostState {
            ctx: ctx.build(),
            http_ctx: WasiHttpCtx::new(),
            hooks: net_allowlist::NetHooks::new(self.grants.net.clone()),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_CAP)
                .instances(16)
                .tables(16)
                .memories(1)
                .build(),
            reporter_output: None,
        };
        let mut store = Store::new(engine()?, state);
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(PER_CALL_FUEL)
            .map_err(|e| format!("library '{}': {e}", self.path.display()))?;
        let instance = match (self.wit_major, self.has_config_schema) {
            (1, _) => Instantiated::V1(
                bindings_v1::PerfscaleLibrary::instantiate(
                    &mut store,
                    &self.component,
                    &self.linker,
                )
                .map_err(|e| {
                    format!(
                        "library '{}': failed to instantiate: {e}",
                        self.path.display()
                    )
                })?,
            ),
            (2, _) => Instantiated::V2(
                bindings_v2::PerfscaleLibrary::instantiate(
                    &mut store,
                    &self.component,
                    &self.linker,
                )
                .map_err(|e| {
                    format!(
                        "library '{}': failed to instantiate: {e}",
                        self.path.display()
                    )
                })?,
            ),
            (_, true) => Instantiated::V3Config(
                bindings_v3_config::PerfscaleLibraryConfig::instantiate(
                    &mut store,
                    &self.component,
                    &self.linker,
                )
                .map_err(|e| {
                    format!(
                        "library '{}': failed to instantiate: {e}",
                        self.path.display()
                    )
                })?,
            ),
            _ => Instantiated::V3(
                bindings_v3::PerfscaleLibrary::instantiate(
                    &mut store,
                    &self.component,
                    &self.linker,
                )
                .map_err(|e| {
                    format!(
                        "library '{}': failed to instantiate: {e}",
                        self.path.display()
                    )
                })?,
            ),
        };
        Ok((store, instance))
    }

    /// Call `info()` on a throwaway instance and parse the JSON contract.
    fn probe_info(&self) -> Result<WasmLibraryInfo, String> {
        let (mut store, instance) = self.instantiate_store()?;
        let json = off_runtime(|| match &instance {
            Instantiated::V1(i) => i.perfscale_library_library().call_info(&mut store),
            Instantiated::V2(i) => i.perfscale_library_library().call_info(&mut store),
            Instantiated::V3(i) => i.perfscale_library_library().call_info(&mut store),
            Instantiated::V3Config(i) => i.perfscale_library_library().call_info(&mut store),
        })
        .map_err(|e| {
            format!(
                "library '{}': info() trapped or exhausted fuel: {e}",
                self.path.display()
            )
        })?;
        let parsed: WasmLibraryInfoJson = serde_json::from_str(&json).map_err(|e| {
            format!(
                "library '{}': info() returned invalid JSON: {e}",
                self.path.display()
            )
        })?;
        if parsed.name.is_empty() {
            return Err(format!(
                "library '{}': info() reports an empty name",
                self.path.display()
            ));
        }
        if parsed.functions.is_empty() {
            return Err(format!(
                "library '{}': info() exports no functions",
                self.path.display()
            ));
        }
        // `FunctionInfo` fields are `&'static str`; the strings arrive at run
        // time from JSON. Leak them: the set is tiny (functions of declared
        // libraries) and load is once per run — the leak never grows at call
        // volume.
        let functions = parsed
            .functions
            .into_iter()
            .map(|f| FunctionInfo {
                name: Box::leak(f.name.into_boxed_str()),
                description: Box::leak(f.description.into_boxed_str()),
                secret: f.secret,
            })
            .collect();
        Ok(WasmLibraryInfo {
            name: parsed.name,
            version: parsed.version,
            pure: parsed.pure,
            functions,
        })
    }

    /// Call the `config-schema` export (issue #10) and validate the
    /// declaration: `{ "key": "mylibrary", "schema": {…} }`. The key must be
    /// a `[a-z][a-z0-9_]*` identifier and must not collide with a built-in
    /// config key; the schema must compile as JSON Schema. Failures are load
    /// errors — the component declares the extension, so a broken
    /// declaration is its bug, not a runtime condition.
    fn probe_config_schema(&mut self) -> Result<(), String> {
        let (mut store, instance) = self.instantiate_store()?;
        let json = off_runtime(|| {
            instance
                .call_config_schema(&mut store)
                .expect("the config world was selected when the export was detected")
        })
        .map_err(|e| {
            format!(
                "library '{}': config-schema() trapped or exhausted fuel: {e}",
                self.path.display()
            )
        })?;
        let parsed: ConfigSchemaJson = serde_json::from_str(&json).map_err(|e| {
            format!(
                "library '{}': config-schema() returned invalid JSON: {e}",
                self.path.display()
            )
        })?;
        let valid_key =
            !parsed.key.is_empty()
                && parsed.key.bytes().enumerate().all(|(i, b)| {
                    b.is_ascii_lowercase() || b == b'_' || (i > 0 && b.is_ascii_digit())
                });
        if !valid_key {
            return Err(format!(
                "library '{}': config-schema key '{}' is not a valid key ([a-z][a-z0-9_]*)",
                self.path.display(),
                parsed.key
            ));
        }
        if crate::schema::known_config_keys().contains(&parsed.key) {
            return Err(format!(
                "library '{}': config-schema key '{}' collides with a built-in config key",
                self.path.display(),
                parsed.key
            ));
        }
        jsonschema::JSONSchema::compile(&parsed.schema).map_err(|e| {
            format!(
                "library '{}': config-schema for key '{}' is not a valid JSON Schema: {e}",
                self.path.display(),
                parsed.key
            )
        })?;
        // Leaked like the function names: one key per declared library, set
        // once per run at load.
        self.config_key = Some(Box::leak(parsed.key.into_boxed_str()));
        self.config_schema = Some(parsed.schema);
        Ok(())
    }
}

/// Parsed `config-schema` JSON contract (issue #10).
#[derive(serde::Deserialize)]
struct ConfigSchemaJson {
    key: String,
    schema: Value,
}

/// A component instance, through the bindings of its WIT major: 0.1
/// components get the legacy context (no `settings-json`), 0.2 and 0.3
/// components the current one (the 0.3 `library` interface is unchanged —
/// the new `config-schema` export lives on the `V3Config` variant).
enum Instantiated {
    V1(bindings_v1::PerfscaleLibrary),
    V2(bindings_v2::PerfscaleLibrary),
    V3(bindings_v3::PerfscaleLibrary),
    V3Config(bindings_v3_config::PerfscaleLibraryConfig),
}

impl Instantiated {
    fn call_init(
        &self,
        store: &mut Store<HostState>,
        config_json: &str,
    ) -> Result<Result<(), String>, wasmtime::Error> {
        match self {
            Instantiated::V1(i) => i.perfscale_library_library().call_init(store, config_json),
            Instantiated::V2(i) => i.perfscale_library_library().call_init(store, config_json),
            Instantiated::V3(i) => i.perfscale_library_library().call_init(store, config_json),
            Instantiated::V3Config(i) => {
                i.perfscale_library_library().call_init(store, config_json)
            }
        }
    }

    /// The optional `config-schema` export (issue #10) — present only on the
    /// 0.3 config world.
    fn call_config_schema(
        &self,
        store: &mut Store<HostState>,
    ) -> Option<Result<String, wasmtime::Error>> {
        match self {
            Instantiated::V3Config(i) => Some(
                i.perfscale_library_config_schema()
                    .call_config_schema(store),
            ),
            _ => None,
        }
    }

    fn call_call(
        &self,
        store: &mut Store<HostState>,
        ctx: &CallCtx<'_>,
        settings_json: String,
        func: &str,
        args_json: &str,
    ) -> Result<Result<String, String>, wasmtime::Error> {
        match self {
            Instantiated::V1(i) => {
                // The 0.1 ABI has no settings field — the guest simply
                // never sees them.
                let _ = settings_json;
                let wctx = bindings_v1::exports::perfscale::library::library::Context {
                    message_seq: ctx.message_seq,
                    iteration_seq: ctx.iteration_seq,
                    vu_id: ctx.vu_id,
                    seed: ctx.seed,
                    time_ms: ctx.time_ms,
                };
                i.perfscale_library_library()
                    .call_call(store, wctx, func, args_json)
            }
            Instantiated::V2(i) => {
                let wctx = bindings_v2::exports::perfscale::library::library::Context {
                    message_seq: ctx.message_seq,
                    iteration_seq: ctx.iteration_seq,
                    vu_id: ctx.vu_id,
                    seed: ctx.seed,
                    time_ms: ctx.time_ms,
                    settings_json,
                };
                i.perfscale_library_library()
                    .call_call(store, &wctx, func, args_json)
            }
            Instantiated::V3(i) => {
                let wctx = bindings_v3::exports::perfscale::library::library::Context {
                    message_seq: ctx.message_seq,
                    iteration_seq: ctx.iteration_seq,
                    vu_id: ctx.vu_id,
                    seed: ctx.seed,
                    time_ms: ctx.time_ms,
                    settings_json,
                };
                i.perfscale_library_library()
                    .call_call(store, &wctx, func, args_json)
            }
            Instantiated::V3Config(i) => {
                let wctx = bindings_v3_config::exports::perfscale::library::library::Context {
                    message_seq: ctx.message_seq,
                    iteration_seq: ctx.iteration_seq,
                    vu_id: ctx.vu_id,
                    seed: ctx.seed,
                    time_ms: ctx.time_ms,
                    settings_json,
                };
                i.perfscale_library_library()
                    .call_call(store, &wctx, func, args_json)
            }
        }
    }
}

/// Parsed `info()` JSON contract (RFC 005: `info` returns JSON — decided).
/// No `deny_unknown_fields`: fields an older engine does not know (like
/// `pure`) are ignored, so new libraries on old engines stay fail-closed
/// (their fs imports then require the grant as before).
#[derive(serde::Deserialize)]
struct WasmLibraryInfoJson {
    name: String,
    #[serde(default)]
    version: String,
    /// The library never touches the filesystem or the wall clock: the host
    /// waives the fs/clock capability requirement (and provides no preopens).
    #[serde(default)]
    pure: bool,
    functions: Vec<WasmFunctionInfoJson>,
}

#[derive(serde::Deserialize)]
struct WasmFunctionInfoJson {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    secret: bool,
}

struct WasmLibraryInfo {
    name: String,
    version: String,
    pure: bool,
    functions: Vec<FunctionInfo>,
}

impl LibraryProvider for WasmLibraryProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn config_schema(&self) -> Option<(&'static str, &Value)> {
        self.config_key.zip(self.config_schema.as_ref())
    }

    fn functions(&self) -> &[FunctionInfo] {
        &self.functions
    }

    fn instantiate(
        &self,
        config: Option<Value>,
        _seed: u64,
    ) -> Result<Box<dyn LibraryInstance>, String> {
        let (mut store, instance) = self.instantiate_store()?;
        // init(config-json) once per instance; failure is fatal to the run
        // (RFC 005 open question — decided). Off the runtime worker thread —
        // the guest may touch granted WASI interfaces (see `off_runtime`).
        let config_json = serde_json::to_string(&config.unwrap_or(Value::Null))
            .map_err(|e| format!("library '{}': {e}", self.id))?; // Value → JSON cannot fail
        off_runtime(|| instance.call_init(&mut store, &config_json))
            .map_err(|e| format!("library '{}': init trapped or exhausted fuel: {e}", self.id))?
            .map_err(|e| format!("library '{}': init failed: {e}", self.id))?;
        Ok(Box::new(WasmLibraryInstance {
            store: Some(store),
            instance: Some(instance),
            args_buf: Vec::new(),
            id: Arc::from(self.id.as_str()),
            pure: self.pure,
        }))
    }

    /// WASM instances are expensive to mint (`Store::new` + WASI linker +
    /// instantiation + `init()`), so one instance per context (VU) is cached
    /// and shared across every generator the VU mints — per-message/connection
    /// instantiation churn disappears. Safe because all per-call inputs
    /// (seed, message_seq) arrive via [`CallCtx`] on every call; nothing a
    /// guest needs per message is frozen at instantiate time.
    fn share_per_context(&self) -> bool {
        true
    }
}

/// One live WASM library instance (per generator owner).
struct WasmLibraryInstance {
    /// `None` only while a call is in flight on the blocking thread.
    store: Option<Store<HostState>>,
    /// From the component's `info()`: pure guests make no blocking WASI
    /// hostcalls and may run inline on a current-thread runtime.
    pure: bool,
    /// The bindgen handles are not `Clone`, so the call path moves the whole
    /// handle to the blocking thread and back (see `call`).
    instance: Option<Instantiated>,
    /// Scratch for the per-call args JSON: the marshal writes into this
    /// reused buffer instead of allocating a fresh `String` per call. On the
    /// blocking-pool path it travels with the store and comes back in the
    /// reply. Empty between calls.
    args_buf: Vec<u8>,
    /// `Arc` so the blocking-pool handoff clones a pointer, not the string.
    id: Arc<str>,
}

impl LibraryInstance for WasmLibraryInstance {
    fn call(&mut self, ctx: &CallCtx<'_>, func: &str, args: &[Value]) -> Result<String, String> {
        let mut args_buf = std::mem::take(&mut self.args_buf);
        args_buf.clear();
        serde_json::to_writer(&mut args_buf, args)
            .map_err(|e| format!("{}.{func}: failed to encode arguments: {e}", self.id))?;
        let handle = match tokio::runtime::Handle::try_current() {
            Err(_) => {
                // No runtime (lint, unit tests, sync embedders): run inline.
                let args_json = std::str::from_utf8(&args_buf).expect("JSON is UTF-8");
                let store = self.store.as_mut().expect("store present between calls");
                let instance = self.instance.as_ref().expect("instance present");
                let result = call_guest(
                    store,
                    instance,
                    &self.id,
                    ctx,
                    ctx.settings_json.to_string(),
                    func,
                    args_json,
                );
                self.args_buf = args_buf;
                return result;
            }
            Ok(h) => h,
        };
        if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
            // The runner's hot path (the CLI builds a multi-thread runtime).
            // `block_in_place` runs the guest call on this worker thread
            // while the scheduler hands the thread's other tasks to a
            // stand-in — no store/instance handoff, no spawn, no channel.
            // Crucially, wasmtime-wasi's sync hostcalls (`Handle::block_on`
            // internally, see `off_runtime`) are legal inside
            // `block_in_place`, so fs/clock guests work too.
            let args_json = std::str::from_utf8(&args_buf).expect("JSON is UTF-8");
            let store = self.store.as_mut().expect("store present between calls");
            let instance = self.instance.as_ref().expect("instance present");
            let id = Arc::clone(&self.id);
            let result = tokio::task::block_in_place(|| {
                call_guest(
                    store,
                    instance,
                    &id,
                    ctx,
                    ctx.settings_json.to_string(),
                    func,
                    args_json,
                )
            });
            self.args_buf = args_buf;
            return result;
        }
        // Single-threaded runtime. `block_in_place` is unavailable there,
        // and `LibraryInstance::call` is a sync trait — the executor thread
        // is busy for the whole guest call no matter where the guest runs.
        // Pure guests make no blocking WASI hostcalls, so nothing can panic
        // on the runtime thread: run inline and skip the hop entirely.
        if self.pure {
            let args_json = std::str::from_utf8(&args_buf).expect("JSON is UTF-8");
            let store = self.store.as_mut().expect("store present between calls");
            let instance = self.instance.as_ref().expect("instance present");
            let result = call_guest(
                store,
                instance,
                &self.id,
                ctx,
                ctx.settings_json.to_string(),
                func,
                args_json,
            );
            self.args_buf = args_buf;
            return result;
        }
        // Capability guests: their sync WASI hostcalls (`in_tokio`) block on
        // a tokio Handle, which panics on a runtime thread — so the call
        // hops to the blocking pool (see `off_runtime`): the store, the
        // instance handle and the args buffer move over and back. One hop
        // per call — microseconds against even a trivial WASM call.
        // `settings_json` is copied exactly once here; the blocking thread
        // hands the owned String straight to the guest context (no second
        // copy).
        let mut store = self.store.take().expect("store present between calls");
        let instance = self.instance.take().expect("instance present");
        let id = Arc::clone(&self.id);
        let settings_json = ctx.settings_json.to_string();
        let func_string = func.to_string();
        let (message_seq, iteration_seq, vu_id, seed, time_ms) = (
            ctx.message_seq,
            ctx.iteration_seq,
            ctx.vu_id,
            ctx.seed,
            ctx.time_ms,
        );
        let (tx, rx) = std::sync::mpsc::channel();
        tokio::task::spawn_blocking(move || {
            let ctx = CallCtx {
                message_seq,
                iteration_seq,
                vu_id,
                seed,
                time_ms,
                settings_json: "",
            };
            let args_json = std::str::from_utf8(&args_buf).expect("JSON is UTF-8");
            let result = call_guest(
                &mut store,
                &instance,
                &id,
                &ctx,
                settings_json,
                &func_string,
                args_json,
            );
            let _ = tx.send((store, instance, args_buf, result));
        });
        match rx.recv() {
            Ok((store, instance, args_buf, result)) => {
                self.store = Some(store);
                self.instance = Some(instance);
                self.args_buf = args_buf;
                result
            }
            // The blocking task panicked or was cancelled before sending;
            // the store is lost with it, so this instance is dead.
            Err(_) => Err(format!(
                "{}.{func}: the library call worker died — this instance is unusable",
                self.id
            )),
        }
    }
}

/// One guest `call` with a fresh fuel budget, mapping the outcome to the
/// engine's error contract: guest error → step failure, trap/fuel → step
/// failure naming the trap.
#[allow(clippy::too_many_arguments)]
fn call_guest(
    store: &mut Store<HostState>,
    instance: &Instantiated,
    id: &str,
    ctx: &CallCtx<'_>,
    settings_json: String,
    func: &str,
    args_json: &str,
) -> Result<String, String> {
    // Fresh fuel budget per call (the store's fuel is cumulative). This is a
    // single store-field write — already the cheapest reset mechanism; the
    // store is deliberately NOT recreated per call.
    store
        .set_fuel(PER_CALL_FUEL)
        .map_err(|e| format!("{id}.{func}: {e}"))?;
    match instance.call_call(store, ctx, settings_json, func, args_json) {
        Ok(Ok(value)) => Ok(value),
        // Guest-level error (bad function/args): the step fails with it.
        Ok(Err(e)) => Err(format!("{id}.{func}: {e}")),
        // Trap or fuel exhaustion.
        Err(e) => Err(format!(
            "{id}.{func}: the WASM guest trapped or exhausted its fuel budget: {e}"
        )),
    }
}

/// Run one guest call off the async runtime's worker threads.
///
/// wasmtime-wasi's *sync* hostcalls (`in_tokio`, `runtime.rs`) block on a
/// tokio `Handle` internally, which panics on a runtime worker thread
/// ("Cannot start a runtime from within a runtime") — so any library call
/// touching a granted WASI interface (fs reads, clocks) from the async
/// runner died at the first hostcall. On a thread with no runtime (sync
/// callers: lint, unit tests, embedders) the call runs inline. The hot path
/// ([`WasmLibraryInstance::call`]) avoids this thread on multi-thread
/// runtimes (`block_in_place`, where `Handle::block_on` is legal) and uses
/// the blocking pool otherwise; this scoped thread serves the cold paths
/// (per-run info probe, per-instance init), where a thread spawn is noise.
pub(crate) fn off_runtime<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    if tokio::runtime::Handle::try_current().is_err() {
        return f();
    }
    std::thread::scope(|s| match s.spawn(f).join() {
        Ok(r) => r,
        Err(payload) => std::panic::resume_unwind(payload),
    })
}

fn link_err(file: &str, e: wasmtime::Error) -> String {
    format!("library '{file}': failed to connect a granted capability: {e}")
}

/// Parse the YAML grant — shared by the file and embedded load paths.
fn check_grants(display: &str, capabilities: Option<&[Capability]>) -> Result<Grants, String> {
    Grants::parse(capabilities.unwrap_or(&[])).map_err(|e| format!("library '{display}': {e}"))
}

/// Build the per-provider linker: wasi:io plumbing and wasi:cli in sink form
/// (Rust-std noise; the sinks expose nothing of the host), plus wasi:clocks
/// and wasi:filesystem unconditionally — jco/StarlingMonkey-built components
/// import fs/wall-clock even when pure, and linking them lets the engine
/// instantiate such components and probe `info()`. The interfaces alone grant
/// nothing: without an `fs` grant the WASI context carries zero preopens, so
/// any actual file access fails at runtime; the wall clock is data, not
/// authority. The fail-closed boundary is the grant-gated preopen in
/// [`WasmLibraryProvider::instantiate_store`], not the link set.
///
/// `wasi:http` is the exception: connected only when `net_granted` (a
/// net-importing component without a grant is already a hard load error, and
/// leaving the interfaces out removes the runtime boundary question
/// entirely). When linked, [`net_allowlist::NetHooks`] mediates every
/// request.
pub(crate) fn build_linker(
    engine: &Engine,
    display: &str,
    net_granted: bool,
) -> Result<Linker<HostState>, String> {
    use wasmtime_wasi::cli::{WasiCli, WasiCliView};

    let mut linker = Linker::<HostState>::new(engine);

    // wasi:io plumbing (streams/poll/error) — transitively imported by cli,
    // filesystem and clock guests.
    let l = &mut linker;
    wasmtime_wasi::p2::bindings::sync::io::error::add_to_linker::<HostState, HasIo>(l, |t| {
        t.ctx().table
    })
    .map_err(|e| link_err(display, e))?;
    wasi_bindings::sync::io::streams::add_to_linker::<HostState, HasIo>(l, |t| t.ctx().table)
        .map_err(|e| link_err(display, e))?;
    wasi_bindings::sync::io::poll::add_to_linker::<HostState, HasIo>(l, |t| t.ctx().table)
        .map_err(|e| link_err(display, e))?;

    // wasi:cli in sink form: every rustc-built wasip2 component imports
    // these. The WasiCtx they talk to is configured with empty args/env,
    // closed stdin and discarded stdout/stderr; `exit` just traps.
    wasi_bindings::cli::exit::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(|e| link_err(display, e))?;
    wasi_bindings::cli::environment::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(|e| link_err(display, e))?;
    wasi_bindings::cli::stdin::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(|e| link_err(display, e))?;
    wasi_bindings::cli::stdout::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(|e| link_err(display, e))?;
    wasi_bindings::cli::stderr::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(|e| link_err(display, e))?;
    wasi_bindings::cli::terminal_input::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(|e| link_err(display, e))?;
    wasi_bindings::cli::terminal_output::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(|e| link_err(display, e))?;
    wasi_bindings::cli::terminal_stdin::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(|e| link_err(display, e))?;
    wasi_bindings::cli::terminal_stdout::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(|e| link_err(display, e))?;
    wasi_bindings::cli::terminal_stderr::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(|e| link_err(display, e))?;

    // The wall clock is unconditional: std's fs-metadata code imports it
    // alongside the filesystem, and jco-built components import it outright.
    use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView};
    wasi_bindings::clocks::wall_clock::add_to_linker::<HostState, WasiClocks>(l, HostState::clocks)
        .map_err(|e| link_err(display, e))?;
    // The monotonic clock is unconditional too: every rustc-built wasip2
    // component imports it (std's Instant/parker), so it cannot be a
    // meaningful grant for Rust guests.
    wasi_bindings::clocks::monotonic_clock::add_to_linker::<HostState, WasiClocks>(
        l,
        HostState::clocks,
    )
    .map_err(|e| link_err(display, e))?;
    // wasi:random/insecure-seed, sunk at a fixed value (see import_capability
    // and instantiate_store): std's HashMap imports it on wasip2. The real
    // entropy interfaces (random/random, random/insecure) are never linked.
    use wasmtime_wasi::random::WasiRandom;
    wasi_bindings::random::insecure_seed::add_to_linker::<HostState, WasiRandom>(l, |t| {
        t.ctx().ctx.random()
    })
    .map_err(|e| link_err(display, e))?;
    // The filesystem interfaces are linked for every component (see the
    // doc comment): with no preopens in the WASI context they are inert.
    use wasmtime_wasi::filesystem::{WasiFilesystem, WasiFilesystemView};
    wasi_bindings::filesystem::preopens::add_to_linker::<HostState, WasiFilesystem>(
        l,
        HostState::filesystem,
    )
    .map_err(|e| link_err(display, e))?;
    wasi_bindings::sync::filesystem::types::add_to_linker::<HostState, WasiFilesystem>(
        l,
        HostState::filesystem,
    )
    .map_err(|e| link_err(display, e))?;

    // wasi:http only when the YAML grants net: the hooks (allowlist, timeout,
    // body caps) live on HostState and mediate every request the guest sends.
    if net_granted {
        add_only_http_to_linker_sync(l).map_err(|e| link_err(display, e))?;
    }
    Ok(linker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{validate_libraries, CallCtx, Capability, LibraryRef};
    use serde_json::Value;

    // --- fixtures: real components built from the SDK examples --------------

    struct Fixtures {
        hello: PathBuf,
        hello01: PathBuf,
        configlib: PathBuf,
        spin: PathBuf,
        fsreader: PathBuf,
        purefs: PathBuf,
        memhog: PathBuf,
        netfetch: PathBuf,
    }

    fn wasip2_installed() -> bool {
        std::process::Command::new("rustup")
            .args(["target", "list", "--installed"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|l| l.trim() == "wasm32-wasip2")
            })
            .unwrap_or(false)
    }

    /// Build the SDK example components once per test binary. Returns `None`
    /// (skipping, with an eprintln) when the wasm32-wasip2 target is missing.
    fn fixtures() -> Option<&'static Fixtures> {
        static FIXTURES: OnceLock<Option<Fixtures>> = OnceLock::new();
        FIXTURES
            .get_or_init(|| {
                if !wasip2_installed() {
                    eprintln!(
                        "skipping WASM library tests: wasm32-wasip2 target not installed \
                         (rustup target add wasm32-wasip2)"
                    );
                    return None;
                }
                let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .canonicalize()
                    .unwrap();
                let target = ws.join("target/wasm-libs-fixtures");
                let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
                let build = |name: &str, artifact: &str| -> Option<PathBuf> {
                    let manifest = ws.join(format!(
                        "crates/perfscale-library-sdk/examples/{name}/Cargo.toml"
                    ));
                    let status = std::process::Command::new(&cargo)
                        .args(["build", "--release", "--target", "wasm32-wasip2"])
                        .arg("--manifest-path")
                        .arg(&manifest)
                        .arg("--target-dir")
                        .arg(&target)
                        .status()
                        .ok()?;
                    if !status.success() {
                        eprintln!("failed to build the {name} fixture component");
                        return None;
                    }
                    Some(target.join(format!("wasm32-wasip2/release/{artifact}.wasm")))
                };
                Some(Fixtures {
                    hello: build("hello", "perfscale_hello_library")?,
                    hello01: build("hello01", "perfscale_hello01_library")?,
                    configlib: build("configlib", "perfscale_configlib_example")?,
                    spin: build("spin", "perfscale_spin_library")?,
                    fsreader: build("fsreader", "perfscale_fsreader_library")?,
                    purefs: build("purefs", "perfscale_purefs_library")?,
                    memhog: build("memhog", "perfscale_memhog_library")?,
                    netfetch: build("netfetch", "perfscale_netfetch_library")?,
                })
            })
            .as_ref()
    }

    fn lib_ref(path: &Path) -> LibraryRef {
        LibraryRef {
            use_: path.to_string_lossy().into_owned(),
            sha256: None,
            r#as: Some("hello".into()),
            capabilities: Some(vec![]),
            with: None,
            secret: None,
            allow: None,
            deny: None,
            log: None,
        }
    }

    fn ctx() -> CallCtx<'static> {
        CallCtx {
            message_seq: 1,
            iteration_seq: 0,
            vu_id: 1,
            seed: 42,
            time_ms: 1_784_160_000_000,
            settings_json: "{}",
        }
    }

    // --- pure checks (no fixtures needed) -----------------------------------

    #[test]
    fn import_capability_mapping() {
        assert_eq!(
            import_capability("wasi:filesystem/types@0.2.9").unwrap(),
            Some("fs")
        );
        assert_eq!(import_capability("wasi:io/streams@0.2.0").unwrap(), None);
        assert_eq!(
            import_capability("wasi:clocks/monotonic-clock@0.2.0").unwrap(),
            None,
            "monotonic clock is always-connected Rust runtime infrastructure"
        );
        assert_eq!(
            import_capability("wasi:clocks/wall-clock@0.2.0").unwrap(),
            Some("clock")
        );
        assert_eq!(
            import_capability("wasi:http/outgoing-handler@0.2.0").unwrap(),
            Some("net")
        );
        assert_eq!(
            import_capability("wasi:random/insecure-seed@0.2.12").unwrap(),
            None,
            "insecure-seed is toolchain noise (std HashMap), sunk at a fixed value"
        );
        assert!(import_capability("wasi:random/random@0.2.0").is_err());
        assert!(import_capability("wasi:random/insecure@0.2.0").is_err());
        assert!(import_capability("wasi:sockets/tcp@0.2.0").is_err());
        assert!(import_capability("acme:custom/thing").is_err());
    }

    #[test]
    fn missing_file_and_wrong_extension_are_clear_errors() {
        let mut r = lib_ref(Path::new("/definitely/missing.wasm"));
        let err = validate_libraries(&[r.clone()], false, None).unwrap_err();
        assert!(err.contains("file not found"), "{err}");
        let dir = tempfile::tempdir().unwrap();
        let notwasm = dir.path().join("lib.txt");
        std::fs::write(&notwasm, b"nope").unwrap();
        r.use_ = notwasm.to_string_lossy().into_owned();
        let err = validate_libraries(&[r], false, None).unwrap_err();
        assert!(err.contains("expected a .wasm"), "{err}");
    }

    // --- fixture-backed tests ------------------------------------------------

    #[test]
    fn loads_and_calls_through_the_component_abi() {
        let Some(f) = fixtures() else { return };
        let resolved = validate_libraries(&[lib_ref(&f.hello)], false, None).unwrap();
        assert_eq!(resolved[0].provider.id(), "perfscale_hello_library@v0.1.0");
        let names: Vec<_> = resolved[0]
            .provider
            .functions()
            .iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["greet", "token", "settings"]);

        let mut inst = resolved[0].provider.instantiate(None, 42).unwrap();
        let v = inst.call(&ctx(), "greet", &[Value::from("world")]).unwrap();
        assert_eq!(v, "hello, world!");
    }

    #[test]
    fn with_config_reaches_init_and_unknown_functions_fail() {
        let Some(f) = fixtures() else { return };
        let mut r = lib_ref(&f.hello);
        r.with = Some(serde_json::json!({ "greeting": "hi" }));
        let resolved = validate_libraries(&[r], false, None).unwrap();
        let mut inst = resolved[0]
            .provider
            .instantiate(resolved[0].config.clone(), 7)
            .unwrap();
        assert_eq!(
            inst.call(&ctx(), "greet", &[Value::from("there")]).unwrap(),
            "hi, there!",
            "the `with:` block must reach the component's init()"
        );
        let err = inst.call(&ctx(), "nope", &[]).unwrap_err();
        assert!(err.contains("unknown function"), "{err}");
    }

    #[test]
    fn memo_key_reuses_within_one_message_seq() {
        let Some(f) = fixtures() else { return };
        let resolved = validate_libraries(&[lib_ref(&f.hello)], false, None).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 7).unwrap();
        let key = [Value::from("order")];
        let a = inst.call(&ctx(), "token", &key).unwrap();
        assert_eq!(inst.call(&ctx(), "token", &key).unwrap(), a, "same seq");
        let mut c2 = ctx();
        c2.message_seq = 2;
        assert_ne!(inst.call(&c2, "token", &key).unwrap(), a, "next seq fresh");
        // A deterministic seed reproduces the token across instances.
        let mut inst2 = resolved[0].provider.instantiate(None, 7).unwrap();
        assert_eq!(inst2.call(&ctx(), "token", &key).unwrap(), a);
    }

    #[test]
    fn expansion_through_the_generator() {
        let Some(f) = fixtures() else { return };
        let resolved = validate_libraries(&[lib_ref(&f.hello)], false, None).unwrap();
        let mut gen = crate::generate::Gen::new(42);
        gen.attach_library(
            resolved[0].alias.clone(),
            resolved[0].provider.instantiate(None, 42).unwrap(),
            resolved[0].rules.clone(),
            resolved[0].provider.functions().to_vec(),
        );
        gen.begin_message();
        assert_eq!(
            gen.expand("${hello.greet(world)}").unwrap(),
            "hello, world!"
        );
        let err = gen.expand("${hello.nope()}").unwrap_err();
        assert!(err.contains("unknown function"), "{err}");
    }

    #[test]
    fn wit_0_1_component_loads_without_settings() {
        let Some(f) = fixtures() else { return };
        // The legacy fixture exports `perfscale:library/library@0.1.0`; the
        // host instantiates it through the 0.1 bindings (no settings field).
        let resolved = validate_libraries(&[lib_ref(&f.hello01)], false, None).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        let v = inst.call(&ctx(), "greet", &[Value::from("world")]).unwrap();
        assert_eq!(v, "hello, world! (vu 1, seq 1)");
        assert!(
            resolved[0]
                .provider
                .functions()
                .iter()
                .any(|fi| fi.name == "token" && fi.secret),
            "info() metadata (secret flags) parses for 0.1 components too"
        );
    }

    // --- config-schema (issue #10, WIT 0.3) --------------------------------

    #[test]
    fn config_schema_is_registered_and_served() {
        let Some(f) = fixtures() else { return };
        let mut r = lib_ref(&f.configlib);
        r.r#as = Some("configlib".into());
        let resolved = validate_libraries(&[r], false, None).unwrap();
        let (key, schema) = resolved[0]
            .provider
            .config_schema()
            .expect("configlib declares a config schema");
        assert_eq!(key, "configlib");
        assert_eq!(schema["required"], serde_json::json!(["endpoint"]));

        // The validated block reaches the guest through settings.extensions.
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        let mut c = ctx();
        c.settings_json =
            r#"{"extensions":{"configlib":{"endpoint":"https://api.test","pool_size":4}}}"#;
        let v = inst.call(&c, "endpoint", &[]).unwrap();
        assert_eq!(v, "https://api.test");
        // Absent block → the guest's own default.
        let v = inst.call(&ctx(), "endpoint", &[]).unwrap();
        assert_eq!(v, "https://localhost");
    }

    #[test]
    fn config_key_collision_between_two_libraries_is_a_hard_error() {
        let Some(f) = fixtures() else { return };
        let mut a = lib_ref(&f.configlib);
        a.r#as = Some("one".into());
        let mut b = lib_ref(&f.configlib);
        b.r#as = Some("two".into());
        let err = validate_libraries(&[a, b], false, None).unwrap_err();
        assert!(err.contains("claimed by both"), "{err}");
    }

    #[test]
    fn legacy_components_have_no_config_schema() {
        let Some(f) = fixtures() else { return };
        // hello (rebuilt against the current ABI) and hello01 (0.1) declare
        // no config extension.
        for path in [&f.hello, &f.hello01] {
            let resolved = validate_libraries(&[lib_ref(path)], false, None).unwrap();
            assert!(resolved[0].provider.config_schema().is_none());
        }
    }

    #[test]
    fn config_extensions_validate_against_the_claiming_library() {
        let Some(f) = fixtures() else { return };
        let mut r = lib_ref(&f.configlib);
        r.r#as = Some("configlib".into());
        let resolved = validate_libraries(&[r], false, None).unwrap();

        // Unknown key — nothing claims it (this is also the #8 typo guard's
        // new home: unknown top-level keys fail here, not at schema parse).
        let ext = serde_json::json!({ "iterations": 1 })
            .as_object()
            .unwrap()
            .clone();
        let err = crate::library::validate_config_extensions(&ext, &resolved).unwrap_err();
        assert!(err.contains("unknown config key 'iterations'"), "{err}");

        // Valid block passes and is returned for the settings.
        let ext = serde_json::json!({ "configlib": { "endpoint": "https://x", "pool_size": 2 } })
            .as_object()
            .unwrap()
            .clone();
        let validated = crate::library::validate_config_extensions(&ext, &resolved).unwrap();
        assert_eq!(validated["configlib"]["pool_size"], serde_json::json!(2));

        // Schema violation: missing required endpoint; pool_size below min.
        for bad in [
            serde_json::json!({ "configlib": { "pool_size": 2 } }),
            serde_json::json!({ "configlib": { "endpoint": "https://x", "pool_size": 0 } }),
        ] {
            let ext = bad.as_object().unwrap().clone();
            let err = crate::library::validate_config_extensions(&ext, &resolved).unwrap_err();
            assert!(err.contains("does not match"), "{err}");
        }
    }

    #[test]
    fn wit_0_2_component_sees_the_run_settings() {
        let Some(f) = fixtures() else { return };
        let resolved = validate_libraries(&[lib_ref(&f.hello)], false, None).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        let mut c = ctx();
        c.settings_json = r#"{"vus":10,"seed":42}"#;
        let v = inst.call(&c, "settings", &[]).unwrap();
        assert_eq!(v, r#"{"vus":10,"seed":42}"#);
    }

    /// The runner's hot path: calls from a multi-thread runtime worker run
    /// via `block_in_place`. wasmtime-wasi's sync hostcalls block on a tokio
    /// `Handle` internally, which is only legal there — an fs-granted guest
    /// proves the whole chain works (issue #4).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_thread_runtime_calls_work_in_place() {
        let Some(f) = fixtures() else { return };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("corpus.txt"), "corpus-data\n").unwrap();
        let mut r = lib_ref(&f.fsreader);
        r.r#as = Some("corpus".into());
        r.capabilities = Some(vec![Capability::Simple("fs".into())]);
        let resolved = validate_libraries(&[r], true, Some(dir.path())).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        // A WASI-touching call (fs preopen read) and a fuel trip, both from
        // runtime worker threads.
        let v = tokio::task::spawn(async move {
            inst.call(&ctx(), "read", &[Value::from("/corpus.txt")])
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(v, "corpus-data");

        let resolved = validate_libraries(&[lib_ref(&f.spin)], false, None).unwrap();
        let mut spin = resolved[0].provider.instantiate(None, 1).unwrap();
        let err = tokio::task::spawn(async move { spin.call(&ctx(), "spin", &[]).unwrap_err() })
            .await
            .unwrap();
        assert!(err.contains("fuel"), "{err}");
    }

    /// Single-threaded runtimes keep the blocking-pool hop (block_in_place
    /// is unavailable there).
    #[tokio::test(flavor = "current_thread")]
    async fn current_thread_runtime_calls_hop_through_the_blocking_pool() {
        let Some(f) = fixtures() else { return };
        let resolved = validate_libraries(&[lib_ref(&f.hello)], false, None).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        let v = tokio::task::spawn(async move {
            inst.call(&ctx(), "greet", &[Value::from("world")]).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(v, "hello, world!");
    }

    #[test]
    fn fs_import_without_grant_is_a_hard_load_error() {
        let Some(f) = fixtures() else { return };
        let r = lib_ref(&f.fsreader);
        let err = validate_libraries(&[r], true, None).unwrap_err();
        assert!(
            err.contains("fs") && err.contains("does not grant"),
            "{err}"
        );
    }

    #[test]
    fn fs_grant_reads_through_the_confined_preopen() {
        let Some(f) = fixtures() else { return };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("corpus.txt"), "  corpus-data\n").unwrap();
        let mut r = lib_ref(&f.fsreader);
        r.r#as = Some("corpus".into());
        r.capabilities = Some(vec![Capability::Simple("fs".into())]);
        let resolved = validate_libraries(&[r], true, Some(dir.path())).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        let v = inst
            .call(&ctx(), "read", &[Value::from("/corpus.txt")])
            .unwrap();
        assert_eq!(v, "corpus-data");
        // No escape from the preopen.
        assert!(inst
            .call(&ctx(), "read", &[Value::from("/../etc/passwd")])
            .is_err());
    }

    /// A different grant does not satisfy an fs import: fail-closed compares
    /// per capability, not "any grant present".
    #[test]
    fn fs_import_with_only_a_clock_grant_is_a_hard_error() {
        let Some(f) = fixtures() else { return };
        let mut r = lib_ref(&f.fsreader);
        r.capabilities = Some(vec![Capability::Simple("clock".into())]);
        let err = validate_libraries(&[r], true, None).unwrap_err();
        assert!(err.contains("needed: fs"), "{err}");
        assert!(err.contains("granted: clock"), "{err}");
    }

    /// The pure marker (RFC 005): a component that imports wasi:filesystem
    /// (jco/StarlingMonkey toolchain noise) but whose info() declares
    /// `"pure": true` loads WITHOUT an fs grant — and without
    /// allow_library_capabilities, since no capabilities are declared. The
    /// sandbox holds at runtime: with no grant there are no preopens, so an
    /// actual file read fails.
    #[test]
    fn pure_library_with_fs_imports_loads_without_a_grant() {
        let Some(f) = fixtures() else { return };
        // Guard the premise: the fixture really does import the fs interfaces
        // (if the toolchain ever stops, this test must not silently vacate).
        let bytes = std::fs::read(&f.purefs).unwrap();
        let component = Component::new(engine().unwrap(), &bytes).unwrap();
        let imports: Vec<String> = component
            .component_type()
            .imports(engine().unwrap())
            .map(|(n, _)| n.to_string())
            .collect();
        assert!(
            imports.iter().any(|n| n.starts_with("wasi:filesystem/")),
            "the purefs fixture must import wasi:filesystem/*: {imports:?}"
        );

        // No capabilities, gate off — the pure marker waives the fs import.
        let resolved = validate_libraries(&[lib_ref(&f.purefs)], false, None).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        // Non-fs calls work fine.
        assert_eq!(
            inst.call(&ctx(), "echo", &[Value::from("hi")]).unwrap(),
            "hi"
        );
        // An actual file access fails at runtime: interfaces linked, zero
        // preopens.
        let err = inst
            .call(&ctx(), "read", &[Value::from("/etc/hostname")])
            .unwrap_err();
        assert!(!err.is_empty(), "read without a preopen must fail");
    }

    /// A pure library that IS granted fs keeps the confined preopen — the
    /// marker waives the requirement, it does not disable the grant.
    #[test]
    fn pure_library_with_an_fs_grant_reads_through_the_preopen() {
        let Some(f) = fixtures() else { return };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("corpus.txt"), "  corpus-data\n").unwrap();
        let mut r = lib_ref(&f.purefs);
        r.r#as = Some("corpus".into());
        r.capabilities = Some(vec![Capability::Simple("fs".into())]);
        let resolved = validate_libraries(&[r], true, Some(dir.path())).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        let v = inst
            .call(&ctx(), "read", &[Value::from("/corpus.txt")])
            .unwrap();
        assert_eq!(v, "corpus-data");
    }

    /// Fail-closed stays the default: a component importing fs whose info()
    /// does NOT declare pure still hits the hard load error without a grant
    /// (the pre-existing fsreader tests assert the same for the SDK-built
    /// non-pure fixture).
    #[test]
    fn non_pure_library_without_a_grant_still_fails_closed() {
        let Some(f) = fixtures() else { return };
        let err = validate_libraries(&[lib_ref(&f.fsreader)], true, None).unwrap_err();
        assert!(
            err.contains("fs") && err.contains("does not grant"),
            "{err}"
        );
    }

    /// A symlink inside fs_root pointing outside it must not be followed:
    /// the preopen confines by capability (cap-std), not by path string.
    #[test]
    fn fs_preopen_refuses_symlink_escape() {
        let Some(f) = fixtures() else { return };
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "outside-data").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();
        #[cfg(not(unix))]
        {
            eprintln!("skipping symlink check on this platform");
            return;
        }
        let mut r = lib_ref(&f.fsreader);
        r.r#as = Some("corpus".into());
        r.capabilities = Some(vec![Capability::Simple("fs".into())]);
        let resolved = validate_libraries(&[r], true, Some(dir.path())).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        let err = inst
            .call(&ctx(), "read", &[Value::from("/link.txt")])
            .unwrap_err();
        assert!(!err.contains("outside-data"), "symlink escape read: {err}");
    }

    /// The per-instance memory cap: a guest allocating past 64 MiB traps
    /// (store limiter refuses the growth); the failed instance is dead but
    /// the provider still mints working instances.
    #[test]
    fn memory_cap_traps_growth_past_the_limit() {
        let Some(f) = fixtures() else { return };
        let mut r = lib_ref(&f.memhog);
        r.r#as = Some("mem".into());
        let resolved = validate_libraries(&[r], false, None).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        // Well under the cap: fine.
        let v = inst.call(&ctx(), "grow", &[Value::from(1)]).unwrap();
        assert!(v.contains("grew 1 MiB"), "{v}");
        // Past the 64 MiB cap: trap, not silent success and not a host OOM.
        let err = inst.call(&ctx(), "grow", &[Value::from(128)]).unwrap_err();
        assert!(err.contains("trapped") || err.contains("memory"), "{err}");
        // A fresh instance of the same provider still works.
        let mut fresh = resolved[0].provider.instantiate(None, 1).unwrap();
        let v = fresh.call(&ctx(), "grow", &[Value::from(1)]).unwrap();
        assert!(v.contains("grew 1 MiB"), "{v}");
    }

    /// Load-time size cap: an oversized local component is rejected before
    /// compilation (compile memory scales with module size; the cap matches
    /// the 64 MiB fetch cap `perfscale install` enforces).
    #[test]
    fn oversized_component_is_rejected_before_compilation() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.wasm");
        std::fs::write(&big, vec![0u8; MAX_COMPONENT_BYTES + 1]).unwrap();
        let err = validate_libraries(&[lib_ref(&big)], true, None).unwrap_err();
        assert!(err.contains("64 MiB"), "{err}");
    }

    // --- net capability (wasi:http, host allowlist) -------------------------

    /// A one-shot loopback HTTP server answering a canned response; returns
    /// the bound port. Runs on the test's tokio runtime (the previous
    /// std::net thread with a 5s blocking read timeout was the flake source:
    /// under CI load the starved runtime could deliver the host-side hyper
    /// request later than that, the server's `read().unwrap()` panicked, and
    /// the dropped socket surfaced at the guest as "connection reset").
    /// Timeouts here stay under the host's 30s request budget; every error
    /// just ends that connection — a test server must never panic.
    fn canned_http_server(body: &'static str) -> u16 {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = std_listener.local_addr().unwrap().port();
        std_listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut conn, _)) = listener.accept().await {
                tokio::spawn(async move {
                    // Read the request head (the fixture sends a bodyless GET).
                    let mut buf = [0u8; 4096];
                    let mut read = 0;
                    let head = tokio::time::timeout(Duration::from_secs(25), async {
                        while !buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                            match conn.read(&mut buf[read..]).await {
                                Ok(0) | Err(_) => return false,
                                Ok(n) => read += n,
                            }
                        }
                        true
                    })
                    .await;
                    if head != Ok(true) {
                        return;
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    if conn.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                    // Close gracefully: half-close the write side, then drain
                    // whatever the client still sends (chunked terminators,
                    // pipelined bytes) until it hangs up. Dropping the socket
                    // with unread inbound data sends RST, and the guest then
                    // reports "connection reset" instead of reading the body.
                    if conn.shutdown().await.is_err() {
                        return;
                    }
                    let mut sink = [0u8; 4096];
                    let _ = tokio::time::timeout(Duration::from_secs(25), async {
                        loop {
                            match conn.read(&mut sink).await {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    })
                    .await;
                });
            }
        });
        port
    }

    /// net-granted library fetches from an allowlisted host (the grant pins
    /// the exact loopback port), through the runner's multi-thread hot path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_grant_fetches_from_an_allowlisted_host() {
        let Some(f) = fixtures() else { return };
        let mut r = lib_ref(&f.netfetch);
        r.r#as = Some("net".into());

        // The exchange parks the guest on a blocking-pool thread while
        // wasmtime-wasi-http drives its spawned send/IO tasks on this
        // 2-worker runtime. Under full-suite CI load (2 vCPUs, ~900 tests)
        // that runtime starves long enough for the connection to die with a
        // spurious "connection reset" (CI runs 38750639062 and 38755874228,
        // both ubuntu; never reproduced locally, even CPU-saturated — 85/85).
        // What's under test is the allowlist semantics, not the kernel's
        // patience, so that one flake class retries with a fresh server; any
        // other error (a real regression, e.g. a refusal) fails immediately.
        let mut last_reset = None;
        for attempt in 1..=3 {
            let port = canned_http_server("corpus-over-http\n");
            let mut grant = r.clone();
            grant.capabilities = Some(vec![Capability::Net {
                net: vec![format!("127.0.0.1:{port}")],
            }]);
            let resolved = validate_libraries(&[grant], true, None).unwrap();
            let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
            let url = format!("http://127.0.0.1:{port}/corpus");
            // spawn_blocking, not spawn: the guest call blocks its thread for
            // the whole fetch. On a worker thread it can starve the runtime
            // that drives the host-side hyper socket (wasi-http sync bindings
            // block on the ambient runtime) — under CI load that surfaced as
            // a flaky "connection reset". The blocking pool keeps the runtime
            // free.
            let result =
                tokio::task::spawn_blocking(move || inst.call(&ctx(), "get", &[Value::from(url)]))
                    .await
                    .unwrap();
            match result {
                Ok(v) => {
                    assert_eq!(v, "corpus-over-http");
                    return;
                }
                Err(e) if e.contains("connection reset") => {
                    eprintln!("attempt {attempt}: flaky connection reset, retrying");
                    last_reset = Some(e);
                }
                Err(e) => panic!("unexpected fetch error: {e}"),
            }
        }
        panic!("3 attempts all hit the CI flake, last: {last_reset:?}");
    }

    /// A destination outside the allowlist is refused by the host's
    /// send-request hook, and the error names the destination.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_refuses_a_host_outside_the_allowlist() {
        let Some(f) = fixtures() else { return };
        let port = canned_http_server("nope");
        let mut r = lib_ref(&f.netfetch);
        r.r#as = Some("net".into());
        // The grant pins a different port than the server listens on.
        r.capabilities = Some(vec![Capability::Net {
            net: vec!["127.0.0.1:1".into()],
        }]);
        let resolved = validate_libraries(&[r], true, None).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        let url = format!("http://127.0.0.1:{port}/corpus");
        let err = tokio::task::spawn(async move {
            inst.call(&ctx(), "get", &[Value::from(url)]).unwrap_err()
        })
        .await
        .unwrap();
        assert!(
            err.contains("not allowlisted") && err.contains(&format!("127.0.0.1:{port}")),
            "the refusal must name the destination: {err}"
        );
    }

    /// A component importing wasi:http WITHOUT a net grant is a hard load
    /// error (fail-closed: imports declare need, YAML grants).
    #[test]
    fn net_import_without_grant_is_a_hard_load_error() {
        let Some(f) = fixtures() else { return };
        let err = validate_libraries(&[lib_ref(&f.netfetch)], true, None).unwrap_err();
        assert!(
            err.contains("needed: net") && err.contains("does not grant"),
            "{err}"
        );
    }

    // --- net allowlist matcher (pure unit tests, no fixtures) ---------------

    use super::net_allowlist::{allowed, AllowEntry};

    fn entries(raw: &[&str]) -> Vec<AllowEntry> {
        raw.iter().map(|e| AllowEntry::parse(e).unwrap()).collect()
    }

    #[test]
    fn net_allowlist_exact_hosts_and_default_ports() {
        let e = entries(&["api.acme.com"]);
        assert!(allowed(&e, "api.acme.com", 443));
        assert!(allowed(&e, "api.acme.com", 8443), "no port pin = any port");
        // Case-insensitive, trailing dot tolerated.
        assert!(allowed(&e, "API.Acme.COM.", 443));
        // Not subdomains, not siblings.
        assert!(!allowed(&e, "www.api.acme.com", 443));
        assert!(!allowed(&e, "acme.com", 443));
        assert!(!allowed(&e, "api.acme.com.evil.net", 443));
    }

    #[test]
    fn net_allowlist_port_pinning() {
        let e = entries(&["api.acme.com:8443"]);
        assert!(allowed(&e, "api.acme.com", 8443));
        assert!(!allowed(&e, "api.acme.com", 443));
        assert!(!allowed(&e, "api.acme.com", 80));
    }

    #[test]
    fn net_allowlist_suffix_wildcards() {
        let e = entries(&["*.internal"]);
        assert!(allowed(&e, "db.internal", 5432));
        assert!(allowed(&e, "a.b.internal", 443));
        // The bare suffix and lookalikes stay out.
        assert!(!allowed(&e, "internal", 443));
        assert!(!allowed(&e, "foo.internal.evil.net", 443));
        assert!(!allowed(&e, "foointernal", 443));
        // Wildcard with a pinned port.
        let e = entries(&["*.internal:8080"]);
        assert!(allowed(&e, "db.internal", 8080));
        assert!(!allowed(&e, "db.internal", 443));
    }

    #[test]
    fn net_allowlist_rejects_malformed_entries() {
        for bad in [
            "",
            ":",
            "host:",
            "host:abc",
            "host:99999",
            "*",
            "*.",
            "*internal",
            "a*b.com",
        ] {
            assert!(AllowEntry::parse(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn grants_require_the_global_gate() {
        let Some(f) = fixtures() else { return };
        let mut r = lib_ref(&f.hello);
        r.capabilities = Some(vec![Capability::Simple("clock".into())]);
        let err = validate_libraries(&[r], false, None).unwrap_err();
        assert!(err.contains("allow_library_capabilities"), "{err}");
    }

    #[test]
    fn infinite_loop_trips_the_fuel_budget() {
        let Some(f) = fixtures() else { return };
        let mut r = lib_ref(&f.spin);
        r.r#as = Some("spin".into());
        let resolved = validate_libraries(&[r], false, None).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        let err = inst.call(&ctx(), "spin", &[]).unwrap_err();
        assert!(err.contains("fuel"), "{err}");
        // A trapped component instance is dead (wasmtime refuses re-entry);
        // a fresh instance of the same provider still works.
        let err = inst.call(&ctx(), "spin", &[]).unwrap_err();
        assert!(err.contains("cannot enter component instance"), "{err}");
        let mut fresh = resolved[0].provider.instantiate(None, 1).unwrap();
        let err = fresh.call(&ctx(), "other", &[]).unwrap_err();
        assert!(err.contains("unknown function"), "{err}");
    }

    // --- burn cache (RFC 005 burn) -------------------------------------------

    /// A provider built from a burn artifact produces bit-identical values
    /// to one compiled from source with the same seed.
    #[test]
    fn burned_load_matches_fresh_compile() {
        let Some(f) = fixtures() else { return };
        let bytes = std::fs::read(&f.hello).unwrap();
        let sha = super::super::burn::sha256_bytes(&bytes);
        let artifact = super::super::burn::burn_component(&bytes).unwrap();
        let component = super::super::burn::load_burned(&artifact, &sha)
            .expect("the burn artifact deserializes");
        let burned =
            WasmLibraryProvider::load_embedded(&f.hello.to_string_lossy(), component, None, None)
                .unwrap();
        let resolved = validate_libraries(&[lib_ref(&f.hello)], false, None).unwrap();

        let args = [Value::from("order")];
        let mut a = burned.instantiate(None, 7).unwrap();
        let mut b = resolved[0].provider.instantiate(None, 7).unwrap();
        assert_eq!(
            a.call(&ctx(), "token", &args).unwrap(),
            b.call(&ctx(), "token", &args).unwrap(),
            "burned load is bit-identical to a fresh compile"
        );
        assert_eq!(burned.id(), resolved[0].provider.id());
    }

    /// `load()` itself probes the burn cache: with a matching `.cwasm` under
    /// PERFSCALE_CACHE_DIR the provider loads identically (and the artifact
    /// is exactly what `burn_component` would write).
    #[test]
    #[serial_test::file_serial(burn_cache_env)]
    fn load_probes_the_burn_cache() {
        let Some(f) = fixtures() else { return };
        let bytes = std::fs::read(&f.hello).unwrap();
        let sha = super::super::burn::sha256_hex(&bytes);
        let dir = tempfile::tempdir().unwrap();
        let root = crate::import::library_cache_root(&crate::import::ImportOptions {
            cache_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        });
        crate::import::write_library_burn(
            &root,
            &sha,
            &super::super::burn::burn_component(&bytes).unwrap(),
        )
        .unwrap();

        // Point the loader's default cache root at the prepared cache.
        let prev = std::env::var_os("PERFSCALE_CACHE_DIR");
        std::env::set_var("PERFSCALE_CACHE_DIR", dir.path());
        let result = validate_libraries(&[lib_ref(&f.hello)], false, None);
        match prev {
            Some(v) => std::env::set_var("PERFSCALE_CACHE_DIR", v),
            None => std::env::remove_var("PERFSCALE_CACHE_DIR"),
        }
        let resolved = result.unwrap();
        assert_eq!(resolved[0].provider.id(), "perfscale_hello_library@v0.1.0");
    }

    /// Forgery attempt: a `.cwasm` whose *filename* is the victim's digest
    /// but whose header carries another component's source hash. The loader
    /// hashes the bytes it actually read, so the header's source_sha256
    /// mismatches and the artifact is ignored (silent fallback to compile).
    #[test]
    #[serial_test::file_serial(burn_cache_env)]
    fn forged_burn_artifact_is_ignored() {
        let Some(f) = fixtures() else { return };
        let hello_bytes = std::fs::read(&f.hello).unwrap();
        let hello_sha = super::super::burn::sha256_hex(&hello_bytes);
        let spin_bytes = std::fs::read(&f.spin).unwrap();
        // Burn *spin* but store it under *hello*'s digest — cache poisoning.
        let forged = super::super::burn::burn_component(&spin_bytes).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("libraries");
        crate::import::write_library_burn(&root, &hello_sha, &forged).unwrap();

        let prev = std::env::var_os("PERFSCALE_CACHE_DIR");
        std::env::set_var("PERFSCALE_CACHE_DIR", dir.path());
        let result = validate_libraries(&[lib_ref(&f.hello)], false, None);
        match prev {
            Some(v) => std::env::set_var("PERFSCALE_CACHE_DIR", v),
            None => std::env::remove_var("PERFSCALE_CACHE_DIR"),
        }
        let resolved = result.unwrap();
        // The forged artifact was ignored: the provider is hello, not spin.
        assert_eq!(resolved[0].provider.id(), "perfscale_hello_library@v0.1.0");
        let mut inst = resolved[0].provider.instantiate(None, 7).unwrap();
        assert_eq!(
            inst.call(&ctx(), "greet", &[Value::from("world")]).unwrap(),
            "hello, world!"
        );
    }

    /// A grant wider than the component's imports is inert and must not fail
    /// the load (RFC 005: lint-warning territory — the load-time trace is the
    /// implemented surface; fail-closed concerns imports, not spare grants).
    #[test]
    fn grant_wider_than_imports_loads() {
        let Some(f) = fixtures() else { return };
        let mut r = lib_ref(&f.hello);
        r.capabilities = Some(vec![Capability::Simple("clock".into())]);
        let resolved = validate_libraries(&[r], true, None).unwrap();
        let mut inst = resolved[0].provider.instantiate(None, 1).unwrap();
        assert_eq!(
            inst.call(&ctx(), "greet", &[Value::from("world")]).unwrap(),
            "hello, world!"
        );
    }
}
