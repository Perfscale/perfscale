use std::path::Path;
use std::time::Duration;

use perfscale_core::runner::locust::LocustOpts;
use perfscale_core::runner::{self, ExecutionPlan, LogLine, LogSource, RunOutput};
use perfscale_core::step::TestDef;
use perfscale_core::yaml::{self, ConfigFile};

use crate::cli::RunArgs;
use crate::error::CliError;

pub async fn run(args: RunArgs) -> Result<(), CliError> {
    let config = load_config(args.config.as_deref())?;
    let native_test = match &args.file {
        Some(path) => Some(load_test_def(path)?),
        None => None,
    };

    let plan = resolve_plan(&args, native_test, config.as_ref());
    let report_url = resolve_report_url(&args, config.as_ref());

    let RunOutput { mut lines, exit } =
        runner::execute(plan).await.map_err(CliError::from_engine)?;
    let mut summary_lines: Vec<String> = Vec::new();

    while let Some(line) = lines.recv().await {
        print_line(&line);
        if matches!(line.source, LogSource::Stdout) && is_summary_line(&line.text) {
            summary_lines.push(line.text.clone());
        }
    }

    // Crash detection: a non-zero exit code by itself is test feedback (k6
    // exits non-zero on failed thresholds, locust on failed requests). But a
    // non-zero exit with NO metrics produced means the engine died before it
    // ever ran the test — that's a CLI error, not a result.
    let exit_code = exit.await.ok().flatten();
    if let Some(code) = exit_code {
        if code != 0 && summary_lines.is_empty() {
            return Err(CliError::new(format!("engine exited with code {code} before producing any results"))
                .hint("the engine crashed at startup — its output above usually names the cause (script error, broken installation, bad flags)")
                .docs("cli/commands.md#exit-code-semantics"));
        }
    }

    if let Some(url) = report_url {
        report_summary(&url, &summary_lines).await;
    }

    Ok(())
}

/// Metric-summary lines (the block every engine emits at the end of a run).
/// Only these are forwarded to `--report` — per-iteration log output can run
/// to hundreds of thousands of lines and would blow past any collector's
/// request-size limit.
fn is_summary_line(text: &str) -> bool {
    const MARKERS: [&str; 8] = [
        "vus",
        "iterations",
        "iteration_duration",
        "http_req",
        "http_reqs",
        "data_received",
        "data_sent",
        "checks",
    ];
    let trimmed = text.trim_start();
    MARKERS.iter().any(|m| trimmed.starts_with(m))
}

fn load_config(path: Option<&Path>) -> Result<Option<ConfigFile>, CliError> {
    match path {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| {
                CliError::new(format!("failed to read config file '{}'", path.display()))
                    .cause(e.to_string())
                    .hint("`-c` expects a YAML load config, e.g. `vus: 10` + `duration: 30s`")
                    .docs("yaml-reference.md#config--c-configyaml")
            })?;
            let config = yaml::parse_config_file(&text).map_err(|e| {
                CliError::new(format!("invalid config file '{}'", path.display()))
                    .cause(e)
                    .hint("valid fields: `vus` (integer), `duration` (\"30s\"/\"5m\"/\"1h\"), optional `report.url`")
                    .docs("yaml-reference.md#config--c-configyaml")
            })?;
            Ok(Some(config))
        }
        None => Ok(None),
    }
}

fn load_test_def(path: &Path) -> Result<TestDef, CliError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CliError::new(format!("failed to read test file '{}'", path.display()))
            .cause(e.to_string())
            .hint("`-f` expects a YAML test definition with a `steps:` list")
            .docs("yaml-reference.md#test-definition--f-testyaml")
    })?;
    yaml::parse_test_file(&text).map_err(|e| {
        CliError::new(format!("invalid test file '{}'", path.display()))
            .cause(e)
            .hint(
                "each step needs `use:` naming an action (std/http@v1, std/check@v1, std/sleep@v1, std/log@v1); \
                 parameters go under `with:`",
            )
            .docs("yaml-reference.md#test-definition--f-testyaml")
    })
}

/// Resolve CLI flags + parsed config into an [`ExecutionPlan`]. Pure — no I/O.
///
/// `native_test` must be `Some` iff `args.file` is set; the caller loads and
/// parses it beforehand since that step needs the filesystem.
fn resolve_plan(
    args: &RunArgs,
    native_test: Option<TestDef>,
    config: Option<&ConfigFile>,
) -> ExecutionPlan {
    if let Some(script) = &args.k6 {
        return ExecutionPlan::K6Script(script.clone());
    }

    if let Some(script) = &args.locust {
        let opts = match config {
            Some(cfg) => LocustOpts::from_run_config(&cfg.run, args.host.clone()),
            None => LocustOpts {
                host: args.host.clone(),
                ..LocustOpts::default()
            },
        };
        return ExecutionPlan::LocustScript {
            path: script.clone(),
            opts,
        };
    }

    if args.file.is_some() {
        let test = native_test.expect("caller must load the test def when --file is set");
        // `-f` requires `-c` (enforced by clap), so config is always present here.
        let run_config = config.expect("clap requires -c with -f").run.clone();
        return ExecutionPlan::NativeSteps {
            test,
            config: run_config,
        };
    }

    unreachable!("clap ArgGroup guarantees exactly one of --k6/--locust/-f")
}

/// `--report` wins over a `report:` block in the config file.
fn resolve_report_url(args: &RunArgs, config: Option<&ConfigFile>) -> Option<String> {
    args.report
        .clone()
        .or_else(|| config.and_then(|c| c.report.as_ref().map(|r| r.url.clone())))
}

fn print_line(line: &LogLine) {
    match line.source {
        LogSource::Stdout => println!("{}", line.text),
        LogSource::Stderr => eprintln!("{}", line.text),
        LogSource::System => eprintln!("[system] {}", line.text),
    }
}

async fn report_summary(url: &str, lines: &[String]) {
    let endpoint = format!("{}/api/v1/metrics", url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let body = serde_json::json!({ "lines": lines });

    match client
        .post(&endpoint)
        .json(&body)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => eprintln!("[report] {endpoint} returned {}", resp.status()),
        Err(e) => eprintln!("[report] failed to reach {endpoint}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use perfscale_core::step::RunConfig;
    use perfscale_core::yaml::ReportConfig;

    use super::*;

    fn base_args() -> RunArgs {
        RunArgs {
            k6: None,
            locust: None,
            file: None,
            config: None,
            host: None,
            report: None,
        }
    }

    fn sample_test() -> TestDef {
        TestDef { steps: vec![] }
    }

    fn sample_config(report_url: Option<&str>) -> ConfigFile {
        ConfigFile {
            run: RunConfig {
                vus: 4,
                duration: "2m".into(),
            },
            report: report_url.map(|url| ReportConfig {
                url: url.to_string(),
            }),
        }
    }

    #[test]
    fn resolve_plan_picks_k6_when_k6_flag_set() {
        let args = RunArgs {
            k6: Some(PathBuf::from("a.js")),
            ..base_args()
        };
        let plan = resolve_plan(&args, None, None);
        assert!(matches!(plan, ExecutionPlan::K6Script(p) if p == Path::new("a.js")));
    }

    #[test]
    fn resolve_plan_locust_without_config_uses_defaults() {
        let args = RunArgs {
            locust: Some(PathBuf::from("b.py")),
            host: Some("https://example.com".into()),
            ..base_args()
        };
        let plan = resolve_plan(&args, None, None);
        match plan {
            ExecutionPlan::LocustScript { path, opts } => {
                assert_eq!(path, PathBuf::from("b.py"));
                assert_eq!(opts.users, 1);
                assert_eq!(opts.host.as_deref(), Some("https://example.com"));
            }
            _ => panic!("expected LocustScript plan"),
        }
    }

    #[test]
    fn resolve_plan_locust_with_config_maps_vus_to_users() {
        let args = RunArgs {
            locust: Some(PathBuf::from("b.py")),
            ..base_args()
        };
        let config = sample_config(None);
        let plan = resolve_plan(&args, None, Some(&config));
        match plan {
            ExecutionPlan::LocustScript { opts, .. } => {
                assert_eq!(opts.users, 4);
                assert_eq!(opts.duration, "2m");
            }
            _ => panic!("expected LocustScript plan"),
        }
    }

    #[test]
    fn resolve_plan_native_uses_loaded_test_and_config() {
        let args = RunArgs {
            file: Some(PathBuf::from("t.yaml")),
            ..base_args()
        };
        let config = sample_config(None);
        let plan = resolve_plan(&args, Some(sample_test()), Some(&config));
        match plan {
            ExecutionPlan::NativeSteps { config, .. } => assert_eq!(config.vus, 4),
            _ => panic!("expected NativeSteps plan"),
        }
    }

    #[test]
    #[should_panic(expected = "caller must load the test def")]
    fn resolve_plan_native_without_loaded_test_panics() {
        let args = RunArgs {
            file: Some(PathBuf::from("t.yaml")),
            ..base_args()
        };
        let config = sample_config(None);
        let _ = resolve_plan(&args, None, Some(&config));
    }

    #[test]
    fn resolve_report_url_prefers_cli_flag_over_config() {
        let args = RunArgs {
            report: Some("http://cli-wins".into()),
            ..base_args()
        };
        let config = sample_config(Some("http://config-loses"));
        assert_eq!(
            resolve_report_url(&args, Some(&config)).as_deref(),
            Some("http://cli-wins")
        );
    }

    #[test]
    fn resolve_report_url_falls_back_to_config() {
        let args = base_args();
        let config = sample_config(Some("http://from-config"));
        assert_eq!(
            resolve_report_url(&args, Some(&config)).as_deref(),
            Some("http://from-config")
        );
    }

    #[test]
    fn resolve_report_url_none_when_neither_set() {
        let args = base_args();
        assert_eq!(resolve_report_url(&args, None), None);
    }

    #[test]
    fn is_summary_line_accepts_metric_lines_from_all_engines() {
        // Native/locust-style (our own formatting).
        assert!(is_summary_line("vus....................: 3 min=1 max=3"));
        assert!(is_summary_line("iterations..............: 42 42.00/s"));
        assert!(is_summary_line("http_req_duration......: avg=1.00ms p(50)=1ms p(90)=1ms p(95)=1ms p(99)=1ms min=1ms max=1ms"));
        assert!(is_summary_line("http_req_failed........: 0.00%"));
        // k6-style (leading indentation, different dot padding).
        assert!(is_summary_line(
            "     http_reqs......................: 1      0.744818/s"
        ));
        assert!(is_summary_line(
            "     data_received..................: 4.9 kB 3.6 kB/s"
        ));
        assert!(is_summary_line(
            "     checks.........................: 100.00% ✓ 1        ✗ 0"
        ));
    }

    #[test]
    fn is_summary_line_rejects_per_iteration_log_output() {
        assert!(!is_summary_line("loop-test"));
        assert!(!is_summary_line("GET https://example.com → 200 OK (12ms)"));
        assert!(!is_summary_line("[check] homepage: status==200 → PASS"));
        assert!(!is_summary_line(""));
    }
}
