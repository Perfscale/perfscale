//! End-usage workflow tests: full user journeys through the compiled binary —
//! the exact command lines a user would type, including the `run` → `serve`
//! reporting loop between two real processes.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use predicates::prelude::*;
use serial_test::file_serial;

fn k6_available() -> bool {
    Command::new("k6").arg("version").output().is_ok()
}

fn locust_available() -> bool {
    Command::new("locust").arg("--version").output().is_ok()
}

/// A `perfscale serve` child on an OS-assigned port, stdout captured.
struct ServeProc {
    child: Child,
    url: String,
    reader: BufReader<std::process::ChildStdout>,
}

impl ServeProc {
    /// Spawn `perfscale serve --port 0` and parse the bound address from its
    /// startup line (`perfscale serve listening on http://0.0.0.0:PORT`).
    fn start() -> Self {
        let mut child = Command::new(cargo_bin("perfscale"))
            .args(["serve", "--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn perfscale serve");

        let mut reader = BufReader::new(child.stdout.take().expect("stdout piped"));
        let mut first_line = String::new();
        reader
            .read_line(&mut first_line)
            .expect("read serve startup line");

        let port = first_line
            .trim()
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or_else(|| panic!("no port in serve startup line: {first_line:?}"));

        Self {
            child,
            url: format!("http://127.0.0.1:{port}"),
            reader,
        }
    }

    /// Read captured stdout lines until `pattern` appears (or ~5s passes).
    fn wait_for_output(&mut self, pattern: &str) -> Vec<String> {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut line = String::new();
                loop {
                    line.clear();
                    match self.reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let done = line.contains(pattern);
                            if tx.send(line.trim_end().to_string()).is_err() || done {
                                break;
                            }
                        }
                    }
                }
            });

            let mut lines = Vec::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(l) => {
                        let found = l.contains(pattern);
                        lines.push(l);
                        if found {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            // Kill before leaving the scope so the reader thread's read_line
            // unblocks (EOF) and the scope can join it.
            let _ = self.child.kill();
            lines
        })
    }
}

impl Drop for ServeProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_temp(suffix: &str, content: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new().suffix(suffix).tempfile().unwrap();
    write!(f, "{content}").unwrap();
    f
}

// ---------------------------------------------------------------------------
// The full run → serve reporting loop, two real processes
// ---------------------------------------------------------------------------

#[test]
#[file_serial(heavy_io)]
fn run_reports_summary_to_live_serve_instance() {
    let mut serve = ServeProc::start();

    // Deliberately log-only with no sleep: this spins hundreds of thousands of
    // iterations in 1s. Regression test for the 413 Payload Too Large bug —
    // only the summary block may be forwarded to --report, never the full log.
    let test_file = write_temp(
        ".yaml",
        "steps:\n  - use: std/log@v1\n    with:\n      message: loop-test\n",
    );
    let config_file = write_temp(".yaml", "vus: 1\nduration: 1s\n");

    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args([
            "run",
            "-f",
            test_file.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
            "--report",
            &serve.url,
        ])
        .timeout(Duration::from_secs(30))
        .assert()
        .success()
        .stdout(predicate::str::contains("loop-test"));

    let serve_output = serve.wait_for_output("metrics batch");
    let joined = serve_output.join("\n");
    assert!(joined.contains("metrics batch"), "serve printed:\n{joined}");
}

#[test]
#[file_serial(heavy_io)]
fn report_url_can_come_from_config_file_instead_of_flag() {
    let mut serve = ServeProc::start();

    let test_file = write_temp(
        ".yaml",
        "steps:\n  - use: std/log@v1\n    with:\n      message: from-config-report\n",
    );
    let config_file = write_temp(
        ".yaml",
        &format!("vus: 1\nduration: 1s\nreport:\n  url: {}\n", serve.url),
    );

    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args([
            "run",
            "-f",
            test_file.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
        ])
        .timeout(Duration::from_secs(30))
        .assert()
        .success();

    // Wait for the payload itself, not just the batch header — the summary
    // lines are printed after it.
    let serve_output = serve.wait_for_output("iterations");
    let joined = serve_output.join("\n");
    assert!(joined.contains("metrics batch"), "serve printed:\n{joined}");
    assert!(
        joined.contains("iterations"),
        "summary lines forwarded, serve printed:\n{joined}"
    );
}

#[test]
#[file_serial(heavy_io)]
fn run_with_unreachable_report_url_still_succeeds() {
    // Reporting is best-effort: a dead collector must not fail the run itself.
    let test_file = write_temp(
        ".yaml",
        "steps:\n  - use: std/log@v1\n    with:\n      message: no-collector\n",
    );
    let config_file = write_temp(".yaml", "vus: 1\nduration: 1s\n");

    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args([
            "run",
            "-f",
            test_file.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
            "--report",
            "http://127.0.0.1:1",
        ])
        .timeout(Duration::from_secs(30))
        .assert()
        .success()
        .stderr(predicate::str::contains("[report] failed to reach"));
}

// ---------------------------------------------------------------------------
// Native engine summary content (what the user actually reads)
// ---------------------------------------------------------------------------

#[test]
#[file_serial(heavy_io)]
fn native_run_prints_k6_compatible_summary_block() {
    let test_file = write_temp(
        ".yaml",
        "steps:\n  - use: std/sleep@v1\n    with:\n      ms: 10\n",
    );
    let config_file = write_temp(".yaml", "vus: 3\nduration: 1s\n");

    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args([
            "run",
            "-f",
            test_file.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
        ])
        .timeout(Duration::from_secs(30))
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "vus....................: 3 min=1 max=3",
        ))
        .stdout(predicate::str::contains("iterations"))
        .stderr(predicate::str::contains("Done —"));
}

#[test]
#[file_serial(heavy_io)]
fn native_run_shows_check_failures_on_stderr_but_exits_zero() {
    // A failing assertion is load-test feedback, not a CLI error: the run
    // completes and reports, mirroring k6's default behaviour without thresholds.
    let test_file = write_temp(
        ".yaml",
        "steps:\n  - use: std/http@v1\n    with:\n      url: http://127.0.0.1:1/\n      timeout: 200\n    check:\n      status: 200\n",
    );
    let config_file = write_temp(".yaml", "vus: 1\nduration: 1s\n");

    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args([
            "run",
            "-f",
            test_file.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
        ])
        .timeout(Duration::from_secs(30))
        .assert()
        .success()
        .stdout(predicate::str::contains("http_req_failed........: 100.00%"))
        .stderr(predicate::str::contains("FAIL"));
}

// ---------------------------------------------------------------------------
// WebSocket end-to-end: real binary against a local echo server
// ---------------------------------------------------------------------------

/// A WebSocket echo server on an OS-assigned port. The returned runtime keeps
/// the accept loop alive; hold it for the duration of the test.
fn ws_echo_server() -> (tokio::runtime::Runtime, String) {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::Message;

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let url = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                        return;
                    };
                    while let Some(Ok(msg)) = ws.next().await {
                        match msg {
                            Message::Text(t) => {
                                if ws.send(Message::Text(t)).await.is_err() {
                                    break;
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });
        format!("ws://{addr}")
    });
    (rt, url)
}

#[test]
#[file_serial(heavy_io)]
fn websocket_live_connection_full_journey() {
    let (_rt, url) = ws_echo_server();

    // The full live-connection lifecycle plus a one-shot session — the same
    // shape as examples/websocket.test.yaml, pointed at the local echo.
    let test_file = write_temp(
        ".yaml",
        &format!(
            r#"steps:
  - name: open
    use: std/ws-connect@v1
    with: {{ url: "{url}" }}
    outputs: feed
  - name: send
    use: std/ws-send@v1
    with:
      id: "${{{{ feed.id }}}}"
      send: "sub-${{seq}}"
      repeat: 2
  - name: recv
    use: std/ws-recv@v1
    with:
      id: "${{{{ feed.id }}}}"
      until_contains: "sub-2"
    check:
      message_contains: "sub-1"
      messages_count_gte: 2
  - name: close
    use: std/ws-close@v1
    with: {{ id: "${{{{ feed.id }}}}" }}
  - name: one-shot
    use: std/ws@v1
    with:
      url: "{url}"
      messages:
        - send: "ping-${{uuid}}"
          until_contains: "ping-"
  - use: std/sleep@v1
    with: {{ ms: 100 }}
"#
        ),
    );
    let config_file = write_temp(".yaml", "vus: 1\nduration: 1s\n");

    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args([
            "run",
            "-f",
            test_file.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
        ])
        .timeout(Duration::from_secs(30))
        .assert()
        .success()
        // Custom WS metrics land in the summary block…
        .stdout(predicate::str::contains("ws_msgs_sent"))
        .stdout(predicate::str::contains("ws_msgs_received"))
        // …including the message-RTT histogram with its sample count…
        .stdout(predicate::str::is_match(r"ws_msg_rtt: avg=.* count=\d+").unwrap())
        // …and the handshake/session samples feed the shared histogram.
        .stdout(predicate::str::contains("http_req_duration"))
        // Both message asserts passed; nothing failed.
        .stderr(predicate::str::contains("FAIL").not());
}

#[test]
#[file_serial(heavy_io)]
fn websocket_unmet_until_rule_fails_step_but_run_completes() {
    let (_rt, url) = ws_echo_server();

    // The until rule can never match (the echo returns what was sent), so the
    // recv step fails — load-test feedback on stderr, exit code still 0.
    let test_file = write_temp(
        ".yaml",
        &format!(
            r#"steps:
  - use: std/ws-connect@v1
    with: {{ url: "{url}" }}
    outputs: feed
  - use: std/ws-send@v1
    with: {{ id: "${{{{ feed.id }}}}", send: "hello" }}
  - use: std/ws-recv@v1
    with:
      id: "${{{{ feed.id }}}}"
      until_contains: "never-arrives"
      timeout: 200
  - use: std/sleep@v1
    with: {{ ms: 200 }}
"#
        ),
    );
    let config_file = write_temp(".yaml", "vus: 1\nduration: 1s\n");

    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args([
            "run",
            "-f",
            test_file.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
        ])
        .timeout(Duration::from_secs(30))
        .assert()
        .success()
        .stdout(predicate::str::contains("ws_msgs_sent"))
        .stderr(predicate::str::contains(
            "timeout before the stopping rule was reached",
        ));
}

// ---------------------------------------------------------------------------
// Shipped examples work as-is for engines that don't need the network
// ---------------------------------------------------------------------------

#[test]
#[file_serial(heavy_io)]
fn shipped_k6_example_runs_when_k6_installed() {
    if !k6_available() {
        eprintln!("skipping: k6 not installed");
        return;
    }
    // Trim the example to 1 iteration so the suite stays fast: reuse the file's
    // default function but override load via a wrapper script.
    let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/hello.k6.js");
    let source = std::fs::read_to_string(example).unwrap();
    let trimmed = source
        .replace("vus: 5", "vus: 1")
        .replace("duration: '30s'", "iterations: 1");
    let script = write_temp(".js", &trimmed);

    // k6 writes its whole report to stdout when piped — assert there.
    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args(["run", "--k6", script.path().to_str().unwrap()])
        .timeout(Duration::from_secs(60))
        .assert()
        .success()
        .stdout(predicate::str::contains("1 complete"));
}

#[test]
#[file_serial(heavy_io)]
fn locust_headless_run_produces_unified_summary() {
    if !locust_available() {
        eprintln!("skipping: locust not installed");
        return;
    }
    let script = write_temp(
        ".py",
        "from locust import HttpUser, task\nclass U(HttpUser):\n    @task\n    def t(self):\n        self.client.get('/')\n",
    );
    let config_file = write_temp(".yaml", "vus: 1\nduration: 2s\n");

    // Host is unreachable on purpose — requests fail, but the run itself must
    // complete and emit the k6-compatible summary parsed from locust's CSV.
    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args([
            "run",
            "--locust",
            script.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
            "--host",
            "http://127.0.0.1:1",
        ])
        .timeout(Duration::from_secs(60))
        .assert()
        .success()
        .stdout(predicate::str::contains("http_reqs"));
}

// ---------------------------------------------------------------------------
// Engine crash detection (regression: a startup crash must exit 1, not 0)
// ---------------------------------------------------------------------------

#[test]
#[file_serial(heavy_io)]
fn run_k6_broken_script_exits_nonzero_with_no_metrics() {
    if !k6_available() {
        eprintln!("skipping: k6 not installed");
        return;
    }
    // Invalid JS: k6 fails before ever running an iteration, so no
    // http_req_* summary line is ever produced.
    let script = write_temp(".js", "this is not valid javascript {{{");

    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args(["run", "--k6", script.path().to_str().unwrap()])
        .timeout(Duration::from_secs(30))
        .assert()
        .failure()
        .stderr(predicate::str::contains("before producing any results"));
}
