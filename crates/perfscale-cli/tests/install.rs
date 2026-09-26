//! `perfscale install` end-to-end: wiremock serves the artifact, install
//! fetches + verifies + pins it, and lint afterwards runs fully offline.
//! The lint-green path needs a real WASM component, so it reuses the SDK
//! hello example (built on demand, skipped when wasm32-wasip2 is missing —
//! same policy as the core wasm tests).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use assert_cmd::Command;
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cmd() -> Command {
    Command::cargo_bin("perfscale").unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
                eprintln!("skipping component-backed lint check: wasm32-wasip2 not installed");
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
async fn install_https_pins_lock_and_cache_then_lint_is_offline() {
    let Some(component) = hello_component() else {
        return;
    };
    let bytes = std::fs::read(component).unwrap();
    let sha = sha256_hex(&bytes);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/lib.wasm"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
        .expect(1) // exactly one network hit: install itself
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let url = format!("{}/lib.wasm", server.uri());
    let yaml = write_yaml(
        dir.path(),
        "test.yaml",
        &format!(
            "libraries:\n  - use: \"{url}\"\n    sha256: \"{sha}\"\nsteps:\n  - use: std/log@v1\n    with: {{ message: hi }}\n"
        ),
    );

    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success()
        .stdout(predicates::str::contains(format!("installed {url}")))
        .stdout(predicates::str::contains("perfscale.lock"));

    // Lock pins the exact use string; cache holds the artifact by digest.
    let lock = std::fs::read_to_string(dir.path().join("perfscale.lock")).unwrap();
    assert!(lock.contains(&format!("use = \"{url}\"")), "{lock}");
    assert!(lock.contains(&format!("sha256 = \"{sha}\"")), "{lock}");
    assert!(cache.path().join(format!("libraries/{sha}.wasm")).is_file());

    // The server is gone: lint resolves through lock + cache, fully offline.
    drop(server);
    cmd()
        .arg("lint")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success();

    // Second install: pinned + cached, no re-download.
    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("up to date"));
}

#[tokio::test]
async fn install_https_sha_mismatch_is_a_hard_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/lib.wasm"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"not the bytes you pinned"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let url = format!("{}/lib.wasm", server.uri());
    let wrong = "a".repeat(64);
    let yaml = write_yaml(
        dir.path(),
        "test.yaml",
        &format!(
            "libraries:\n  - use: \"{url}\"\n    sha256: \"{wrong}\"\nsteps:\n  - use: std/log@v1\n    with: {{ message: hi }}\n"
        ),
    );

    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("sha256 mismatch"))
        .stderr(predicates::str::contains(&wrong));
    assert!(
        !dir.path().join("perfscale.lock").exists(),
        "a failed verification must not write a lock"
    );
}

#[tokio::test]
async fn install_https_without_declared_sha256_fails() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("{}/lib.wasm", server.uri());
    let yaml = write_yaml(
        dir.path(),
        "test.yaml",
        &format!("libraries:\n  - use: \"{url}\"\nsteps:\n  - use: std/log@v1\n    with: {{ message: hi }}\n"),
    );
    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", tempfile::tempdir().unwrap().path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("require a `sha256:` field"));
}

#[tokio::test]
async fn lint_remote_library_without_lock_points_at_install() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = write_yaml(
        dir.path(),
        "test.yaml",
        &format!(
            "libraries:\n  - use: \"https://example.com/l.wasm\"\n    sha256: \"{}\"\nsteps:\n  - use: std/log@v1\n    with: {{ message: hi }}\n",
            "a".repeat(64)
        ),
    );
    cmd()
        .arg("lint")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", tempfile::tempdir().unwrap().path())
        .assert()
        .failure()
        // Import-resolution failures are lint findings (stdout), exit 1.
        .stdout(predicates::str::contains("no perfscale.lock found"))
        .stdout(predicates::str::contains("perfscale install"));
}

#[tokio::test]
async fn install_without_remote_libraries_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = write_yaml(
        dir.path(),
        "test.yaml",
        "steps:\n  - use: std/log@v1\n    with: { message: hi }\n",
    );
    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", tempfile::tempdir().unwrap().path())
        .assert()
        .success()
        .stdout(predicates::str::contains("no remote libraries"));
}

/// Burn cache (RFC 005 burn, phase 1): installing a local `./x.wasm` library
/// precompiles it into `<cache>/libraries/<sha256>.cwasm` — no lockfile
/// entry (nothing to pin for a local file). A repeat install skips the burn.
#[tokio::test]
async fn install_burns_local_wasm_libraries_without_a_lockfile() {
    let Some(component) = hello_component() else {
        return;
    };
    let bytes = std::fs::read(component).unwrap();
    let sha = sha256_hex(&bytes);

    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::copy(component, dir.path().join("lib.wasm")).unwrap();
    let yaml = write_yaml(
        dir.path(),
        "test.yaml",
        "libraries:\n  - use: ./lib.wasm\n    as: hello\nsteps:\n  - use: std/log@v1\n    with: { message: hi }\n",
    );

    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("burned"));
    assert!(
        cache.path().join(format!("libraries/{sha}.cwasm")).is_file(),
        "burn artifact written"
    );
    assert!(
        !dir.path().join("perfscale.lock").exists(),
        "local libraries get no lockfile"
    );

    // Idempotent: a valid burn artifact is not recompiled.
    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("up to date"));
}

/// Remote installs burn too: next to the `.wasm` artifact and the lock pin,
/// the cache gains the precompiled `.cwasm`.
#[tokio::test]
async fn install_https_also_writes_the_burn_artifact() {
    let Some(component) = hello_component() else {
        return;
    };
    let bytes = std::fs::read(component).unwrap();
    let sha = sha256_hex(&bytes);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/lib.wasm"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let url = format!("{}/lib.wasm", server.uri());
    let yaml = write_yaml(
        dir.path(),
        "test.yaml",
        &format!(
            "libraries:\n  - use: \"{url}\"\n    sha256: \"{sha}\"\nsteps:\n  - use: std/log@v1\n    with: {{ message: hi }}\n"
        ),
    );

    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("burned"));
    assert!(cache.path().join(format!("libraries/{sha}.wasm")).is_file());
    assert!(
        cache.path().join(format!("libraries/{sha}.cwasm")).is_file(),
        "burn artifact written next to the .wasm"
    );

    // Second install: pin + artifact + burn all up to date, one network hit
    // total (asserted by the mock's expect(1)).
    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("up to date"));
}
