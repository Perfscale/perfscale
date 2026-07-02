//! End-usage integration tests: exercise perfscale-core the way an external
//! consumer (the CLI, or a third-party embedding the engine) would — through
//! the public API only: parse YAML → build an ExecutionPlan → execute →
//! consume the LogLine stream.

use perfscale_core::runner::{self, ExecutionPlan, LogLine, LogSource, RunOutput};
use perfscale_core::step::RunConfig;
use perfscale_core::yaml;
use serial_test::file_serial;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn collect(output: RunOutput) -> Vec<LogLine> {
    let RunOutput {
        mut lines,
        exit: _,
        pid: _,
    } = output;
    let mut collected = Vec::new();
    while let Some(line) = lines.recv().await {
        collected.push(line);
    }
    collected
}

fn stdout_text(lines: &[LogLine]) -> String {
    lines
        .iter()
        .filter(|l| matches!(l.source, LogSource::Stdout))
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// YAML file → native engine → summary (the `-f/-c` user journey)
// ---------------------------------------------------------------------------

#[tokio::test]
#[file_serial(heavy_io)]
async fn yaml_test_file_runs_against_http_backend_and_reports_metrics() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"up"}"#))
        .mount(&server)
        .await;

    let test_yaml = format!(
        r#"
steps:
  - name: health check
    use: std/http@v1
    with:
      method: GET
      url: {}/health
    check:
      status: 200
      body_contains: up
    outputs: resp
  - name: echo status
    use: std/log@v1
    with:
      message: "health returned ${{{{ resp.status }}}}"
  - use: std/sleep@v1
    with:
      ms: 50
"#,
        server.uri()
    );
    let config_yaml = "vus: 2\nduration: 1s\n";

    let test = yaml::parse_test_file(&test_yaml).expect("test yaml parses");
    let config = yaml::parse_config_file(config_yaml).expect("config yaml parses");

    let rx = runner::execute(ExecutionPlan::NativeSteps {
        test,
        config: config.run,
    })
    .await
    .unwrap();
    let lines = collect(rx).await;
    let out = stdout_text(&lines);

    // Interpolation worked end to end.
    assert!(out.contains("health returned 200"), "stdout was:\n{out}");
    // Checks passed.
    assert!(out.contains("status==200 → PASS"), "stdout was:\n{out}");
    assert!(
        out.contains(r#"body contains "up" → PASS"#),
        "stdout was:\n{out}"
    );
    // k6-compatible summary block present. The exact error rate is not
    // asserted — under full-suite load a single loopback request can
    // spuriously fail; per-request success is covered by the action tests.
    assert!(out.contains("http_req_failed"), "stdout was:\n{out}");
    assert!(out.contains("http_req_duration"), "stdout was:\n{out}");
    assert!(
        out.contains("vus....................: 2 min=1 max=2"),
        "stdout was:\n{out}"
    );
}

#[tokio::test]
#[file_serial(heavy_io)]
async fn yaml_post_step_sends_body_and_headers_to_backend() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login"))
        .and(header("x-tenant", "acme"))
        .and(body_string_contains("secret"))
        .respond_with(ResponseTemplate::new(201).set_body_string(r#"{"token":"abc123"}"#))
        .expect(1..)
        .mount(&server)
        .await;

    let test_yaml = format!(
        r#"
steps:
  - name: login
    use: std/http@v1
    with:
      method: POST
      url: {}/login
      headers:
        x-tenant: acme
      body:
        password: secret
    check:
      status: 201
    outputs: login
  - name: use token
    use: std/log@v1
    with:
      message: "token body: ${{{{ login.body }}}}"
  - use: std/sleep@v1
    with:
      ms: 50
"#,
        server.uri()
    );

    let test = yaml::parse_test_file(&test_yaml).unwrap();
    let config = RunConfig {
        vus: 1,
        duration: "1s".into(),
    };

    let rx = runner::execute(ExecutionPlan::NativeSteps { test, config })
        .await
        .unwrap();
    let lines = collect(rx).await;
    let out = stdout_text(&lines);

    assert!(out.contains("status==201 → PASS"), "stdout was:\n{out}");
    assert!(
        out.contains(r#"token body: {"token":"abc123"}"#),
        "stdout was:\n{out}"
    );
    server.verify().await;
}

#[tokio::test]
#[file_serial(heavy_io)]
async fn failing_backend_shows_up_in_error_rate_and_check_failures() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/broken"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let test_yaml = format!(
        r#"
steps:
  - name: broken endpoint
    use: std/http@v1
    with:
      url: {}/broken
    check:
      status: 200
  - use: std/sleep@v1
    with:
      ms: 50
"#,
        server.uri()
    );

    let test = yaml::parse_test_file(&test_yaml).unwrap();
    let config = RunConfig {
        vus: 1,
        duration: "1s".into(),
    };

    let rx = runner::execute(ExecutionPlan::NativeSteps { test, config })
        .await
        .unwrap();
    let lines = collect(rx).await;

    // 503s are recorded as failures in the summary...
    let out = stdout_text(&lines);
    assert!(
        out.contains("http_req_failed........: 100.00%"),
        "stdout was:\n{out}"
    );
    // ...and both the request line and the failed check go to stderr.
    let err_text: String = lines
        .iter()
        .filter(|l| matches!(l.source, LogSource::Stderr))
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(err_text.contains("503"), "stderr was:\n{err_text}");
    assert!(
        err_text.contains("status==200 → FAIL"),
        "stderr was:\n{err_text}"
    );
}

// ---------------------------------------------------------------------------
// Shipped examples must stay valid (they are the first thing users copy)
// ---------------------------------------------------------------------------

#[test]
fn shipped_example_test_yaml_parses() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    let text = std::fs::read_to_string(format!("{root}/hello.test.yaml")).unwrap();
    let test = yaml::parse_test_file(&text).expect("examples/hello.test.yaml must parse");
    assert!(!test.steps.is_empty());
}

#[test]
fn shipped_example_config_yaml_parses() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    let text = std::fs::read_to_string(format!("{root}/hello.config.yaml")).unwrap();
    let config = yaml::parse_config_file(&text).expect("examples/hello.config.yaml must parse");
    assert_eq!(config.run.vus, 5);
    assert_eq!(config.run.duration, "30s");
}

#[test]
fn shipped_schemas_match_generated_ones() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../schema");
    let on_disk_test: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{root}/test.schema.json")).unwrap())
            .unwrap();
    let on_disk_config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{root}/config.schema.json")).unwrap(),
    )
    .unwrap();

    assert_eq!(
        on_disk_test,
        perfscale_core::schema::test_schema(),
        "schema/test.schema.json is stale — run `cargo run -p perfscale-core --example gen_schema`"
    );
    assert_eq!(
        on_disk_config,
        perfscale_core::schema::config_schema(),
        "schema/config.schema.json is stale — run `cargo run -p perfscale-core --example gen_schema`"
    );
}

// ---------------------------------------------------------------------------
// k6 script journey (gated on a real k6 install)
// ---------------------------------------------------------------------------

#[tokio::test]
#[file_serial(heavy_io)]
async fn k6_script_against_backend_reports_success() {
    if std::process::Command::new("k6")
        .arg("version")
        .output()
        .is_err()
    {
        eprintln!("skipping: k6 not installed");
        return;
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/k6"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1..)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("script.js");
    std::fs::write(
        &script_path,
        format!(
            "import http from 'k6/http';\nexport const options = {{ vus: 1, iterations: 2 }};\nexport default function() {{ http.get('{}/k6'); }}",
            server.uri()
        ),
    )
    .unwrap();

    let rx = runner::execute(ExecutionPlan::K6Script(script_path))
        .await
        .unwrap();
    let lines = collect(rx).await;
    let all: String = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(all.contains("http_reqs"), "k6 output was:\n{all}");
    assert!(all.contains("2 complete"), "k6 output was:\n{all}");
    server.verify().await;
}
