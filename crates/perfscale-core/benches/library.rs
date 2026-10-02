//! Per-call library benchmarks (issue #4): what one `${alias.fn(...)}` costs
//! after burn removed the per-run compilation.
//!
//! - `wasm_call_inline` — a WASM component call with no tokio runtime in
//!   scope (the lint/unit-test path): JSON arg marshal + fuel reset + string
//!   lift/lower, no thread hop.
//! - `wasm_call_blocking_hop` — the same call from a runtime worker thread
//!   (the runner's hot path: `block_in_place` on multi-thread runtimes).
//! - `wasm_call_current_thread_hop` — from a single-threaded runtime, where
//!   the call must hop through the blocking pool.
//! - `std_random_call` — the native built-in, the no-WASM floor.
//!
//! The fixture component is the SDK `hello` example, built once per bench
//! process (skipped with an eprintln when wasm32-wasip2 is not installed).
//! Run with `cargo bench -p perfscale-core --features wasm-libs --bench library`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, Criterion};
use serde_json::Value;

use perfscale_core::library::{validate_libraries, CallCtx, LibraryInstance, LibraryRef};

fn fixture_hello() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let installed = std::process::Command::new("rustup")
            .args(["target", "list", "--installed"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|l| l.trim() == "wasm32-wasip2")
            })
            .unwrap_or(false);
        if !installed {
            eprintln!("skipping WASM library benches: wasm32-wasip2 target not installed");
            return None;
        }
        let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let target = ws.join("target/wasm-libs-fixtures");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "--release", "--target", "wasm32-wasip2"])
            .arg("--manifest-path")
            .arg(ws.join("crates/perfscale-library-sdk/examples/hello/Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .status()
            .ok()?;
        if !status.success() {
            eprintln!("failed to build the hello fixture component");
            return None;
        }
        Some(target.join("wasm32-wasip2/release/perfscale_hello_library.wasm"))
    })
    .as_deref()
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
        settings_json: r#"{"vus":10,"seed":42}"#,
    }
}

fn wasm_instance() -> Option<Box<dyn LibraryInstance>> {
    let path = fixture_hello()?;
    // Without the wasm-libs feature (plain `cargo bench`) this resolve fails
    // cleanly — skip rather than panic.
    let resolved = validate_libraries(&[lib_ref(path)], false, None).ok()?;
    Some(resolved[0].provider.instantiate(None, 7).unwrap())
}

fn bench_wasm_call_inline(c: &mut Criterion) {
    let Some(mut inst) = wasm_instance() else {
        return;
    };
    let args = [Value::from("world")];
    c.bench_function("wasm_call_inline", |b| {
        b.iter(|| {
            inst.call(std::hint::black_box(&ctx()), "greet", &args)
                .unwrap()
        })
    });
}

fn bench_wasm_call_blocking_hop(c: &mut Criterion) {
    let Some(mut inst) = wasm_instance() else {
        return;
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let args = [Value::from("world")];
    c.bench_function("wasm_call_blocking_hop", |b| {
        let _guard = rt.enter();
        b.iter(|| {
            inst.call(std::hint::black_box(&ctx()), "greet", &args)
                .unwrap()
        })
    });
}

fn bench_wasm_call_current_thread_hop(c: &mut Criterion) {
    let Some(mut inst) = wasm_instance() else {
        return;
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let args = [Value::from("world")];
    c.bench_function("wasm_call_current_thread_hop", |b| {
        let _guard = rt.enter();
        b.iter(|| {
            inst.call(std::hint::black_box(&ctx()), "greet", &args)
                .unwrap()
        })
    });
}

fn bench_std_random_call(c: &mut Criterion) {
    let resolved = validate_libraries(
        &[LibraryRef {
            use_: "@std/random@v1".into(),
            sha256: None,
            r#as: None,
            capabilities: None,
            with: None,
            secret: None,
            allow: None,
            deny: None,
            log: None,
        }],
        false,
        None,
    )
    .unwrap();
    let mut inst = resolved[0].provider.instantiate(None, 7).unwrap();
    c.bench_function("std_random_call", |b| {
        b.iter(|| {
            inst.call(std::hint::black_box(&ctx()), "uuid4", &[])
                .unwrap()
        })
    });
}

criterion_group!(
    benches,
    bench_wasm_call_inline,
    bench_wasm_call_blocking_hop,
    bench_wasm_call_current_thread_hop,
    bench_std_random_call
);
criterion_main!(benches);
