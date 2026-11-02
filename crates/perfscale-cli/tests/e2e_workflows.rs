//! End-usage workflow tests: full user journeys through the compiled binary —
//! the exact command lines a user would type, including the `run` → `serve`
//! reporting loop between two real processes.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use predicates::prelude::*;
use prost::Message as _;
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

#[test]
#[file_serial(heavy_io)]
fn run_with_html_reporter_writes_a_self_contained_file() {
    let test_file = write_temp(
        ".yaml",
        "steps:\n  - use: std/sleep@v1\n    with:\n      ms: 5\n",
    );
    let config_file = write_temp(".yaml", "vus: 1\nduration: 1s\n");
    let out = tempfile::Builder::new().suffix(".html").tempfile().unwrap();
    // The reporter creates the file; start from a removed path (dropping a
    // NamedTempFile already deletes it).
    let out_path = out.path().to_path_buf();
    drop(out);
    assert!(!out_path.exists());

    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args([
            "run",
            "-f",
            test_file.path().to_str().unwrap(),
            "-c",
            config_file.path().to_str().unwrap(),
            "--report",
            &format!("html:{}", out_path.display()),
        ])
        .timeout(Duration::from_secs(30))
        .assert()
        .success()
        .stderr(predicate::str::contains("html report written"));

    let html = std::fs::read_to_string(&out_path).expect("html report written");
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert_eq!(html.matches("<html").count(), 1, "one <html> element");
    // Run identity and the steps section are present…
    assert!(html.contains("perfscale report"), "{html}");
    assert!(html.contains("id=\"steps\""), "{html}");
    // …and nothing external is referenced (offline/archivable artifact).
    assert!(!html.contains("src=\"http"), "{html}");
    assert!(!html.contains("href=\"http"), "{html}");
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

/// `std/pubsub@v1` memory roundtrip through the compiled binary: custom
/// pubsub metrics land in the run summary, and a `body_contains` check over
/// the joined received payloads passes.
#[test]
#[file_serial(heavy_io)]
fn pubsub_memory_roundtrip_full_journey() {
    let test_file = write_temp(
        ".yaml",
        r#"steps:
  - name: order events roundtrip
    use: std/pubsub@v1
    with:
      subject: orders.created
      publish: ["order-1 created", "order-2 created"]
      subscribe: { count: 2, until_contains: created, timeout_ms: 2000 }
    check:
      body_contains: order-2
  - use: std/sleep@v1
    with: { ms: 100 }
"#,
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
        // Custom pub/sub metrics land in the summary block…
        .stdout(predicate::str::contains("pubsub_msgs_published"))
        .stdout(predicate::str::contains("pubsub_msgs_received"))
        // …including the end-to-end latency histogram with its sample count…
        .stdout(predicate::str::is_match(r"pubsub_e2e_ms: avg=.* count=\d+").unwrap())
        // …and the body check passed; nothing failed.
        .stderr(predicate::str::contains("FAIL").not());
}

/// A subscribe-only `std/pubsub@v1` step that never receives its message
/// fails the step (load-test feedback on stderr) but the run still completes.
#[test]
#[file_serial(heavy_io)]
fn pubsub_subscribe_timeout_fails_step_but_run_completes() {
    let test_file = write_temp(
        ".yaml",
        r#"steps:
  - use: std/pubsub@v1
    with:
      subject: never.published
      subscribe: { count: 1, timeout_ms: 200 }
  - use: std/sleep@v1
    with: { ms: 200 }
"#,
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
        .stderr(predicate::str::contains(
            "subscribe timeout: 0 of 1 message(s) arrived",
        ));
}

/// Contract test: a Live Connection opened in `before:` setup does NOT leak
/// into VU iterations. The setup context (and its socket) is gone before any
/// VU starts, and N concurrent VUs must never share one socket — they would
/// race for each other's frames and the test would measure a mutex, not the
/// server. The connect itself succeeds and its id lands in `config`, but a
/// VU step using that id gets a clear error naming the cause.
#[test]
#[file_serial(heavy_io)]
fn websocket_connection_from_before_setup_does_not_leak_into_vus() {
    let (_rt, url) = ws_echo_server();

    let config_file = write_temp(
        ".yaml",
        &format!(
            r#"vus: 1
duration: 1s
before:
  - use: std/ws-connect@v1
    with: {{ url: "{url}" }}
    outputs: feed
"#
        ),
    );
    let test_file = write_temp(
        ".yaml",
        r#"steps:
  - name: use stale id
    use: std/ws-send@v1
    with:
      id: "${{ config.feed.id }}"
      send: "hello"
  - use: std/sleep@v1
    with: { ms: 200 }
"#,
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
        // Setup succeeded (the connect itself works), the run completes —
        // the failing send is load-test feedback, not a CLI error.
        .success()
        .stderr(predicate::str::contains("unknown connection id"))
        .stderr(predicate::str::contains("`before:` setup"));
}

/// `after:` is a first-class config section (one-time teardown steps). The
/// linter accepts a well-formed one — and still names problems inside its
/// steps.
#[test]
#[file_serial(heavy_io)]
fn config_after_section_lints_clean() {
    let config_file = write_temp(
        ".yaml",
        r#"vus: 1
duration: 1s
after:
  - use: std/kill_process@v1
    with: { name: web }
"#,
    );

    assert_cmd::Command::new(cargo_bin("perfscale"))
        .args(["lint", config_file.path().to_str().unwrap()])
        .timeout(Duration::from_secs(30))
        .assert()
        .stdout(predicate::str::contains("ok"));
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
    let config_file = write_temp(".yaml", "vus: 1\nduration: 1s\n");

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

// ---------------------------------------------------------------------------
// Streaming reporters (issue #12): Prometheus remote write fan-out.
// ---------------------------------------------------------------------------

/// A minimal HTTP sink for the remote-write endpoint: 200 OK everything,
/// record bodies. std-only (the e2e file is sync).
struct PromSink {
    url: String,
    bodies: std::sync::mpsc::Receiver<(String, Vec<u8>)>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl PromSink {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = tx.send(read_http_request(&mut stream));
                use std::io::Write as _;
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}/api/v1/write"),
            bodies: rx,
            join: Some(join),
        }
    }

    /// All bodies received so far, draining the channel.
    fn drain(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        while let Ok(b) = self.bodies.try_recv() {
            out.push(b);
        }
        out
    }

    /// Wait until at least `n` requests arrived (or ~10s pass), then drain.
    fn wait_for(&self, n: usize) -> Vec<(String, Vec<u8>)> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got = self.drain();
            if got.len() >= n || std::time::Instant::now() >= deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for PromSink {
    fn drop(&mut self) {
        // The listener thread blocks in `incoming`; the process exit reaps it.
        let _ = self.join.take();
    }
}

/// Read one HTTP request: headers, then the content-length body. Returns the
/// content-encoding header and the body.
fn read_http_request(stream: &mut std::net::TcpStream) -> (String, Vec<u8>) {
    use std::io::{BufRead, BufReader, Read};
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut content_length = 0usize;
    let mut encoding = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim().to_string();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = v.trim().parse().unwrap_or(0),
                "content-encoding" => encoding = v.trim().to_string(),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; content_length];
    let _ = reader.read_exact(&mut body);
    (encoding, body)
}

#[test]
#[file_serial(heavy_io)]
fn run_streams_to_prometheus_remote_write_and_serve_simultaneously() {
    let prom = PromSink::start();
    let mut serve = ServeProc::start();

    let test_file = write_temp(
        ".yaml",
        "steps:\n  - use: std/sleep@v1\n    with:\n      ms: 5\n    labels: { phase: smoke }\n",
    );
    let config_file = write_temp(
        ".yaml",
        &format!(
            "vus: 1\nduration: 1s\nreport:\n  labels: {{ testid: e2e-stream }}\n  reporters:\n    - use: std/report/prometheus-rw@v1\n      during_run: true\n      interval_ms: 200\n      labels: {{ env: staging }}\n      with: {{ url: {} }}\n    - use: std/report/serve@v1\n      during_run: true\n      interval_ms: 200\n      with: {{ url: {} }}\n",
            prom.url, serve.url
        ),
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

    // The remote-write endpoint received Snappy-framed protobuf batches with
    // merged labels and no quantile series.
    let reqs = prom.wait_for(1);
    assert!(!reqs.is_empty(), "no remote-write POSTs received");
    let (encoding, body) = &reqs[0];
    assert_eq!(encoding, "snappy");
    let raw = snap::raw::Decoder::new().decompress_vec(body).unwrap();
    let req =
        perfscale_core::report::prometheus_rw::PromWriteRequest::decode(raw.as_slice()).unwrap();
    assert!(!req.timeseries.is_empty());
    let labels_of = |ts: &perfscale_core::report::prometheus_rw::PromTimeSeries| {
        ts.labels
            .iter()
            .map(|l| (l.name.clone(), l.value.clone()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let first = labels_of(&req.timeseries[0]);
    assert_eq!(first.get("testid").map(String::as_str), Some("e2e-stream"));
    assert_eq!(first.get("env").map(String::as_str), Some("staging"));
    assert!(first.contains_key("run_id"));
    assert_eq!(first.get("worker").map(String::as_str), Some("0"));
    assert!(
        req.timeseries
            .iter()
            .all(|t| !labels_of(t).contains_key("quantile")),
        "no client-side quantile series"
    );

    // …and the serve reporter still got its live batches.
    let serve_output = serve.wait_for_output("live snapshot");
    assert!(
        serve_output.join("\n").contains("live snapshot"),
        "serve printed: {}",
        serve_output.join("\n")
    );
}

#[test]
#[file_serial(heavy_io)]
fn run_with_a_dead_streaming_reporter_still_succeeds_and_serves() {
    let mut serve = ServeProc::start();

    let test_file = write_temp(
        ".yaml",
        "steps:\n  - use: std/log@v1\n    with:\n      message: fanout-isolation\n",
    );
    let config_file = write_temp(
        ".yaml",
        &format!(
            "vus: 1\nduration: 1s\nreport:\n  reporters:\n    - use: std/report/prometheus-rw@v1\n      during_run: true\n      interval_ms: 200\n      with:\n        url: http://127.0.0.1:1/write\n        retry: {{ max_attempts: 1 }}\n        final_timeout: 2s\n    - use: std/report/serve@v1\n      during_run: true\n      interval_ms: 200\n      with: {{ url: {} }}\n",
            serve.url
        ),
    );

    // The dead Prometheus endpoint must not fail or hang the run, and the
    // live serve reporter keeps receiving metrics.
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
        .stdout(predicate::str::contains("fanout-isolation"));

    let serve_output = serve.wait_for_output("live snapshot");
    assert!(
        serve_output.join("\n").contains("live snapshot"),
        "a dead reporter never starves the live one, serve printed: {}",
        serve_output.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Streaming reporters (issue #12): Prometheus remote write + OTLP/gRPC in one
// run.
// ---------------------------------------------------------------------------

/// An in-process OTLP/gRPC collector (the hand-rolled service from core's
/// `report::otel` wire tests): every Export call's gRPC frame + protobuf is
/// decoded and stored. The runtime keeps the server alive for the test.
struct OtlpCollector {
    endpoint: String,
    requests: std::sync::Arc<
        std::sync::Mutex<Vec<perfscale_core::report::otel::OtlpExportMetricsServiceRequest>>,
    >,
    _rt: tokio::runtime::Runtime,
}

#[derive(Clone, Default)]
struct CollectorSvc {
    requests: std::sync::Arc<
        std::sync::Mutex<Vec<perfscale_core::report::otel::OtlpExportMetricsServiceRequest>>,
    >,
}

impl tonic::server::NamedService for CollectorSvc {
    const NAME: &'static str = "opentelemetry.proto.collector.metrics.v1.MetricsService";
}

impl tower::Service<http::Request<tonic::body::Body>> for CollectorSvc {
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<tonic::body::Body>) -> Self::Future {
        let requests = std::sync::Arc::clone(&self.requests);
        Box::pin(async move {
            use http_body_util::BodyExt as _;
            let payload = req
                .into_body()
                .collect()
                .await
                .expect("readable body")
                .to_bytes();
            let mut rest = &payload[..];
            while rest.len() >= 5 {
                let len = u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
                let msg = &rest[5..5 + len];
                let decoded =
                    perfscale_core::report::otel::OtlpExportMetricsServiceRequest::decode(msg)
                        .expect("OTLP protobuf decodes");
                requests.lock().unwrap().push(decoded);
                rest = &rest[5 + len..];
            }
            // Trailers-only success: grpc-status rides the headers.
            let resp = http::Response::builder()
                .status(200)
                .header("content-type", "application/grpc")
                .header("grpc-status", "0")
                .body(tonic::body::Body::default())
                .unwrap();
            Ok(resp)
        })
    }
}

impl OtlpCollector {
    fn start() -> Self {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let svc = CollectorSvc::default();
        let requests = std::sync::Arc::clone(&svc.requests);
        let endpoint = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(svc)
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
            );
            format!("http://{addr}")
        });
        Self {
            endpoint,
            requests,
            _rt: rt,
        }
    }

    fn drain(&self) -> Vec<perfscale_core::report::otel::OtlpExportMetricsServiceRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Wait until at least one Export arrived (bounded).
    fn wait_nonempty(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while self.requests.lock().unwrap().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "no OTLP export arrived"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[test]
#[file_serial(heavy_io)]
fn run_streams_to_prometheus_remote_write_and_otlp_simultaneously() {
    use perfscale_core::report::otel::{otlp_any_value, otlp_metric, OtlpAnyValue};

    let prom = PromSink::start();
    let otel = OtlpCollector::start();

    // One native run, both streaming reporters, labels at all three levels.
    let test_file = write_temp(
        ".yaml",
        "steps:\n  - use: std/http@v1\n    with:\n      method: GET\n      url: http://127.0.0.1:1/unreachable\n    labels: { channel: book }\n  - use: std/sleep@v1\n    with:\n      ms: 20\n",
    );
    let config_file = write_temp(
        ".yaml",
        &format!(
            "vus: 1\nduration: 1s\nreport:\n  labels: {{ testid: e2e-both }}\n  reporters:\n    - use: std/report/prometheus-rw@v1\n      during_run: true\n      interval_ms: 200\n      labels: {{ sink: prom }}\n      with: {{ url: {} }}\n    - use: std/report/otel@v1\n      during_run: true\n      interval_ms: 200\n      labels: {{ sink: otlp }}\n      with: {{ endpoint: {} }}\n",
            prom.url, otel.endpoint
        ),
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

    // --- Prometheus remote write: Snappy + protobuf decoded ---
    let prom_reqs = prom.wait_for(1);
    assert!(!prom_reqs.is_empty(), "no remote-write POSTs");
    let raw = snap::raw::Decoder::new()
        .decompress_vec(&prom_reqs.last().unwrap().1)
        .unwrap();
    let wr =
        perfscale_core::report::prometheus_rw::PromWriteRequest::decode(raw.as_slice()).unwrap();
    assert!(!wr.timeseries.is_empty());
    let labels_of = |ts: &perfscale_core::report::prometheus_rw::PromTimeSeries| {
        ts.labels
            .iter()
            .map(|l| (l.name.clone(), l.value.clone()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let bucket = wr
        .timeseries
        .iter()
        .find(|t| {
            let l = labels_of(t);
            l.get("__name__").map(String::as_str) == Some("http_req_duration_bucket")
                && l.get("le").map(String::as_str) == Some("+Inf")
        })
        .expect("classic +Inf bucket series");
    let bl = labels_of(bucket);
    // step > reporter > run + reserved engine labels.
    assert_eq!(bl.get("channel").map(String::as_str), Some("book"));
    assert_eq!(bl.get("sink").map(String::as_str), Some("prom"));
    assert_eq!(bl.get("testid").map(String::as_str), Some("e2e-both"));
    assert!(bl.get("run_id").is_some_and(|v| !v.is_empty()));
    assert_eq!(bl.get("worker").map(String::as_str), Some("0"));
    // Cumulative histogram data: +Inf bucket count == _count > 0.
    let count = wr
        .timeseries
        .iter()
        .find(|t| {
            labels_of(t).get("__name__").map(String::as_str) == Some("http_req_duration_count")
        })
        .expect("_count series");
    assert_eq!(
        bucket.samples[0].value, count.samples[0].value,
        "+Inf bucket equals _count"
    );
    assert!(count.samples[0].value > 0.0, "requests recorded");
    assert!(
        wr.timeseries
            .iter()
            .all(|t| !labels_of(t).contains_key("quantile")),
        "no client-side quantile series"
    );

    // --- OTLP/gRPC: protobuf decoded ---
    otel.wait_nonempty();
    let reqs = otel.drain();
    let last = reqs.last().unwrap();
    let metrics = &last.resource_metrics[0].scope_metrics[0].metrics;
    assert!(!metrics.is_empty());
    let find = |name: &str| metrics.iter().find(|m| m.name == name);
    let hist = match find("http_req_duration")
        .expect("duration metric")
        .data
        .as_ref()
        .unwrap()
    {
        otlp_metric::Data::ExponentialHistogram(h) => h,
        other => panic!("duration is an exponential histogram: {other:?}"),
    };
    assert_eq!(hist.aggregation_temporality, 2, "cumulative temporality");
    let point = &hist.data_points[0];
    assert!(point.count > 0, "samples recorded");
    assert_eq!(
        point.count,
        point
            .positive
            .as_ref()
            .map(|b| b.bucket_counts.iter().sum::<u64>())
            .unwrap_or(0),
        "bucket counts sum to count"
    );
    assert!(point.sum.unwrap_or(0.0) >= 0.0);
    assert!(point.start_time_unix_nano > 0 && point.start_time_unix_nano <= point.time_unix_nano);
    let attr = |k: &str| {
        point.attributes.iter().find_map(|a| {
            if a.key == k {
                match &a.value {
                    Some(OtlpAnyValue {
                        value: Some(otlp_any_value::Value::StringValue(v)),
                    }) => Some(v.clone()),
                    _ => None,
                }
            } else {
                None
            }
        })
    };
    assert_eq!(attr("channel").as_deref(), Some("book"));
    assert_eq!(attr("sink").as_deref(), Some("otlp"));
    assert_eq!(attr("testid").as_deref(), Some("e2e-both"));
    assert!(attr("run_id").is_some_and(|v| !v.is_empty()));
    assert_eq!(attr("worker").as_deref(), Some("0"));
    // No client-side quantile data anywhere: the duration metric exists only
    // as the exponential histogram above (no Summary/gauge duplicate).
    assert_eq!(
        metrics
            .iter()
            .filter(|m| m.name == "http_req_duration")
            .count(),
        1,
        "exactly one duration metric entry"
    );
}
