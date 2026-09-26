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
        before: config.before,
        after: config.after,
        variables: config.variables,
        shared_variables: config.shared_variables,
        libraries: Vec::new(),
        config: Box::new(config.run),
        quiet: false,
        metrics_tx: None,
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
        ..Default::default()
    };

    let rx = runner::execute(ExecutionPlan::NativeSteps {
        test,
        before: Vec::new(),
        after: Vec::new(),
        variables: serde_json::Map::new(),
        shared_variables: serde_json::Map::new(),
        libraries: Vec::new(),
        config: Box::new(config),
        quiet: false,
        metrics_tx: None,
    })
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
        ..Default::default()
    };

    let rx = runner::execute(ExecutionPlan::NativeSteps {
        test,
        before: Vec::new(),
        after: Vec::new(),
        variables: serde_json::Map::new(),
        shared_variables: serde_json::Map::new(),
        libraries: Vec::new(),
        config: Box::new(config),
        quiet: false,
        metrics_tx: None,
    })
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
// ${{ env.NAME }} secrets: masked in the run log, real on the wire
// ---------------------------------------------------------------------------

/// A resolved `${{ env.NAME }}` value must never appear in the run log
/// (masked as `***` by the log pipeline), while steps still receive the real
/// value — here: an HTTP header the backend matches on.
#[tokio::test]
#[file_serial(heavy_io)]
async fn env_secret_is_masked_in_run_logs_but_reaches_the_backend_verbatim() {
    const VAR: &str = "PERFSCALE_TEST_E2E_MASKED_ENV_SECRET";
    const SECRET: &str = "e2e-s3cr3t-token-7f9c";
    std::env::set_var(VAR, SECRET);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/whoami"))
        // The mock only matches the REAL header value — masking must not
        // rewrite what goes on the wire.
        .and(header("x-api-key", SECRET))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .expect(1..)
        .mount(&server)
        .await;

    let test_yaml = format!(
        r#"
steps:
  - name: authed call
    use: std/http@v1
    with:
      method: GET
      url: {}/whoami
      headers:
        x-api-key: ${{{{ env.{VAR} }}}}
    check:
      status: 200
  - name: log the token
    use: std/log@v1
    with:
      message: "token=${{{{ env.{VAR} }}}} end"
  - use: std/sleep@v1
    with:
      ms: 50
"#,
        server.uri()
    );

    let test = yaml::parse_test_file(&test_yaml).expect("test yaml parses");
    let config = RunConfig {
        vus: 1,
        duration: "1s".into(),
        ..Default::default()
    };

    let rx = runner::execute(ExecutionPlan::NativeSteps {
        test,
        before: Vec::new(),
        after: Vec::new(),
        variables: serde_json::Map::new(),
        shared_variables: serde_json::Map::new(),
        libraries: Vec::new(),
        config: Box::new(config),
        quiet: false,
        metrics_tx: None,
    })
    .await
    .unwrap();
    let lines = collect(rx).await;
    let all: String = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // The resolved secret appears in no run-log line of any source…
    assert!(
        !all.contains(SECRET),
        "secret leaked into the run log:\n{all}"
    );
    // …the std/log step's line shows the mask instead…
    assert!(all.contains("token=*** end"), "run log was:\n{all}");
    // …and ordinary output is untouched by masking.
    assert!(all.contains("status==200 → PASS"), "run log was:\n{all}");

    // The backend saw the real header value (the mock expects ≥1 match).
    server.verify().await;
    std::env::remove_var(VAR);
}

// ---------------------------------------------------------------------------
// Shipped examples must stay valid (they are the first thing users copy)
// ---------------------------------------------------------------------------

#[test]
fn shipped_example_test_yaml_parses() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    let mut count = 0;
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if !name.ends_with(".test.yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let test = yaml::parse_test_file(&text)
            .unwrap_or_else(|e| panic!("examples/{name} must parse: {e}"));
        assert!(!test.steps.is_empty(), "examples/{name} has no steps");
        count += 1;
    }
    assert!(count > 0, "no *.test.yaml examples found under {root}");
}

#[test]
fn shipped_example_config_yaml_parses() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    let text = std::fs::read_to_string(format!("{root}/hello.config.yaml")).unwrap();
    let config = yaml::parse_config_file(&text).expect("examples/hello.config.yaml must parse");
    assert_eq!(config.run.vus, 5);
    assert_eq!(config.run.duration, "30s");

    // Every shipped config parses AND its load profile resolves — the
    // examples are the first thing users copy.
    let mut count = 0;
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if !name.ends_with(".config.yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let config = yaml::parse_config_file(&text)
            .unwrap_or_else(|e| panic!("examples/{name} must parse: {e}"));
        config
            .run
            .resolve_schedule()
            .unwrap_or_else(|e| panic!("examples/{name} load profile must resolve: {e}"));
        count += 1;
    }
    assert!(
        count >= 4,
        "expected hello + load-profile configs, got {count}"
    );
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
// GPU bench suite must stay valid and its SLO gates typo-free
// ---------------------------------------------------------------------------

/// The bench/gpu test definitions (`-f`) parse and carry steps.
#[test]
fn bench_gpu_test_yamls_parse() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/gpu");
    for name in ["ollama.yaml", "vllm.yaml"] {
        let text = std::fs::read_to_string(format!("{root}/{name}"))
            .unwrap_or_else(|e| panic!("bench/gpu/{name} must exist: {e}"));
        let test = yaml::parse_test_file(&text)
            .unwrap_or_else(|e| panic!("bench/gpu/{name} must parse: {e}"));
        assert!(!test.steps.is_empty(), "bench/gpu/{name} has no steps");
    }
}

/// The bench/gpu load profiles (`-c`) parse, their schedules resolve, and
/// every `std/thresholds@v1` gate references real LLM-bench metrics with
/// parseable expressions — a typo here would fail every bench run as a
/// config error.
#[test]
fn bench_gpu_profile_yamls_parse_and_have_valid_gates() {
    use perfscale_core::step::thresholds;

    const KNOWN_GATE_METRICS: &[&str] = &[
        "llm_ttft_ms",
        "llm_tokens_per_sec",
        "llm_ttft_ms_failed",
        "llm_tokens_per_sec_failed",
        "dropped_iterations",
    ];

    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/gpu");
    for name in ["stages.yaml", "arrival.yaml"] {
        let text = std::fs::read_to_string(format!("{root}/{name}"))
            .unwrap_or_else(|e| panic!("bench/gpu/{name} must exist: {e}"));
        let config = yaml::parse_config_file(&text)
            .unwrap_or_else(|e| panic!("bench/gpu/{name} must parse: {e}"));
        config
            .run
            .resolve_schedule()
            .unwrap_or_else(|e| panic!("bench/gpu/{name} load profile must resolve: {e}"));

        let gates: Vec<_> = config
            .after
            .iter()
            .filter(|s| s.action == "std/thresholds@v1")
            .collect();
        assert!(
            !gates.is_empty(),
            "bench/gpu/{name} must carry at least one SLO gate"
        );
        for gate in gates {
            let with = gate
                .with
                .as_ref()
                .and_then(|w| w.as_object())
                .unwrap_or_else(|| panic!("bench/gpu/{name}: gate `with` must be an object"));
            for (metric, exprs) in with {
                assert!(
                    KNOWN_GATE_METRICS.contains(&metric.as_str()),
                    "bench/gpu/{name}: gate references unknown metric '{metric}' \
                     (known: {KNOWN_GATE_METRICS:?})"
                );
                let list: Vec<&str> = match exprs {
                    serde_json::Value::String(s) => vec![s.as_str()],
                    serde_json::Value::Array(items) => items
                        .iter()
                        .map(|i| {
                            i.as_str()
                                .unwrap_or_else(|| panic!("bench/gpu/{name}: non-string expr"))
                        })
                        .collect(),
                    other => panic!("bench/gpu/{name}: bad exprs value {other}"),
                };
                for raw in list {
                    thresholds::parse_expr(raw).unwrap_or_else(|e| {
                        panic!("bench/gpu/{name}: unparseable expression \"{raw}\": {e}")
                    });
                }
            }
        }
    }
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

// ---------------------------------------------------------------------------
// WASM value-generator libraries (RFC 005 phase 2)
// ---------------------------------------------------------------------------

/// Build the SDK `hello` example component once; `None` (skip) when the
/// wasm32-wasip2 target is not installed.
#[cfg(feature = "wasm-libs")]
fn hello_component() -> Option<std::path::PathBuf> {
    use std::sync::OnceLock;
    static HELLO: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    HELLO
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
                eprintln!(
                    "skipping WASM library e2e test: wasm32-wasip2 target not installed \
                     (rustup target add wasm32-wasip2)"
                );
                return None;
            }
            let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .unwrap();
            let target = ws.join("target/wasm-libs-fixtures");
            let status = std::process::Command::new(
                std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()),
            )
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
        .clone()
}

/// A YAML config declaring a local `.wasm` library expands `${hello.*}`
/// tokens through the component end to end (RFC 005 phase 2).
#[cfg(feature = "wasm-libs")]
#[tokio::test]
#[file_serial(heavy_io)]
async fn yaml_run_expands_wasm_library_tokens() {
    let Some(component) = hello_component() else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/greet"))
        // std/http expands `${...}` in the body: this expectation only
        // matches when the WASM component produced the greeting.
        .and(body_string_contains("hello, world!"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1..)
        .mount(&server)
        .await;

    let test_yaml = format!(
        r#"
steps:
  - name: greet
    use: std/http@v1
    with:
      method: POST
      url: {0}/greet
      body: |
        {{ "msg": "${{hello.greet(world)}}" }}
"#,
        server.uri()
    );
    let config_yaml = "vus: 1\nduration: 1s\n";

    let test = yaml::parse_test_file(&test_yaml).expect("test yaml parses");
    let config = yaml::parse_config_file(config_yaml).expect("config yaml parses");

    let libraries = vec![perfscale_core::library::LibraryRef {
        use_: component.to_string_lossy().into_owned(),
        sha256: None,
        r#as: Some("hello".into()),
        capabilities: None,
        with: None,
        secret: None,
        allow: None,
        deny: None,
        log: None,
    }];

    let rx = runner::execute(ExecutionPlan::NativeSteps {
        test,
        before: config.before,
        after: config.after,
        variables: config.variables,
        shared_variables: config.shared_variables,
        libraries,
        config: Box::new(config.run),
        quiet: false,
        metrics_tx: None,
    })
    .await
    .unwrap();
    let _lines = collect(rx).await;
    // The mock only matched if the body contained the component's output.
    server.verify().await;
}

/// RFC 005 phase 3.5: the frozen run settings JSON reaches the WASM library
/// through the 0.2 call context — vus/seed/variables visible to the guest.
#[cfg(feature = "wasm-libs")]
#[tokio::test]
#[file_serial(heavy_io)]
async fn yaml_run_passes_frozen_settings_to_wasm_library() {
    let Some(component) = hello_component() else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/settings"))
        .and(body_string_contains(r#""vus":1"#))
        .and(body_string_contains(r#""seed":7"#))
        .and(body_string_contains(r#""region":"eu-west""#))
        .respond_with(ResponseTemplate::new(200))
        .expect(1..)
        .mount(&server)
        .await;

    let test_yaml = format!(
        r#"
steps:
  - name: settings
    use: std/http@v1
    with:
      method: POST
      url: {0}/settings
      body: ${{hello.settings()}}
"#,
        server.uri()
    );
    let config_yaml = "vus: 1\nduration: 1s\nseed: 7\nvariables:\n  region: eu-west\n";

    let test = yaml::parse_test_file(&test_yaml).expect("test yaml parses");
    let config = yaml::parse_config_file(config_yaml).expect("config yaml parses");

    let libraries = vec![perfscale_core::library::LibraryRef {
        use_: component.to_string_lossy().into_owned(),
        sha256: None,
        r#as: Some("hello".into()),
        capabilities: None,
        with: None,
        secret: None,
        allow: None,
        deny: None,
        log: None,
    }];

    let rx = runner::execute(ExecutionPlan::NativeSteps {
        test,
        before: config.before,
        after: config.after,
        variables: config.variables,
        shared_variables: config.shared_variables,
        libraries,
        config: Box::new(config.run),
        quiet: false,
        metrics_tx: None,
    })
    .await
    .unwrap();
    let _lines = collect(rx).await;
    server.verify().await;
}

/// RFC 005 phase 3.5: `secret: true` on a library entry masks every result
/// in the run log, while the wire still carries the real value.
///
/// Log surface: `std/log` never expands `${...}`, so the observable surface
/// is the http per-request line — `GET <url> → 200 …` — which logs the
/// *expanded* (pre-reqwest-encoding) URL. The library value goes into the
/// URL path: run A (`secret: true`) must show `***` there, run B (no rule)
/// must show the real value (proving the surface is real and the assertion
/// non-vacuous), and in both runs the backend receives the real value
/// (reqwest percent-encodes the space on the wire, the mock matches the
/// encoded path).
#[cfg(feature = "wasm-libs")]
#[tokio::test]
#[file_serial(heavy_io)]
async fn library_entry_secret_masks_results_in_the_run_log() {
    let Some(component) = hello_component() else {
        return;
    };

    /// One run of the value-in-URL scenario. Returns (run log, paths the
    /// backend actually saw on the wire).
    async fn run_once(component: &std::path::Path, secret: Option<bool>) -> (String, Vec<String>) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            // The encoded form of the library's "hello, world!" — the mock
            // only matches when the real value reached the wire.
            .and(path("/t/hello,%20world!"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1..)
            .mount(&server)
            .await;

        let test_yaml = format!(
            r#"
steps:
  - name: greet call
    use: std/http@v1
    with:
      method: GET
      url: "{0}/t/${{hello.greet(world)}}"
"#,
            server.uri()
        );
        let test = yaml::parse_test_file(&test_yaml).expect("test yaml parses");
        let config = RunConfig {
            vus: 1,
            duration: "1s".into(),
            ..Default::default()
        };
        let libraries = vec![perfscale_core::library::LibraryRef {
            use_: component.to_string_lossy().into_owned(),
            sha256: None,
            r#as: Some("hello".into()),
            capabilities: None,
            with: None,
            secret,
            allow: None,
            deny: None,
            log: None,
        }];

        let rx = runner::execute(ExecutionPlan::NativeSteps {
            test,
            before: Vec::new(),
            after: Vec::new(),
            variables: serde_json::Map::new(),
            shared_variables: serde_json::Map::new(),
            libraries,
            config: Box::new(config),
            quiet: false,
            metrics_tx: None,
        })
        .await
        .unwrap();
        let lines = collect(rx).await;
        server.verify().await; // real value on the wire, every iteration
        let wire_paths = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect();
        let log: String = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        (log, wire_paths)
    }

    let (log_secret, wire_secret) = run_once(&component, Some(true)).await;
    let (log_plain, _) = run_once(&component, None).await;

    assert!(!wire_secret.is_empty(), "the run must have called the backend");

    // Without the rule the value reaches the request line in the log…
    assert!(
        log_plain.contains("hello, world!"),
        "the log surface must show the expanded URL without masking:\n{log_plain}"
    );
    // …with `secret: true` every occurrence is masked…
    assert!(
        !log_secret.contains("hello, world!"),
        "secret library result leaked into the run log:\n{log_secret}"
    );
    assert!(
        log_secret.contains("/t/***"),
        "the request line must show the mask instead:\n{log_secret}"
    );
}

/// RFC 005 phase 3.5: `deny:` blocks the call at expansion time — the step
/// fails with a message naming the rule, before any network call. (`std/log`
/// never expands `${...}`, so the denied token must live in an expanding
/// action — here the http body.)
#[cfg(feature = "wasm-libs")]
#[tokio::test]
#[file_serial(heavy_io)]
async fn library_deny_rule_fails_the_step() {
    let Some(component) = hello_component() else {
        return;
    };
    // The blocked call must never reach the wire.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/greet"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let test_yaml = format!(
        r#"
steps:
  - name: greet
    use: std/http@v1
    with:
      method: POST
      url: {0}/greet
      body: "${{hello.greet(world)}}"
"#,
        server.uri()
    );
    let test = yaml::parse_test_file(&test_yaml).expect("test yaml parses");
    let config = RunConfig {
        vus: 1,
        duration: "1s".into(),
        ..Default::default()
    };
    let libraries = vec![perfscale_core::library::LibraryRef {
        use_: component.to_string_lossy().into_owned(),
        sha256: None,
        r#as: Some("hello".into()),
        capabilities: None,
        with: None,
        secret: None,
        allow: None,
        deny: Some(vec!["greet".into()]),
        log: None,
    }];

    let rx = runner::execute(ExecutionPlan::NativeSteps {
        test,
        before: Vec::new(),
        after: Vec::new(),
        variables: serde_json::Map::new(),
        shared_variables: serde_json::Map::new(),
        libraries,
        config: Box::new(config),
        quiet: false,
        metrics_tx: None,
    })
    .await
    .unwrap();
    let lines = collect(rx).await;
    let all: String = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all.contains("deny"),
        "deny-blocked call must surface in the run log:\n{all}"
    );
    assert!(
        !all.contains("hello, world!"),
        "the denied call must not have run:\n{all}"
    );
    server.verify().await;
}

/// RFC 005 burn (phase 1): a run resolving the library through the burn
/// cache (`.cwasm` artifact) produces exactly the values of a run compiling
/// from source — same seed, same token on the wire.
#[cfg(feature = "wasm-libs")]
#[tokio::test]
#[file_serial(heavy_io)]
async fn run_with_burn_cache_matches_run_without() {
    let Some(component) = hello_component() else {
        return;
    };

    /// Run once against a fresh mock; returns the request bodies seen.
    async fn run_token(component: &std::path::Path) -> Vec<String> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1..)
            .mount(&server)
            .await;
        let test_yaml = format!(
            r#"
steps:
  - name: token
    use: std/http@v1
    with:
      method: POST
      url: {0}/token
      body: "${{hello.token(order)}}"
"#,
            server.uri()
        );
        let test = yaml::parse_test_file(&test_yaml).expect("test yaml parses");
        let config = RunConfig {
            vus: 1,
            duration: "1s".into(),
            seed: Some(7),
            ..Default::default()
        };
        let libraries = vec![perfscale_core::library::LibraryRef {
            use_: component.to_string_lossy().into_owned(),
            sha256: None,
            r#as: Some("hello".into()),
            capabilities: None,
            with: None,
            secret: None,
            allow: None,
            deny: None,
            log: None,
        }];
        let rx = runner::execute(ExecutionPlan::NativeSteps {
            test,
            before: Vec::new(),
            after: Vec::new(),
            variables: serde_json::Map::new(),
            shared_variables: serde_json::Map::new(),
            libraries,
            config: Box::new(config),
            quiet: false,
            metrics_tx: None,
        })
        .await
        .unwrap();
        let _lines = collect(rx).await;
        server.verify().await; // the token call must have happened
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .collect()
    }

    // The burn-cache probe reads PERFSCALE_CACHE_DIR at load time.
    let prev = std::env::var_os("PERFSCALE_CACHE_DIR");
    let restore = |prev: &Option<std::ffi::OsString>| match prev {
        Some(v) => std::env::set_var("PERFSCALE_CACHE_DIR", v),
        None => std::env::remove_var("PERFSCALE_CACHE_DIR"),
    };

    // Without burn cache: an empty cache dir — full compile.
    let empty = tempfile::tempdir().unwrap();
    std::env::set_var("PERFSCALE_CACHE_DIR", empty.path());
    let fresh = run_token(&component).await;

    // With burn cache: precompiled artifact in place.
    let bytes = std::fs::read(&component).unwrap();
    let sha = perfscale_core::library::burn::sha256_hex(&bytes);
    let cache = tempfile::tempdir().unwrap();
    let root = cache.path().join("libraries");
    perfscale_core::import::write_library_burn(
        &root,
        &sha,
        &perfscale_core::library::burn::burn_component(&bytes).unwrap(),
    )
    .unwrap();
    std::env::set_var("PERFSCALE_CACHE_DIR", cache.path());
    let burned = run_token(&component).await;
    restore(&prev);

    assert!(!fresh.is_empty(), "the run must have POSTed");
    assert!(!burned.is_empty(), "the burned-cache run must have POSTed");
    // Iteration counts differ (compile time eats into the 1s window) — the
    // token sequence must be identical where both ran.
    let (short, long) = if fresh.len() <= burned.len() {
        (&fresh, &burned)
    } else {
        (&burned, &fresh)
    };
    assert_eq!(
        &long[..short.len()],
        short.as_slice(),
        "burn cache must not change the token sequence"
    );
}
