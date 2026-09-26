//! `perfscale burn` end-to-end: the derived binary carries the document's
//! WASM libraries inside itself and runs them with the source `.wasm` (and
//! the cache) gone. Skipped when wasm32-wasip2 is not installed (same policy
//! as the core wasm tests).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use assert_cmd::Command;

fn cmd() -> Command {
    Command::cargo_bin("perfscale").unwrap()
}

fn write_yaml(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    path
}

/// A valid WASM component from the SDK examples, built once per test
/// binary. `None` when the wasm32-wasip2 target is not installed.
fn hello_component() -> Option<&'static PathBuf> {
    static COMPONENT: OnceLock<Option<PathBuf>> = OnceLock::new();
    COMPONENT
        .get_or_init(|| {
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
                eprintln!("skipping burn e2e: wasm32-wasip2 not installed");
                return None;
            }
            let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .unwrap();
            let target = ws.join("target/wasm-libs-fixtures");
            let artifact = target.join("wasm32-wasip2/release/perfscale_hello_library.wasm");
            if artifact.is_file() {
                return Some(artifact);
            }
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args(["build", "--release", "--target", "wasm32-wasip2"])
                .arg("--manifest-path")
                .arg(ws.join("crates/perfscale-library-sdk/examples/hello/Cargo.toml"))
                .arg("--target-dir")
                .arg(&target)
                .status()
                .ok()?;
            status.success().then_some(artifact)
        })
        .as_ref()
}

#[tokio::test]
async fn burn_builds_a_binary_that_runs_its_libraries_standalone() {
    let Some(component) = hello_component() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let lib = dir.path().join("hello.wasm");
    std::fs::copy(component, &lib).unwrap();

    // The backend only responds when the WASM component actually produced
    // the greeting (std/log prints its message verbatim — token expansion
    // must be observed on the wire).
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/greet"))
        .and(wiremock::matchers::body_string_contains("hello, world!"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(1..)
        .mount(&server)
        .await;

    let test = write_yaml(
        dir.path(),
        "test.yaml",
        &format!(
            r#"steps:
  - use: std/http@v1
    with:
      method: POST
      url: {}/greet
      body: '{{ "msg": "${{hello.greet(world)}}" }}'
"#,
            server.uri()
        ),
    );
    let config = write_yaml(
        dir.path(),
        "config.yaml",
        "vus: 1\nduration: 1s\nlibraries:\n  - use: ./hello.wasm\n    as: hello\n",
    );
    let out = dir.path().join("perfscale-burned");

    cmd()
        .arg("burn")
        .arg("-f")
        .arg(&test)
        .arg("-c")
        .arg(&config)
        .arg("-o")
        .arg(&out)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("burned 1 library"))
        .stdout(predicates::str::contains("embedded"))
        .stdout(predicates::str::contains("sha256:"));

    // The .wasm is gone and the cache is empty: only the embedded payload
    // can satisfy the library.
    std::fs::rename(&lib, dir.path().join("hello.wasm.gone")).unwrap();
    let empty_cache = tempfile::tempdir().unwrap();
    Command::new(&out)
        .arg("run")
        .arg("-f")
        .arg(&test)
        .arg("-c")
        .arg(&config)
        .env("PERFSCALE_CACHE_DIR", empty_cache.path())
        .assert()
        .success();
    server.verify().await;

    // Sanity: the unburned binary fails on the same setup (the file is
    // missing and nothing is embedded) — the burn is what made it work.
    cmd()
        .arg("run")
        .arg("-f")
        .arg(&test)
        .arg("-c")
        .arg(&config)
        .env("PERFSCALE_CACHE_DIR", empty_cache.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("file not found"));
}
