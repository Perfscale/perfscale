//! End-to-end self-update tests against a mock release server.
//!
//! `PERFSCALE_UPDATE_API_BASE` / `PERFSCALE_UPDATE_DOWNLOAD_BASE` point the
//! binary at a local wiremock instance that serves a fake "latest release"
//! feed, a fake platform asset, and its sha256sums.txt. The binary under test
//! is a *copy* of the real executable in a temp dir, so the replace step
//! swaps a real file without touching the build artefact.

use std::path::PathBuf;
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use predicates::prelude::*;
use serial_test::file_serial;
use sha2::Digest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(data))
}

fn platform_artifact() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "perfscale-linux-amd64",
        ("linux", "aarch64") => "perfscale-linux-arm64",
        ("macos", "aarch64") => "perfscale-darwin-arm64",
        ("macos", "x86_64") => "perfscale-darwin-amd64",
        ("windows", "x86_64") => "perfscale-windows-amd64.exe",
        ("windows", "aarch64") => "perfscale-windows-arm64.exe",
        other => panic!("no release artifact for test platform {other:?}"),
    }
}

/// Copy the built binary into a temp dir so self-update replaces the copy.
fn binary_copy(dir: &tempfile::TempDir) -> PathBuf {
    let name = if cfg!(windows) {
        "perfscale.exe"
    } else {
        "perfscale"
    };
    let copy = dir.path().join(name);
    std::fs::copy(cargo_bin("perfscale"), &copy).unwrap();
    copy
}

/// Mock a release feed: `latest` returns `tag`, the platform asset download
/// returns `binary_body`, sha256sums.txt matches it.
async fn mock_release(server: &MockServer, tag: &str, binary_body: &[u8]) {
    let artifact = platform_artifact();

    Mock::given(method("GET"))
        .and(path("/repos/Perfscale/perfscale/releases/latest"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "tag_name": tag })),
        )
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/Perfscale/perfscale/releases/download/{tag}/{artifact}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(binary_body.to_vec()))
        .mount(server)
        .await;

    let sums = format!("{}  {}\n", sha256_hex(binary_body), artifact);
    Mock::given(method("GET"))
        .and(path(format!(
            "/Perfscale/perfscale/releases/download/{tag}/sha256sums.txt"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(sums))
        .mount(server)
        .await;
}

fn update_cmd(
    bin: &PathBuf,
    server: &MockServer,
    cache_dir: &tempfile::TempDir,
) -> assert_cmd::Command {
    let mut cmd = assert_cmd::Command::new(bin);
    cmd.env("PERFSCALE_UPDATE_API_BASE", server.uri())
        .env("PERFSCALE_UPDATE_DOWNLOAD_BASE", server.uri())
        // Redirect the version-check cache away from the real user cache dir.
        .env(
            if cfg!(windows) {
                "LOCALAPPDATA"
            } else {
                "XDG_CACHE_HOME"
            },
            cache_dir.path(),
        )
        .env("HOME", cache_dir.path())
        .timeout(Duration::from_secs(60));
    cmd
}

// ---------------------------------------------------------------------------

#[tokio::test]
#[file_serial(heavy_io)]
async fn self_update_replaces_binary_and_verifies_checksum() {
    let server = MockServer::start().await;
    let fake_new_binary = b"#!/bin/sh\necho fake next version\n";
    mock_release(&server, "v99.0.0", fake_new_binary).await;

    let dir = tempfile::tempdir().unwrap();
    let bin = binary_copy(&dir);
    let cache = tempfile::tempdir().unwrap();

    update_cmd(&bin, &server, &cache)
        .arg("self-update")
        .assert()
        .success()
        .stdout(predicate::str::contains("updated perfscale"))
        .stdout(predicate::str::contains("v99.0.0"))
        .stderr(predicate::str::contains("sha256 verified"));

    // The executable on disk is now the downloaded content.
    assert_eq!(std::fs::read(&bin).unwrap(), fake_new_binary);
}

#[tokio::test]
#[file_serial(heavy_io)]
async fn self_update_check_exits_10_when_update_available() {
    let server = MockServer::start().await;
    mock_release(&server, "v99.0.0", b"irrelevant").await;

    let dir = tempfile::tempdir().unwrap();
    let bin = binary_copy(&dir);
    let cache = tempfile::tempdir().unwrap();

    update_cmd(&bin, &server, &cache)
        .args(["self-update", "--check"])
        .assert()
        .code(10)
        .stdout(predicate::str::contains("update available: v99.0.0"));

    // --check must not touch the binary.
    assert_eq!(
        std::fs::read(&bin).unwrap(),
        std::fs::read(cargo_bin("perfscale")).unwrap()
    );
}

#[tokio::test]
#[file_serial(heavy_io)]
async fn self_update_check_exits_0_when_up_to_date() {
    let server = MockServer::start().await;
    // Latest == the version we just built.
    let current_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    mock_release(&server, &current_tag, b"irrelevant").await;

    let dir = tempfile::tempdir().unwrap();
    let bin = binary_copy(&dir);
    let cache = tempfile::tempdir().unwrap();

    update_cmd(&bin, &server, &cache)
        .args(["self-update", "--check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("up to date"));
}

#[tokio::test]
#[file_serial(heavy_io)]
async fn self_update_noop_when_already_latest_without_force() {
    let server = MockServer::start().await;
    let current_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    mock_release(&server, &current_tag, b"must never be installed").await;

    let dir = tempfile::tempdir().unwrap();
    let bin = binary_copy(&dir);
    let original = std::fs::read(&bin).unwrap();
    let cache = tempfile::tempdir().unwrap();

    update_cmd(&bin, &server, &cache)
        .arg("self-update")
        .assert()
        .success()
        .stdout(predicate::str::contains("already the latest version"));

    assert_eq!(
        std::fs::read(&bin).unwrap(),
        original,
        "binary must be untouched"
    );
}

#[tokio::test]
#[file_serial(heavy_io)]
async fn self_update_force_reinstalls_same_version() {
    let server = MockServer::start().await;
    let current_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    let fake = b"reinstalled same version";
    mock_release(&server, &current_tag, fake).await;

    let dir = tempfile::tempdir().unwrap();
    let bin = binary_copy(&dir);
    let cache = tempfile::tempdir().unwrap();

    update_cmd(&bin, &server, &cache)
        .args(["self-update", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("updated perfscale"));

    assert_eq!(std::fs::read(&bin).unwrap(), fake);
}

#[tokio::test]
#[file_serial(heavy_io)]
async fn self_update_rejects_corrupted_download() {
    let server = MockServer::start().await;
    let artifact = platform_artifact();

    Mock::given(method("GET"))
        .and(path("/repos/Perfscale/perfscale/releases/latest"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "tag_name": "v99.0.0" })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/Perfscale/perfscale/releases/download/v99.0.0/{artifact}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tampered contents".to_vec()))
        .mount(&server)
        .await;
    // Sums file advertises a digest that does NOT match the download.
    Mock::given(method("GET"))
        .and(path(
            "/Perfscale/perfscale/releases/download/v99.0.0/sha256sums.txt",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "0000000000000000000000000000000000000000000000000000000000000000  {artifact}\n"
        )))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let bin = binary_copy(&dir);
    let original = std::fs::read(&bin).unwrap();
    let cache = tempfile::tempdir().unwrap();

    update_cmd(&bin, &server, &cache)
        .arg("self-update")
        .assert()
        .failure()
        .stderr(predicate::str::contains("checksum"));

    assert_eq!(
        std::fs::read(&bin).unwrap(),
        original,
        "corrupted download must not be installed"
    );
}

#[tokio::test]
#[file_serial(heavy_io)]
async fn self_update_unreachable_feed_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let bin = binary_copy(&dir);
    let cache = tempfile::tempdir().unwrap();

    let mut cmd = assert_cmd::Command::new(&bin);
    cmd.env("PERFSCALE_UPDATE_API_BASE", "http://127.0.0.1:1")
        .env(
            if cfg!(windows) {
                "LOCALAPPDATA"
            } else {
                "XDG_CACHE_HOME"
            },
            cache.path(),
        )
        .env("HOME", cache.path())
        .timeout(Duration::from_secs(60))
        .arg("self-update")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "failed to look up the latest release",
        ))
        .stderr(predicate::str::contains("hint:"));
}
