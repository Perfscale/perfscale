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
//! `wasi:http/*` → `net` (**rejected in this build** — see
//! [`WasmLibraryProvider::load`]). A component importing more than granted is
//! a hard load error naming needed vs granted capabilities. The linker then
//! connects *only* the granted interfaces: `fs` is a read-only preopen
//! confined to the run's `fs_root`, `clock` is `wasi:clocks`. `wasi:random`
//! is never connected (determinism: libraries draw from `ctx.seed`).
//!
//! # Resource limits
//!
//! - **Fuel**: the engine is built with `consume_fuel(true)` and every
//!   `call`/`init` runs under a fresh [`PER_CALL_FUEL`] budget; a runaway
//!   guest traps and fails the step. Wall-time epoch interruption is *not*
//!   wired in phase 2: it needs a dedicated ticking thread per engine, and
//!   fuel already bounds non-I/O execution — documented deviation, revisit
//!   when `net` lands.
//! - **Memory**: [`StoreLimits`] caps linear memory at [`MEMORY_CAP`].

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p2::bindings as wasi_bindings;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};

use super::{CallCtx, Capability, FunctionInfo, LibraryInstance, LibraryProvider};

/// Host-side bindings generated from the same `wit/library.wit` the SDK uses
/// (the current ABI, 0.2.x — context carries `settings-json`).
mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "perfscale-library",
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
const SUPPORTED_WIT: &str = "perfscale:library/library@0.1.x and @0.2.x";

/// Fuel budget per guest call (`init` and `call`). Fuel is the phase-2
/// execution bound: ~50M fuel is generous for value generation (string
/// formatting, JSON parsing, small corpora lookups measure in the low
/// thousands) and traps infinite loops long before they hurt tail latency.
const PER_CALL_FUEL: u64 = 50_000_000;

/// Per-instance linear-memory cap: one instance per (library × generator
/// owner), so this multiplies — keep it tight (RFC 005 pitfall 5).
const MEMORY_CAP: usize = 64 << 20; // 64 MiB

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
            Engine::new(&cfg).map_err(|e| format!("failed to create the WASM engine: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Per-instance host state. `limits` enforces [`MEMORY_CAP`].
struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// Marker for the `wasi:io/*` plumbing (streams/poll/error) that `fs`- and
/// `clock`-granted guests import transitively.
struct HasIo;

impl wasmtime::component::HasData for HasIo {
    type Data<'a> = &'a mut ResourceTable;
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
#[derive(Debug, Default)]
struct Grants {
    fs: bool,
    clock: bool,
    /// Host allowlist; rejected at load in this build (see [`WasmLibraryProvider::load`]).
    net: Vec<String>,
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
                Capability::Net { net } => g.net = net.clone(),
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
            out.push(format!("net: [{}]", self.net.join(", ")));
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
            "clocks" => Some("clock"),
            "http" => Some("net"),
            other => {
                return Err(format!(
                    "wasi:{other}/* — this capability is not supported (supported: fs, clock)"
                ));
            }
        });
    }
    Err(format!(
        "'{name}' — unknown import; only wasi:filesystem, wasi:clocks (and their wasi:io plumbing) are supportable"
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
    /// WIT ABI major the component was built against (1 or 2) — picks the
    /// bindings used at instantiation.
    wit_major: u8,
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
    /// check, capability enforcement, linker construction, `info()` probe.
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
        for (name, _) in ty.exports(engine) {
            if let Some(rest) = name.strip_prefix("perfscale:library/library") {
                exported_wit = Some(rest.trim_start_matches('@').to_string());
            }
        }
        let wit_major: u8 = match exported_wit.as_deref() {
            Some(v) if v.starts_with("0.1.") => 1,
            Some(v) if v.starts_with("0.2.") => 2,
            other => {
                return Err(format!(
                    "library '{}': unsupported WIT interface version — this build supports {SUPPORTED_WIT}, the component exports perfscale:library/library@{}",
                    display(),
                    other.unwrap_or("<none — not a perfscale library component>")
                ));
            }
        };

        // Capability enforcement: map every import to its capability and
        // refuse imports beyond the grant.
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
        let mut missing = Vec::new();
        if needed.fs && !grants.fs {
            missing.push("fs");
        }
        // The wall clock is dragged in by std's fs-metadata code too, so an
        // `fs` grant satisfies it; `clock` alone also satisfies it.
        if needed.clock && !grants.clock && !grants.fs {
            missing.push("clock");
        }
        if needed.net {
            // No grant can satisfy it in this build — say so explicitly.
            return Err(format!(
                "library '{}' imports wasi:http (net): net capability is not yet supported in this build",
                display()
            ));
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

        // Connect exactly what the component may use (fail-closed):
        //   - wasi:io plumbing + wasi:cli in sink form: unconditionally
        //     (rustc-built wasip2 components import them no matter what; the
        //     sinks expose nothing of the host);
        //   - wasi:clocks / wasi:filesystem: only when granted.
        let linker = build_linker(engine, &grants, &display())?;

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
            grants,
            wit_major,
        };

        // `info()` once at load: populates id/name/functions for
        // lint/validation. A malformed or trapping `info()` is a load error.
        let info = provider.probe_info()?;
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
    fn instantiate_store(&self) -> Result<(Store<HostState>, Instantiated), String> {
        let mut ctx = WasiCtx::builder();
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
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_CAP)
                .instances(16)
                .tables(16)
                .memories(1)
                .build(),
        };
        let mut store = Store::new(engine()?, state);
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(PER_CALL_FUEL)
            .map_err(|e| format!("library '{}': {e}", self.path.display()))?;
        let instance = match self.wit_major {
            1 => Instantiated::V1(
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
            _ => Instantiated::V2(
                bindings::PerfscaleLibrary::instantiate(&mut store, &self.component, &self.linker)
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
        let json = match &instance {
            Instantiated::V1(i) => i.perfscale_library_library().call_info(&mut store),
            Instantiated::V2(i) => i.perfscale_library_library().call_info(&mut store),
        }
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
            functions,
        })
    }
}

/// A component instance, through the bindings of its WIT major: 0.1
/// components get the legacy context (no `settings-json`), 0.2 components
/// the current one.
enum Instantiated {
    V1(bindings_v1::PerfscaleLibrary),
    V2(bindings::PerfscaleLibrary),
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
        }
    }

    fn call_call(
        &self,
        store: &mut Store<HostState>,
        ctx: &CallCtx<'_>,
        func: &str,
        args_json: &str,
    ) -> Result<Result<String, String>, wasmtime::Error> {
        match self {
            Instantiated::V1(i) => {
                // The 0.1 ABI has no settings field — the guest simply
                // never sees them.
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
                let wctx = bindings::exports::perfscale::library::library::Context {
                    message_seq: ctx.message_seq,
                    iteration_seq: ctx.iteration_seq,
                    vu_id: ctx.vu_id,
                    seed: ctx.seed,
                    time_ms: ctx.time_ms,
                    settings_json: ctx.settings_json.to_string(),
                };
                i.perfscale_library_library()
                    .call_call(store, &wctx, func, args_json)
            }
        }
    }
}

/// Parsed `info()` JSON contract (RFC 005: `info` returns JSON — decided).
#[derive(serde::Deserialize)]
struct WasmLibraryInfoJson {
    name: String,
    #[serde(default)]
    version: String,
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
    functions: Vec<FunctionInfo>,
}

impl LibraryProvider for WasmLibraryProvider {
    fn id(&self) -> &str {
        &self.id
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
        // (RFC 005 open question — decided).
        let config_json = serde_json::to_string(&config.unwrap_or(Value::Null))
            .map_err(|e| format!("library '{}': {e}", self.id))?; // Value → JSON cannot fail
        instance
            .call_init(&mut store, &config_json)
            .map_err(|e| format!("library '{}': init trapped or exhausted fuel: {e}", self.id))?
            .map_err(|e| format!("library '{}': init failed: {e}", self.id))?;
        Ok(Box::new(WasmLibraryInstance {
            store,
            instance,
            id: self.id.clone(),
        }))
    }
}

/// One live WASM library instance (per generator owner).
struct WasmLibraryInstance {
    store: Store<HostState>,
    instance: Instantiated,
    id: String,
}

impl LibraryInstance for WasmLibraryInstance {
    fn call(&mut self, ctx: &CallCtx<'_>, func: &str, args: &[Value]) -> Result<String, String> {
        let args_json = serde_json::to_string(args)
            .map_err(|e| format!("{}.{func}: failed to encode arguments: {e}", self.id))?;
        // Fresh fuel budget per call (the store's fuel is cumulative).
        self.store
            .set_fuel(PER_CALL_FUEL)
            .map_err(|e| format!("{}.{func}: {e}", self.id))?;
        match self
            .instance
            .call_call(&mut self.store, ctx, func, &args_json)
        {
            Ok(Ok(value)) => Ok(value),
            // Guest-level error (bad function/args): the step fails with it.
            Ok(Err(e)) => Err(format!("{}.{func}: {e}", self.id)),
            // Trap or fuel exhaustion.
            Err(e) => Err(format!(
                "{}.{func}: the WASM guest trapped or exhausted its fuel budget: {e}",
                self.id
            )),
        }
    }
}

fn link_err(file: &str, e: wasmtime::Error) -> String {
    format!("library '{file}': failed to connect a granted capability: {e}")
}

/// Parse the YAML grant and reject `net` (not implemented in this build) —
/// shared by the file and embedded load paths.
fn check_grants(display: &str, capabilities: Option<&[Capability]>) -> Result<Grants, String> {
    let grants = Grants::parse(capabilities.unwrap_or(&[]))
        .map_err(|e| format!("library '{display}': {e}"))?;

    // Phase 2 deviation: wasi:http egress (host-mediated, allowlisted) is
    // not implemented in this build. The grant is still parsed and
    // reported, but rejected — fail-closed rather than silently unmediated.
    if !grants.net.is_empty() {
        return Err(format!(
            "library '{display}': net capability is not yet supported in this build"
        ));
    }
    Ok(grants)
}

/// Build the per-provider linker: wasi:io plumbing and wasi:cli in sink form
/// always (Rust-std noise; the sinks expose nothing of the host), wasi:clocks
/// and wasi:filesystem only per grant.
fn build_linker(
    engine: &Engine,
    grants: &Grants,
    display: &str,
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

    if grants.clock || grants.fs {
        // std's fs-metadata code imports the wall clock alongside filesystem.
        use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView};
        wasi_bindings::clocks::wall_clock::add_to_linker::<HostState, WasiClocks>(
            l,
            HostState::clocks,
        )
        .map_err(|e| link_err(display, e))?;
    }
    // The monotonic clock is unconditional: every rustc-built wasip2
    // component imports it (std's Instant/parker), so it cannot be a
    // meaningful grant for Rust guests.
    {
        use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView};
        wasi_bindings::clocks::monotonic_clock::add_to_linker::<HostState, WasiClocks>(
            l,
            HostState::clocks,
        )
        .map_err(|e| link_err(display, e))?;
    }
    if grants.fs {
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
        spin: PathBuf,
        fsreader: PathBuf,
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
                    spin: build("spin", "perfscale_spin_library")?,
                    fsreader: build("fsreader", "perfscale_fsreader_library")?,
                })
            })
            .as_ref()
    }

    fn lib_ref(path: &Path) -> LibraryRef {
        LibraryRef {
            use_: path.to_string_lossy().into_owned(),
            sha256: None,
            r#as: Some("hello".into()),
            capabilities: None,
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
        assert!(import_capability("wasi:random/random@0.2.0").is_err());
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

    #[test]
    fn net_grant_is_rejected_in_this_build() {
        let Some(f) = fixtures() else { return };
        let mut r = lib_ref(&f.hello);
        r.capabilities = Some(vec![Capability::Net {
            net: vec!["api.example.com".into()],
        }]);
        let err = validate_libraries(&[r], true, None).unwrap_err();
        assert!(err.contains("net capability is not yet supported"), "{err}");
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
}
