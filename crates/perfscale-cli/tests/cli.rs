//! End-to-end CLI tests: arg validation and real engine runs.
//! "Not found" paths are made deterministic by pointing PATH at a directory
//! with nothing in it, rather than relying on the test environment lacking
//! k6/locust. Happy-path k6 tests are gated on k6 actually being installed
//! (see `k6_available`) since CI installs it but a bare dev machine may not.

use std::io::Write;
use std::time::Duration;

use assert_cmd::Command;
use predicates::prelude::*;
use serial_test::file_serial;

fn cmd() -> Command {
    Command::cargo_bin("perfscale").unwrap()
}

fn k6_available() -> bool {
    std::process::Command::new("k6")
        .arg("version")
        .output()
        .is_ok()
}

// ---------------------------------------------------------------------------
// Arg validation
// ---------------------------------------------------------------------------

#[test]
fn run_without_target_flag_fails() {
    cmd()
        .arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("required arguments"));
}

#[test]
fn run_with_multiple_target_flags_fails() {
    cmd()
        .args(["run", "--k6", "a.js", "--locust", "b.py"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn run_native_file_without_config_fails() {
    cmd()
        .args(["run", "-f", "test.yaml"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--config"));
}

#[test]
fn help_flag_lists_all_commands() {
    cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("run"))
        .stdout(predicate::str::contains("serve"))
        .stdout(predicate::str::contains("Examples:"))
        .stdout(predicate::str::contains("Documentation:"));
}

#[test]
fn run_help_shows_examples_engine_rule_and_docs_links() {
    cmd()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Exactly one of --k6 / --locust / -f",
        ))
        .stdout(predicate::str::contains("Examples:"))
        .stdout(predicate::str::contains("docs/cli/commands.md"))
        .stdout(predicate::str::contains("docs/yaml-reference.md"));
}

#[test]
fn serve_help_documents_endpoints_and_port_zero() {
    cmd()
        .args(["serve", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("POST /api/v1/metrics"))
        .stdout(predicate::str::contains("--port 0"));
}

#[test]
fn version_flag_prints_version() {
    cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("perfscale"));
}

#[test]
fn run_nonexistent_k6_file_reports_read_error() {
    cmd()
        .args(["run", "--k6", "/no/such/file.js"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to read"));
}

#[test]
fn errors_carry_hint_and_docs_sections() {
    // Missing test file → what happened, why, what -f expects, where the docs are.
    cmd()
        .args([
            "run",
            "-f",
            "/no/such/test.yaml",
            "-c",
            "/no/such/config.yaml",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error: failed to read"))
        .stderr(predicate::str::contains("cause:"))
        .stderr(predicate::str::contains("hint:"))
        .stderr(predicate::str::contains("docs/yaml-reference.md"));
}

// ---------------------------------------------------------------------------
// Deterministic "binary not found" paths (PATH pointed at an empty dir)
// ---------------------------------------------------------------------------

#[test]
fn run_k6_not_found_in_path_reports_friendly_error() {
    let empty_dir = tempfile::tempdir().unwrap();
    let mut script = tempfile::Builder::new().suffix(".js").tempfile().unwrap();
    writeln!(script, "export default function() {{}}").unwrap();

    cmd()
        .env("PATH", empty_dir.path())
        .args(["run", "--k6", script.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("k6 not found in PATH"))
        .stderr(predicate::str::contains("built-in engine"))
        .stderr(predicate::str::contains("docs/getting-started.md"));
}

#[test]
fn run_locust_not_found_in_path_reports_friendly_error() {
    let empty_dir = tempfile::tempdir().unwrap();
    cmd()
        .env("PATH", empty_dir.path())
        .args(["run", "--locust", "whatever.py"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pip install locust"));
}

// ---------------------------------------------------------------------------
// Native engine (-f/-c)
// ---------------------------------------------------------------------------

#[test]
fn run_native_with_invalid_test_file_reports_schema_error() {
    let mut test_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(
        test_file,
        "steps:\n  - name: bad\n    with:\n      url: https://example.com\n"
    )
    .unwrap();

    let mut config_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(config_file, "vus: 1\nduration: 1s\n").unwrap();

    cmd()
        .args([
            "run",
            "-f",
            test_file.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("schema validation failed"));
}

#[test]
fn run_native_with_malformed_config_reports_yaml_error() {
    let mut test_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(
        test_file,
        "steps:\n  - use: std/log@v1\n    with:\n      message: hi\n"
    )
    .unwrap();

    let mut config_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(config_file, "vus: [this, is, not, a, number]\n").unwrap();

    cmd()
        .args([
            "run",
            "-f",
            test_file.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("schema validation failed"));
}

#[test]
#[file_serial(heavy_io)]
fn run_native_sleep_only_test_succeeds() {
    let mut test_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(
        test_file,
        "steps:\n  - use: std/log@v1\n    with:\n      message: hello-from-test\n  - use: std/sleep@v1\n    with:\n      ms: 10\n"
    )
    .unwrap();

    let mut config_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(config_file, "vus: 1\nduration: 1s\n").unwrap();

    cmd()
        .args([
            "run",
            "-f",
            test_file.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello-from-test"));
}

#[tokio::test]
#[file_serial(heavy_io)]
async fn run_native_with_report_forwards_summary_to_url() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/api/v1/metrics"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let mut test_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(
        test_file,
        "steps:\n  - use: std/log@v1\n    with:\n      message: reported\n"
    )
    .unwrap();

    let mut config_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(config_file, "vus: 1\nduration: 1s\n").unwrap();

    // assert_cmd's Command is blocking, so run it on a blocking thread to avoid
    // starving the current-thread-friendly async test of progress.
    let test_path = test_file.path().to_path_buf();
    let config_path = config_file.path().to_path_buf();
    let report_url = server.uri();
    tokio::task::spawn_blocking(move || {
        cmd()
            .args([
                "run",
                "-f",
                test_path.to_str().unwrap(),
                "-c",
                config_path.to_str().unwrap(),
                "--report",
                &report_url,
            ])
            .assert()
            .success();
    })
    .await
    .unwrap();

    server.verify().await;
}

// ---------------------------------------------------------------------------
// k6 engine (gated on a real k6 install — CI installs it; a bare dev box may not)
// ---------------------------------------------------------------------------

#[test]
#[file_serial(heavy_io)]
fn run_k6_trivial_script_succeeds() {
    if !k6_available() {
        eprintln!("skipping: k6 not installed");
        return;
    }
    let mut script = tempfile::Builder::new().suffix(".js").tempfile().unwrap();
    writeln!(script, "export default function() {{}}").unwrap();

    cmd()
        .args(["run", "--k6", script.path().to_str().unwrap()])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

#[tokio::test]
#[file_serial(heavy_io)]
async fn serve_binds_and_answers_health_check() {
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("perfscale"))
        .args(["serve", "--port", "18453"])
        .spawn()
        .expect("spawn perfscale serve");

    // Give the server a moment to bind before polling it.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let resp = reqwest::get("http://127.0.0.1:18453/health")
        .await
        .expect("GET /health");
    assert!(resp.status().is_success());

    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// lint
// ---------------------------------------------------------------------------

#[test]
fn lint_valid_files_exits_zero_with_checkmarks() {
    let mut test_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(
        test_file,
        "steps:\n  - use: std/http@v1\n    with:\n      url: https://example.com\n"
    )
    .unwrap();
    let mut config_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(config_file, "vus: 5\nduration: 30s\n").unwrap();

    cmd()
        .args([
            "lint",
            test_file.path().to_str().unwrap(),
            config_file.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("(test definition) — ok"))
        .stdout(predicate::str::contains("(config) — ok"));
}

#[test]
fn lint_typo_gets_did_you_mean_and_exit_one() {
    let mut test_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(
        test_file,
        "steps:\n  - use: std/http@v1\n    with:\n      url: https://x\n    chek:\n      status: 200\n"
    )
    .unwrap();

    cmd()
        .args(["lint", test_file.path().to_str().unwrap()])
        .assert()
        .failure()
        .stdout(predicate::str::contains("unknown field 'chek'"))
        .stdout(predicate::str::contains("did you mean 'check'?"))
        .stdout(predicate::str::contains("docs/yaml-reference.md"));
}

#[test]
fn lint_missing_use_shows_fix_with_action_list() {
    let mut test_file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(
        test_file,
        "steps:\n  - name: nope\n    with:\n      url: https://x\n"
    )
    .unwrap();

    cmd()
        .args(["lint", test_file.path().to_str().unwrap()])
        .assert()
        .failure()
        .stdout(predicate::str::contains("must name an action"))
        .stdout(predicate::str::contains("fix:"))
        .stdout(predicate::str::contains("std/http@v1"));
}

#[test]
fn lint_schema_override_forces_config_validation() {
    // A file with `steps:` would auto-detect as test; forcing config must flag it.
    let mut file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
    writeln!(file, "steps: []\n").unwrap();

    cmd()
        .args(["lint", "--schema", "config", file.path().to_str().unwrap()])
        .assert()
        .failure()
        .stdout(predicate::str::contains("unknown field 'steps'"));
}

#[test]
fn lint_missing_file_is_a_cli_error_with_hint() {
    cmd()
        .args(["lint", "/no/such/file.yaml"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to read"))
        .stderr(predicate::str::contains("hint:"));
}

#[test]
fn lint_shipped_examples_are_clean() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    cmd()
        .args([
            "lint",
            &format!("{root}/hello.test.yaml"),
            &format!("{root}/hello.config.yaml"),
        ])
        .assert()
        .success();
}
