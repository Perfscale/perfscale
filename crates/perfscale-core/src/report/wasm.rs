//! WASM end-of-run reporters (issue #9) — wasmtime host for
//! `perfscale:reporter@0.1.0` (`wit/reporter/reporter.wit` at the workspace root).
//!
//! A reporter is a WASI Preview 2 component (`use: wasm:./reporter.wasm` in
//! `report.reporters`) invoked exactly once per run, after the built-in
//! renderers: the host hands `on-report` the canonical `report@v1` document
//! verbatim plus the reporter's own `with.config` block, and the guest's only
//! output channel is the host import `emit-file`, which writes into the
//! reporter's output directory (`with.output`, default
//! `./perfscale-report-<module-stem>`).
//!
//! # Sandboxing
//!
//! Reporters reuse the library host's sandbox (`crate::library::wasm`): the
//! same engine and WASI link set, with fuel/memory caps sized for one big
//! end-of-run call ([`REPORTER_FUEL`], [`REPORTER_MEMORY_CAP`]) instead of
//! the library's per-message budget. As for libraries, wasi:cli/clocks/
//! filesystem are linked in sink form (no preopens) so jco/StarlingMonkey-built guests
//! instantiate, but any real file or network access fails at runtime. The
//! `net` capability is never granted to reporters. `emit-file` accepts only
//! relative, traversal-safe names (no absolute paths, no `..`, no `\`), so
//! writes cannot escape the output directory.
//!
//! # Error contract
//!
//! Guest errors, traps and `emit-file` failures are best-effort: the caller
//! (CLI delivery) logs them and counts them as `engine_report_errors`; the
//! run's exit code is never affected. Load-time problems (missing file,
//! compile failure, wrong world) are configuration errors — they surface
//! fail-fast via [`crate::report::validate_reporters`] before the run starts.

use std::collections::HashMap;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Store, StoreLimitsBuilder};
use wasmtime_wasi::WasiCtx;
use wasmtime_wasi_http::WasiHttpCtx;

use crate::library::burn;
use crate::library::wasm::{
    build_linker, engine, net_allowlist, off_runtime, HostState, MAX_COMPONENT_BYTES,
};

/// Host-side bindings generated from the same `wit/reporter.wit` the SDKs
/// use (the current ABI, 0.1.x).
mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/reporter",
        world: "perfscale-reporter",
    });
}

/// The WIT package versions this host implements. A component built against
/// another major fails to load with both versions named.
const SUPPORTED_WIT: &str = "perfscale:reporter/reporter@0.1.x";

/// Fuel budget for the single `on-report` call. A reporter runs once per
/// run — unlike the per-message library call budget — and JS-built guests
/// (jco/StarlingMonkey) burn orders of magnitude more fuel parsing the
/// report document than a native value generator does, so the budget is far
/// more generous than [`PER_CALL_FUEL`] while still trapping runaway guests.
const REPORTER_FUEL: u64 = 2_000_000_000;

/// Linear-memory cap for the reporter instance. Same reasoning: one call per
/// run, and a JS runtime needs headroom for the parsed report document.
const REPORTER_MEMORY_CAP: usize = 256 << 20; // 256 MiB

/// The `perfscale:reporter/host` import: the guest's only output channel.
/// Writes are confined to the instance's reporter output directory.
impl bindings::perfscale::reporter::host::Host for &mut HostState {
    fn emit_file(&mut self, name: String, contents: String) -> Result<(), String> {
        let root = self
            .reporter_output
            .clone()
            .ok_or_else(|| "emit-file is not available in this context".to_string())?;
        let path = safe_join(&root, &name)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create '{}': {e}", parent.display()))?;
        }
        std::fs::write(&path, contents)
            .map_err(|e| format!("failed to write '{}': {e}", path.display()))
    }
}

/// Marker wiring `HostState` into the host-import linker (same pattern as
/// `HasIo` in the library host).
struct HasReporter;

impl wasmtime::component::HasData for HasReporter {
    type Data<'a> = &'a mut HostState;
}

/// Resolve `name` under `root`, rejecting anything that could escape it:
/// absolute paths, `..` / `.` components, Windows prefixes and `\`
/// separators (a backslash is a separator on Windows but a plain character
/// on Unix — refusing it keeps the check platform-independent).
fn safe_join(root: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() {
        return Err("emit-file: the file name must not be empty".into());
    }
    if name.contains('\\') {
        return Err(format!(
            "emit-file: '{name}' is not a safe relative path ('\\' is not allowed)"
        ));
    }
    let rel = Path::new(name);
    for c in rel.components() {
        if !matches!(c, PathComponent::Normal(_)) {
            return Err(format!(
                "emit-file: '{name}' is not a safe relative path (no absolute paths, no '..')"
            ));
        }
    }
    Ok(root.join(rel))
}

/// One run-cache entry: the module's content hash plus its compiled component.
type CachedComponent = ([u8; 32], Component);

/// Compiled reporter components of this process, keyed by module path with
/// the content hash alongside: the validate path (`WasmReporter::check`) and
/// the delivery path (`WasmReporter::load`) run in the same CLI process, and
/// the second lookup must reuse the first compile. A module swapped mid-run
/// recompiles — the stored digest is compared against the fresh bytes.
static RUN_COMPONENTS: OnceLock<Mutex<HashMap<PathBuf, CachedComponent>>> = OnceLock::new();

/// How a reporter component was obtained — test instrumentation for the
/// cache layers.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompileSource {
    /// Deserialized from the disk burn cache.
    BurnCache,
    /// Full Cranelift compilation.
    Compiled,
}

/// Where the burn-cache write-through happens after a cold compile.
///
/// The validate path (`WasmReporter::check`, run by lint and pre-run
/// validation) needs only the compiled component for the world-export check,
/// so the serialize+write — which can cost as much as the compile for large
/// JS-built components — moves to a background thread: lint returns after
/// the compile. Delivery (`load`) and `perfscale install` stay synchronous:
/// their caller expects the artifact to exist (install is the explicit
/// prewarm step; delivery usually hits the run cache anyway).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BurnMode {
    /// Serialize and write the artifact before returning.
    Sync,
    /// Serialize and write on a background thread, best-effort. A process
    /// exiting first simply drops the write (the temp+rename in
    /// `write_reporter_burn` guarantees no partial artifact); the next
    /// cold run re-burns.
    Background,
}

/// Background burn-artifact writes in flight. Fire-and-forget by design —
/// shutdown does not join them (a reporter artifact is advisory; joining
/// would put the serialize cost back on the lint critical path). Tests use
/// [`drain_burn_writes`] for determinism.
static PENDING_BURN_WRITES: OnceLock<Mutex<Vec<std::thread::JoinHandle<()>>>> = OnceLock::new();

/// Compile (or reuse) the reporter component for `bytes`.
///
/// Three layers, cheapest first:
///
/// 1. the per-process [`RUN_COMPONENTS`] map — the second `load`/`check` of
///    the same module in one run is free;
/// 2. the disk burn cache (`<cache>/reporters/<sha256>.cwasm`, same
///    `PFSBURN1` format and invalidation rules as the library cache — engine
///    config tag, wasmtime version, target triple, source digest): a hit
///    deserializes native code instead of recompiling;
/// 3. a full `Component::new`, which then writes through to both caches:
///    the in-process entry and a best-effort disk artifact (a cache write
///    failure never fails the load — the cache is advisory; `mode` picks
///    synchronous or background write-through).
fn compile_cached(module: &Path, bytes: &[u8], mode: BurnMode) -> Result<Component, String> {
    let name = || module.display().to_string();
    let sha = burn::sha256_bytes(bytes);
    let run_cache = RUN_COMPONENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let hit = run_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(module)
        .and_then(|(cached_sha, c)| (*cached_sha == sha).then(|| c.clone()));
    if let Some(component) = hit {
        return Ok(component);
    }

    // Disk burn cache: identical rules to the library loader — any header
    // mismatch (engine config, wasmtime version, target, digest) or a
    // corrupted artifact silently falls back to full compilation.
    let cache_root = crate::import::default_reporter_cache_root();
    let burn_path = crate::import::reporter_burn_path(&cache_root, &burn::hex(&sha));
    let component = match std::fs::read(&burn_path) {
        Ok(artifact) => match burn::load_burned(&artifact, &sha) {
            Some(component) => {
                #[cfg(test)]
                record_compile(CompileSource::BurnCache);
                component
            }
            None => {
                tracing::debug!(
                    "reporter '{}': burn artifact '{}' does not match this build — compiling from source",
                    name(),
                    burn_path.display()
                );

                compile_and_burn(&name(), bytes, &cache_root, &sha, mode)?
            }
        },
        Err(_) => compile_and_burn(&name(), bytes, &cache_root, &sha, mode)?,
    };
    run_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(module.to_path_buf(), (sha, component.clone()));
    Ok(component)
}

/// Full compilation plus the burn-cache write-through (layer 3), sync or
/// background per `mode`.
fn compile_and_burn(
    name: &str,
    bytes: &[u8],
    cache_root: &Path,
    sha: &[u8; 32],
    mode: BurnMode,
) -> Result<Component, String> {
    let component = compile_fresh(name, bytes)?;
    match mode {
        BurnMode::Sync => write_burn_artifact(name, &component, cache_root, sha),
        BurnMode::Background => {
            // Serialize and write off the critical path. The component is
            // Send (wasmtime components are); errors are logged, never
            // propagated — the cache is advisory.
            let name = name.to_string();
            let cache_root = cache_root.to_path_buf();
            let sha = *sha;
            let component = component.clone();
            let handle = std::thread::spawn(move || {
                write_burn_artifact(&name, &component, &cache_root, &sha);
            });
            PENDING_BURN_WRITES
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(handle);
        }
    }
    Ok(component)
}

/// Full Cranelift compilation — the cold path every cache layer missed.
fn compile_fresh(name: &str, bytes: &[u8]) -> Result<Component, String> {
    let component = Component::new(engine()?, bytes)
        .map_err(|e| format!("reporter '{name}': failed to compile as a WASM component: {e}"))?;
    #[cfg(test)]
    record_compile(CompileSource::Compiled);
    Ok(component)
}

/// Serialize the component we just compiled (never a second precompile
/// pass) and cache the artifact for the next run. Best-effort — the caller
/// must not fail over a cache write.
fn write_burn_artifact(name: &str, component: &Component, cache_root: &Path, sha: &[u8; 32]) {
    match component.serialize() {
        Ok(payload) => {
            let artifact = burn::wrap_serialized(&payload, sha);
            if let Err(e) =
                crate::import::write_reporter_burn(cache_root, &burn::hex(sha), &artifact)
            {
                tracing::debug!("reporter '{name}': burn-cache write failed: {e}");
            }
        }
        Err(e) => {
            tracing::debug!("reporter '{name}': could not serialize for the burn cache: {e}")
        }
    }
}

/// Join every pending background burn write — the test seam that makes the
/// background write-through deterministic.
#[cfg(test)]
fn drain_burn_writes() {
    let pending: Vec<_> = PENDING_BURN_WRITES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect();
    for h in pending {
        let _ = h.join();
    }
}

#[cfg(test)]
static COMPILE_LOG: Mutex<Vec<CompileSource>> = Mutex::new(Vec::new());

#[cfg(test)]
fn record_compile(source: CompileSource) {
    COMPILE_LOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(source);
}

/// Read a reporter module, enforcing the load-time size cap (compilation
/// memory scales with module size; matches the library host's cap).
fn read_module(module: &Path) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(module)
        .map_err(|e| format!("reporter '{}': failed to read: {e}", module.display()))?;
    if bytes.len() > MAX_COMPONENT_BYTES {
        return Err(format!(
            "reporter '{}': component is {} MiB — the limit is {} MiB",
            module.display(),
            bytes.len() >> 20,
            MAX_COMPONENT_BYTES >> 20,
        ));
    }
    Ok(bytes)
}

/// The fail-closed world check: the component must export
/// `perfscale:reporter/reporter@0.1.x`. Cheap on both the compiled and the
/// deserialized (burn-cache) component — it reads the component type.
fn world_check(component: &Component, module: &Path) -> Result<(), String> {
    let ty = component.component_type();
    let mut exported_wit: Option<String> = None;
    for (name, _) in ty.exports(engine()?) {
        if let Some(rest) = name.strip_prefix("perfscale:reporter/reporter") {
            exported_wit = Some(rest.trim_start_matches('@').to_string());
        }
    }
    match exported_wit.as_deref() {
        Some(v) if v.starts_with("0.1.") => Ok(()),
        other => Err(format!(
            "reporter '{}': unsupported WIT interface version — this build supports {SUPPORTED_WIT}, the component exports perfscale:reporter/reporter@{}",
            module.display(),
            other.unwrap_or("<none — not a perfscale reporter component>")
        )),
    }
}

/// Prewarm the disk burn cache for one reporter module — the
/// `perfscale install` path: full compile, world check, synchronous artifact
/// write. Idempotent: an up-to-date artifact is left alone.
pub fn burn_module(module: &Path) -> Result<(), String> {
    let name = module.display().to_string();
    let bytes = read_module(module)?;
    let sha = burn::sha256_bytes(&bytes);
    let hex = burn::hex(&sha);
    let cache_root = crate::import::default_reporter_cache_root();
    if let Ok(existing) = std::fs::read(crate::import::reporter_burn_path(&cache_root, &hex)) {
        if burn::artifact_matches(&existing, &sha) {
            return Ok(());
        }
    }
    let component = compile_fresh(&name, &bytes)?;
    world_check(&component, module)?;
    write_burn_artifact(&name, &component, &cache_root, &sha);
    Ok(())
}

/// A compiled WASM reporter: compiled once per run (shared between the
/// validate and delivery paths via [`RUN_COMPONENTS`], and across runs via
/// the disk burn cache), instantiated once for the single `on-report` call.
pub struct WasmReporter {
    path: PathBuf,
    output: PathBuf,
    component: Component,
    linker: Linker<HostState>,
}

impl WasmReporter {
    /// Load, compile and world-check the reporter component at `module`.
    /// `output` overrides the output directory; the default is
    /// `./perfscale-report-<module-stem>`.
    pub fn load(module: &Path, output: Option<&str>) -> Result<Self, String> {
        let display = || module.display().to_string();
        let bytes = read_module(module)?;
        let engine = engine()?;
        let component = compile_cached(module, &bytes, BurnMode::Sync)?;

        // World check: the component must export `perfscale:reporter/reporter@0.1.x`.
        let ty = component.component_type();
        let mut exported_wit: Option<String> = None;
        for (name, _) in ty.exports(engine) {
            if let Some(rest) = name.strip_prefix("perfscale:reporter/reporter") {
                exported_wit = Some(rest.trim_start_matches('@').to_string());
            }
        }
        match exported_wit.as_deref() {
            Some(v) if v.starts_with("0.1.") => {}
            other => {
                return Err(format!(
                    "reporter '{}': unsupported WIT interface version — this build supports {SUPPORTED_WIT}, the component exports perfscale:reporter/reporter@{}",
                    display(),
                    other.unwrap_or("<none — not a perfscale reporter component>")
                ));
            }
        }

        // Reporters never get `net`; the WASI sink set lets toolchain-noise
        // imports (jco/StarlingMonkey fs/clocks) link with zero preopens.
        let mut linker = build_linker(engine, &display(), false, false)?;
        bindings::perfscale::reporter::host::add_to_linker::<HostState, HasReporter>(
            &mut linker,
            |s| s,
        )
        .map_err(|e| format!("reporter '{}': failed to connect emit-file: {e}", display()))?;

        let output = output.map(PathBuf::from).unwrap_or_else(|| {
            let stem = module
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("reporter");
            PathBuf::from(format!("./perfscale-report-{stem}"))
        });
        Ok(Self {
            path: module.to_path_buf(),
            output,
            component,
            linker,
        })
    }

    /// Compile-and-world-check only — the fail-fast lint/validation path.
    /// The burn-cache write-through runs in the background (see
    /// [`BurnMode::Background`]): validation needs the compiled component for
    /// the export check, not the artifact on disk.
    pub fn check(module: &Path) -> Result<(), String> {
        let bytes = read_module(module)?;
        let component = compile_cached(module, &bytes, BurnMode::Background)?;
        world_check(&component, module)
    }

    /// Instantiate the component and call `on-report` once. `report_json` is
    /// the canonical `report@v1` document; `config_json` the reporter's
    /// `with.config` block (`{}` when unset). Returns the guest's summary
    /// line; guest errors and traps both surface as `Err`.
    pub fn run(&self, report_json: &str, config_json: &str) -> Result<String, String> {
        let display = || self.path.display().to_string();
        let mut ctx = WasiCtx::builder();
        // Fixed insecure-seed, matching the library host: report rendering
        // stays reproducible even if the guest's toolchain asks for it.
        ctx.insecure_random_seed(0);
        let state = HostState {
            ctx: ctx.build(),
            http_ctx: WasiHttpCtx::new(),
            hooks: net_allowlist::NetHooks::new(Vec::new()),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(REPORTER_MEMORY_CAP)
                .instances(16)
                .tables(16)
                .memories(1)
                .build(),
            reporter_output: Some(self.output.clone()),
        };
        let mut store = Store::new(engine()?, state);
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(REPORTER_FUEL)
            .map_err(|e| format!("reporter '{}': {e}", display()))?;
        let instance =
            bindings::PerfscaleReporter::instantiate(&mut store, &self.component, &self.linker)
                .map_err(|e| format!("reporter '{}': failed to instantiate: {e}", display()))?;
        // Off the runtime worker thread: the guest may touch linked WASI
        // interfaces (see `off_runtime` in the library host).
        let result = off_runtime(|| {
            instance.perfscale_reporter_reporter().call_on_report(
                &mut store,
                report_json,
                config_json,
            )
        });
        match result {
            Ok(Ok(summary)) => Ok(summary),
            Ok(Err(e)) => Err(format!("reporter '{}': on-report failed: {e}", display())),
            Err(e) => Err(format!(
                "reporter '{}': the WASM guest trapped or exhausted its fuel budget: {e}",
                display()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use crate::report::v1::{self, ReportContext};
    use std::sync::OnceLock;

    /// Serializes fixture-backed tests: the burn-cache tests mutate the
    /// process-global `PERFSCALE_CACHE_DIR` and the shared [`RUN_COMPONENTS`]
    /// map, and plain reporter tests must not interleave with that (their
    /// loads would add entries to the compile log).
    static CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn cache_test_lock() -> std::sync::MutexGuard<'static, ()> {
        CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    // --- fixture: a real reporter component built from the SDK examples -----

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

    /// Build the reporter example component once per test binary. Returns
    /// `None` (skipping) when the wasm32-wasip2 target is missing.
    fn fixture() -> Option<&'static PathBuf> {
        static FIXTURE: OnceLock<Option<PathBuf>> = OnceLock::new();
        FIXTURE
            .get_or_init(|| {
                if !wasip2_installed() {
                    eprintln!(
                        "skipping WASM reporter tests: wasm32-wasip2 target not installed \
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
                let status = std::process::Command::new(&cargo)
                    .args(["build", "--release", "--target", "wasm32-wasip2"])
                    .arg("--manifest-path")
                    .arg(ws.join("crates/perfscale-library-sdk/examples/reporter/Cargo.toml"))
                    .arg("--target-dir")
                    .arg(&target)
                    .status()
                    .ok()?;
                if !status.success() {
                    eprintln!("failed to build the reporter fixture component");
                    return None;
                }
                Some(target.join("wasm32-wasip2/release/perfscale_reporter_example.wasm"))
            })
            .as_ref()
    }

    fn report_json() -> String {
        let ctx = ReportContext {
            run_id: "run-42".into(),
            engine: "native".into(),
            perfscale_version: "0.0.0-test".into(),
            vus: Some(1),
            started_at: "2026-10-20T00:00:00Z".into(),
            wall_secs: 5.0,
            labels: serde_json::Map::new(),
            step_order: vec![("hit".into(), "std/http@v1".into())],
            iterations: 5,
            vus_gauge: 1,
            max_samples: Some(5),
        };
        let mut m = Metrics::default();
        for i in 0..5u64 {
            m.record_step("hit", 1.0 + i as f64, false);
        }
        v1::build_report_v1(&m, None, None, &ctx).to_json()
    }

    // --- pure checks (no fixture needed) ------------------------------------

    #[test]
    fn safe_join_rejects_traversal_and_absolute_paths() {
        let root = Path::new("/out");
        assert_eq!(safe_join(root, "a.txt").unwrap(), Path::new("/out/a.txt"));
        assert_eq!(
            safe_join(root, "sub/a.txt").unwrap(),
            Path::new("/out/sub/a.txt")
        );
        for bad in [
            "",
            "../evil.txt",
            "a/../../evil.txt",
            "/etc/passwd",
            "./x",
            "a\\b",
        ] {
            assert!(safe_join(root, bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn missing_module_is_a_clear_error() {
        let err = WasmReporter::check(Path::new("/definitely/missing.wasm")).unwrap_err();
        assert!(err.contains("failed to read"), "{err}");
    }

    // --- fixture-backed tests ------------------------------------------------

    #[test]
    fn on_report_receives_report_v1_and_emit_file_writes() {
        let _lock = cache_test_lock();
        let Some(module) = fixture() else { return };
        let dir = tempfile::tempdir().unwrap();
        let reporter = WasmReporter::load(module, Some(dir.path().to_str().unwrap())).unwrap();
        let summary = reporter.run(&report_json(), "{}").unwrap();
        assert_eq!(summary, "run run-42: 1 step(s)");
        // emit-file wrote both files under the output directory.
        let summary_file = std::fs::read_to_string(dir.path().join("summary.txt")).unwrap();
        assert_eq!(summary_file, summary);
        let report_file = std::fs::read_to_string(dir.path().join("report.json")).unwrap();
        let parsed: v1::ReportV1 = serde_json::from_str(&report_file).unwrap();
        assert_eq!(parsed.run.run_id, "run-42");
        assert_eq!(parsed.steps.len(), 1);
    }

    #[test]
    fn guest_error_is_best_effort() {
        let _lock = cache_test_lock();
        let Some(module) = fixture() else { return };
        let dir = tempfile::tempdir().unwrap();
        let reporter = WasmReporter::load(module, Some(dir.path().to_str().unwrap())).unwrap();
        let err = reporter
            .run(&report_json(), r#"{"fail": true}"#)
            .unwrap_err();
        assert!(err.contains("requested failure"), "{err}");
        // The files emitted before the error stay — the host never rolls back.
        assert!(dir.path().join("report.json").exists());
    }

    #[test]
    fn emit_file_refuses_path_traversal() {
        let _lock = cache_test_lock();
        let Some(module) = fixture() else { return };
        let dir = tempfile::tempdir().unwrap();
        let reporter = WasmReporter::load(module, Some(dir.path().to_str().unwrap())).unwrap();
        let err = reporter
            .run(&report_json(), r#"{"emit_evil": true}"#)
            .unwrap_err();
        assert!(err.contains("not a safe relative path"), "{err}");
        // Nothing escaped the output directory.
        assert!(!dir.path().join("../evil.txt").exists());
    }

    // --- burn cache ----------------------------------------------------------

    struct CacheTestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        reporters_root: PathBuf,
    }

    /// Fresh cache state: empty in-process map, empty compile log, and an
    /// isolated `PERFSCALE_CACHE_DIR`.
    fn cache_test_env() -> CacheTestEnv {
        let lock = cache_test_lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("PERFSCALE_CACHE_DIR", tmp.path());
        RUN_COMPONENTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        COMPILE_LOG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        CacheTestEnv {
            _lock: lock,
            reporters_root: tmp.path().join("reporters"),
            _tmp: tmp,
        }
    }

    fn compile_log() -> Vec<CompileSource> {
        COMPILE_LOG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn burn_path_for(root: &Path, module: &Path) -> PathBuf {
        let bytes = std::fs::read(module).unwrap();
        crate::import::reporter_burn_path(root, &burn::hex(&burn::sha256_bytes(&bytes)))
    }

    #[test]
    fn cold_cache_compiles_once_and_serves_validate_and_delivery() {
        let env = cache_test_env();
        let Some(module) = fixture() else { return };

        // The fail-fast validate path compiles once…
        WasmReporter::check(module).unwrap();
        // …and delivery reuses that compile, in-process.
        let dir = tempfile::tempdir().unwrap();
        let reporter = WasmReporter::load(module, Some(dir.path().to_str().unwrap())).unwrap();
        reporter.run(&report_json(), "{}").unwrap();

        assert_eq!(compile_log(), [CompileSource::Compiled]);
        // The validate path's background write-through lands the artifact.
        drain_burn_writes();
        assert!(
            burn_path_for(&env.reporters_root, module).is_file(),
            "cold compile must write the burn artifact"
        );
    }

    #[test]
    fn background_burn_write_leaves_a_valid_artifact() {
        let env = cache_test_env();
        let Some(module) = fixture() else { return };

        // check() returns after the compile; the serialize+write runs in the
        // background.
        WasmReporter::check(module).unwrap();
        assert_eq!(compile_log(), [CompileSource::Compiled]);
        drain_burn_writes();

        let bytes = std::fs::read(module).unwrap();
        let artifact = std::fs::read(burn_path_for(&env.reporters_root, module)).unwrap();
        assert!(burn::artifact_matches(
            &artifact,
            &burn::sha256_bytes(&bytes)
        ));
        // And it is loadable: the next "process" (fresh run cache) deserializes.
        RUN_COMPONENTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        COMPILE_LOG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        WasmReporter::check(module).unwrap();
        assert_eq!(compile_log(), [CompileSource::BurnCache]);
    }

    /// A leftover `.cwasm.tmp-*` from a killed write must not be picked up:
    /// the load falls back to compilation, and the fresh write lands via
    /// temp+rename.
    #[test]
    fn stale_partial_burn_write_is_ignored() {
        let env = cache_test_env();
        let Some(module) = fixture() else { return };
        std::fs::create_dir_all(&env.reporters_root).unwrap();
        let bytes = std::fs::read(module).unwrap();
        let hex = burn::hex(&burn::sha256_bytes(&bytes));
        std::fs::write(
            env.reporters_root.join(format!(".{hex}.cwasm.tmp-99999")),
            b"partial-from-a-killed-process",
        )
        .unwrap();

        let reporter = WasmReporter::load(module, None).unwrap();
        let _ = reporter;
        assert_eq!(compile_log(), [CompileSource::Compiled]);
        let artifact = std::fs::read(burn_path_for(&env.reporters_root, module)).unwrap();
        assert!(burn::artifact_matches(
            &artifact,
            &burn::sha256_bytes(&bytes)
        ));
    }

    /// `perfscale install`'s prewarm: burn_module compiles, world-checks and
    /// writes the artifact synchronously; a repeat call is a no-op.
    #[test]
    fn burn_module_prewarms_the_disk_cache_idempotently() {
        let env = cache_test_env();
        let Some(module) = fixture() else { return };

        burn_module(module).unwrap();
        assert_eq!(compile_log(), [CompileSource::Compiled]);
        let bytes = std::fs::read(module).unwrap();
        let artifact = std::fs::read(burn_path_for(&env.reporters_root, module)).unwrap();
        assert!(burn::artifact_matches(
            &artifact,
            &burn::sha256_bytes(&bytes)
        ));

        burn_module(module).unwrap();
        assert_eq!(
            compile_log(),
            [CompileSource::Compiled],
            "an up-to-date artifact is not re-burned"
        );
    }

    #[test]
    fn burn_module_rejects_a_wrong_world_component() {
        let env = cache_test_env();
        let _ = &env;
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("not-a-reporter.wasm");
        std::fs::write(&module, b"(component)").unwrap();
        let err = burn_module(&module).unwrap_err();
        assert!(err.contains("unsupported WIT interface version"), "{err}");
    }

    #[test]
    fn warm_burn_cache_deserializes_instead_of_compiling() {
        let env = cache_test_env();
        let Some(module) = fixture() else { return };
        // Pre-populate the disk cache with a well-formed artifact.
        let bytes = std::fs::read(module).unwrap();
        let artifact = burn::burn_component(&bytes).unwrap();
        let burn_path = burn_path_for(&env.reporters_root, module);
        crate::import::write_reporter_burn(
            &env.reporters_root,
            &burn::hex(&burn::sha256_bytes(&bytes)),
            &artifact,
        )
        .unwrap();
        assert!(burn_path.is_file());

        let dir = tempfile::tempdir().unwrap();
        let reporter = WasmReporter::load(module, Some(dir.path().to_str().unwrap())).unwrap();
        let summary = reporter.run(&report_json(), "{}").unwrap();
        assert_eq!(summary, "run run-42: 1 step(s)");
        assert_eq!(compile_log(), [CompileSource::BurnCache]);
    }

    #[test]
    fn corrupted_burn_artifact_falls_back_to_compilation() {
        let env = cache_test_env();
        let Some(module) = fixture() else { return };
        let burn_path = burn_path_for(&env.reporters_root, module);
        std::fs::create_dir_all(&env.reporters_root).unwrap();
        std::fs::write(&burn_path, b"PFSBURN1-corrupted-garbage").unwrap();

        let dir = tempfile::tempdir().unwrap();
        let reporter = WasmReporter::load(module, Some(dir.path().to_str().unwrap())).unwrap();
        reporter.run(&report_json(), "{}").unwrap();
        assert_eq!(compile_log(), [CompileSource::Compiled]);
        // The fallback compile replaces the corrupted artifact.
        let bytes = std::fs::read(module).unwrap();
        let repaired = std::fs::read(&burn_path).unwrap();
        assert!(burn::artifact_matches(
            &repaired,
            &burn::sha256_bytes(&bytes)
        ));
    }

    #[test]
    fn tampered_module_recompiles_instead_of_reusing_the_cached_component() {
        let _env = cache_test_env();
        let Some(fixture_path) = fixture() else {
            return;
        };
        // A mutable copy: overwriting the fixture itself would break others.
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("reporter.wasm");
        std::fs::copy(fixture_path, &module).unwrap();

        WasmReporter::load(&module, None).unwrap();
        assert_eq!(compile_log(), [CompileSource::Compiled]);

        // Same path, different bytes: the in-process entry is keyed on the
        // content hash, so this recompiles — and the (valid, but wrong-world)
        // component fails the reporter export check.
        std::fs::write(&module, b"(component)").unwrap();
        let err = WasmReporter::check(&module).unwrap_err();
        assert!(err.contains("unsupported WIT interface version"), "{err}");
        assert_eq!(
            compile_log(),
            [CompileSource::Compiled, CompileSource::Compiled]
        );
    }
}
