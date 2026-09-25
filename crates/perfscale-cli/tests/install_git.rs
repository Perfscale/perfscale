//! `perfscale install` git-flow end-to-end (RFC 005 phase 3): real clones
//! from local `file://` fixture repos exercise git_fetch / ls_remote /
//! read_repo_artifact and the lock-pin semantics (tag pin, branch pin +
//! --refresh, digest verification, error paths). Like the core git-import
//! tests these shell out to the system git and skip when it is absent.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use assert_cmd::Command;
use sha2::{Digest, Sha256};

fn cmd() -> Command {
    Command::cargo_bin("perfscale").unwrap()
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run git in `repo`, asserting success; returns trimmed stdout.
fn git(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        // Host-level settings (signed tags, hooks, templates, aliases) must
        // not change fixture behavior.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fixture `git {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A fixture repository holding `bytes` as `lib.wasm`, committed on `main`
/// and tagged `v1`. Returns (guard, `file://` URL) — the repo lives as long
/// as the guard.
fn fixture_repo(bytes: &[u8]) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "--quiet", "-b", "main", "."]);
    // Local identity only — never touch the developer's global config.
    git(repo, &["config", "user.email", "t@t"]);
    git(repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("lib.wasm"), bytes).unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "--quiet", "-m", "lib"]);
    git(repo, &["tag", "v1"]);
    let url = format!("file://{}", repo.display());
    (dir, url)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn write_yaml(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("test.yaml");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    path
}

fn git_lib_yaml(url: &str, git_ref: &str, extra: &str) -> String {
    format!(
        "libraries:\n  - use: \"git+{url}@{git_ref}#lib.wasm\"\n{extra}steps:\n  - use: std/log@v1\n    with: {{ message: hi }}\n"
    )
}

/// A valid WASM component from the SDK examples (same policy as the core
/// wasm tests and tests/install.rs: build once, skip without wasm32-wasip2).
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

#[test]
fn install_git_tag_pins_commit_and_digest() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let bytes = b"fake-wasm-artifact-v1";
    let sha = sha256_hex(bytes);
    let (repo, url) = fixture_repo(bytes);
    let tag_sha = git(repo.path(), &["rev-parse", "v1"]);

    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let use_ = format!("git+{url}@v1#lib.wasm");
    let yaml = write_yaml(dir.path(), &git_lib_yaml(&url, "v1", ""));

    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success()
        .stdout(predicates::str::contains(format!("installed {use_}")))
        .stdout(predicates::str::contains("perfscale.lock"));

    // Lock pins the resolved commit AND the artifact digest.
    let lock = std::fs::read_to_string(dir.path().join("perfscale.lock")).unwrap();
    assert!(lock.contains(&format!("use = \"{use_}\"")), "{lock}");
    assert!(lock.contains(&format!("commit = \"{tag_sha}\"")), "{lock}");
    assert!(lock.contains(&format!("sha256 = \"{sha}\"")), "{lock}");
    assert!(cache.path().join(format!("libraries/{sha}.wasm")).is_file());

    // Remote gone: lock + cache make the second install a pure local hit —
    // it never reaches git_fetch (the up-to-date shortcut returns first).
    drop(repo);
    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("up to date"));
}

#[test]
fn install_git_offline_lint_with_real_component() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let Some(component) = hello_component() else {
        return;
    };
    let bytes = std::fs::read(component).unwrap();
    let (repo, url) = fixture_repo(&bytes);

    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let yaml = write_yaml(dir.path(), &git_lib_yaml(&url, "v1", ""));

    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success();

    // Origin repo deleted: lint resolves through lock + cache only, and the
    // cached artifact is a loadable component — fully offline green.
    drop(repo);
    cmd()
        .arg("lint")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success();
}

#[test]
fn install_git_branch_pin_is_stable_until_refresh() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (repo, url) = fixture_repo(b"branch-artifact-v1");
    let commit1 = git(repo.path(), &["rev-parse", "HEAD"]);
    let sha1 = sha256_hex(b"branch-artifact-v1");

    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let yaml = write_yaml(dir.path(), &git_lib_yaml(&url, "main", ""));
    let lock_path = dir.path().join("perfscale.lock");

    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success();
    let lock = std::fs::read_to_string(&lock_path).unwrap();
    assert!(lock.contains(&format!("commit = \"{commit1}\"")), "{lock}");

    // Advance the branch.
    std::fs::write(repo.path().join("lib.wasm"), b"branch-artifact-v2").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "--quiet", "-m", "v2"]);
    let commit2 = git(repo.path(), &["rev-parse", "HEAD"]);
    let sha2 = sha256_hex(b"branch-artifact-v2");
    assert_ne!(commit1, commit2);

    // Without --refresh the lock is the truth: the pin must NOT move. This
    // is deterministic by construction — the up-to-date shortcut (lock
    // entry + cached artifact) returns before git_fetch, so the branch-TTL
    // revalidation inside git_fetch is never reached.
    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("up to date"));
    let lock = std::fs::read_to_string(&lock_path).unwrap();
    assert!(
        lock.contains(&format!("commit = \"{commit1}\"")),
        "pin must stay at commit1 without --refresh: {lock}"
    );
    assert!(lock.contains(&format!("sha256 = \"{sha1}\"")), "{lock}");

    // --refresh re-resolves the ref and rewrites the pin.
    cmd()
        .arg("install")
        .arg(&yaml)
        .arg("--refresh")
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .success();
    let lock = std::fs::read_to_string(&lock_path).unwrap();
    assert!(lock.contains(&format!("commit = \"{commit2}\"")), "{lock}");
    assert!(lock.contains(&format!("sha256 = \"{sha2}\"")), "{lock}");
    assert!(cache
        .path()
        .join(format!("libraries/{sha2}.wasm"))
        .is_file());
}

#[test]
fn install_git_declared_sha256_mismatch_is_a_hard_error() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_repo, url) = fixture_repo(b"the-real-artifact");
    let actual = sha256_hex(b"the-real-artifact");
    let wrong = "a".repeat(64);

    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let yaml = write_yaml(
        dir.path(),
        &git_lib_yaml(&url, "v1", &format!("    sha256: \"{wrong}\"\n")),
    );

    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", cache.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("sha256 mismatch"))
        .stderr(predicates::str::contains(&wrong))
        .stderr(predicates::str::contains(&actual));
    assert!(
        !dir.path().join("perfscale.lock").exists(),
        "a failed verification must not write a lock"
    );
}

#[test]
fn install_git_unknown_ref_fails() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_repo, url) = fixture_repo(b"x");
    let dir = tempfile::tempdir().unwrap();
    let yaml = write_yaml(dir.path(), &git_lib_yaml(&url, "nonexistent", ""));

    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", tempfile::tempdir().unwrap().path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("git fetch failed"));
    assert!(!dir.path().join("perfscale.lock").exists());
}

#[test]
fn install_git_escape_path_fails_before_writing_lock() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_repo, url) = fixture_repo(b"x");
    let dir = tempfile::tempdir().unwrap();
    let yaml = write_yaml(
        dir.path(),
        &format!(
            "libraries:\n  - use: \"git+{url}@v1#../out.wasm\"\nsteps:\n  - use: std/log@v1\n    with: {{ message: hi }}\n"
        ),
    );

    cmd()
        .arg("install")
        .arg(&yaml)
        .env("PERFSCALE_CACHE_DIR", tempfile::tempdir().unwrap().path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("escapes the repository root"));
    assert!(!dir.path().join("perfscale.lock").exists());
}
