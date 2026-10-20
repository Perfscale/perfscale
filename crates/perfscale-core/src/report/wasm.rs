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

use std::path::{Component as PathComponent, Path, PathBuf};

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Store, StoreLimitsBuilder};
use wasmtime_wasi::WasiCtx;
use wasmtime_wasi_http::WasiHttpCtx;

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

/// A compiled WASM reporter: compile once per run (at delivery), instantiate
/// once for the single `on-report` call.
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
        let bytes = std::fs::read(module)
            .map_err(|e| format!("reporter '{}': failed to read: {e}", display()))?;
        if bytes.len() > MAX_COMPONENT_BYTES {
            return Err(format!(
                "reporter '{}': component is {} MiB — the limit is {} MiB",
                display(),
                bytes.len() >> 20,
                MAX_COMPONENT_BYTES >> 20,
            ));
        }
        let engine = engine()?;
        let component = Component::new(engine, &bytes).map_err(|e| {
            format!(
                "reporter '{}': failed to compile as a WASM component: {e}",
                display()
            )
        })?;

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
        let mut linker = build_linker(engine, &display(), false)?;
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
    pub fn check(module: &Path) -> Result<(), String> {
        Self::load(module, None).map(|_| ())
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
}
