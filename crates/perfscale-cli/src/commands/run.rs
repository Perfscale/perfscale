use std::path::Path;
use std::time::Duration;

use perfscale_core::runner::locust::LocustOpts;
use perfscale_core::runner::{self, ExecutionPlan, LogLine, LogSource, RunOutput};
use perfscale_core::step::TestDef;
use perfscale_core::summary::{iso8601_utc, ExportMeta, SummaryExport};
use perfscale_core::yaml::{self, ConfigFile};

use crate::cli::{RunArgs, SummaryFormat};
use crate::error::CliError;

pub async fn run(args: RunArgs) -> Result<(), CliError> {
    let config = load_config(args.config.as_deref())?;
    let native_test = match &args.file {
        Some(path) => Some(load_test_def(path)?),
        None => None,
    };

    let plan = resolve_plan(&args, native_test, config.as_ref());
    let (engine, vus, duration) = plan_meta(&plan);
    let report_url = resolve_report_url(&args, config.as_ref());

    let RunOutput {
        mut lines, exit, ..
    } = runner::execute(plan).await.map_err(CliError::from_engine)?;
    let mut summary_lines: Vec<String> = Vec::new();

    while let Some(line) = lines.recv().await {
        if should_print(&line, args.quiet) {
            print_line(&line);
        }
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

    if !args.summary_export.is_empty() {
        let export = build_export(engine, vus, duration, &summary_lines);
        for path in &args.summary_export {
            write_summary_export(path, args.summary_format, &export)?;
        }
    }

    Ok(())
}

/// Engine name and load shape for the export metadata. k6 owns its load
/// shape inside the script, so vus/duration are unknown to the CLI there.
fn plan_meta(plan: &ExecutionPlan) -> (&'static str, Option<u32>, Option<String>) {
    match plan {
        ExecutionPlan::K6Script(_) => ("k6", None, None),
        ExecutionPlan::LocustScript { opts, .. } => {
            ("locust", Some(opts.users), Some(opts.duration.clone()))
        }
        ExecutionPlan::NativeSteps { config, .. } => {
            ("native", Some(config.vus), Some(config.duration.clone()))
        }
    }
}

fn build_export(
    engine: &str,
    vus: Option<u32>,
    duration: Option<String>,
    summary_lines: &[String],
) -> SummaryExport {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    SummaryExport {
        meta: ExportMeta {
            perfscale_version: env!("CARGO_PKG_VERSION").to_string(),
            engine: engine.to_string(),
            vus,
            duration,
            timestamp: iso8601_utc(secs),
        },
        summary: perfscale_core::summary::parse_summary(&summary_lines.join("\n")),
    }
}

/// Format precedence per file: a recognized `.md`/`.json` extension wins,
/// then the `--summary-format` flag, then JSON. Extension-first keeps mixed
/// multi-exports intuitive: `--summary-export a.json --summary-export
/// "$GITHUB_STEP_SUMMARY" --summary-format md` writes JSON to `a.json` and
/// Markdown to the extension-less CI summary file.
fn export_format(path: &Path, flag: Option<SummaryFormat>) -> SummaryFormat {
    match path.extension() {
        Some(e) if e.eq_ignore_ascii_case("md") => SummaryFormat::Md,
        Some(e) if e.eq_ignore_ascii_case("json") => SummaryFormat::Json,
        _ => flag.unwrap_or(SummaryFormat::Json),
    }
}

fn write_summary_export(
    path: &Path,
    flag: Option<SummaryFormat>,
    export: &SummaryExport,
) -> Result<(), CliError> {
    let body = match export_format(path, flag) {
        SummaryFormat::Json => export.to_json(),
        SummaryFormat::Md => export.to_markdown(),
    };
    std::fs::write(path, body).map_err(|e| {
        CliError::new(format!(
            "failed to write summary export '{}'",
            path.display()
        ))
        .cause(e.to_string())
        .hint("the run itself completed — only writing the export file failed")
        .docs("cli/commands.md#perfscale-run")
    })?;
    eprintln!("[system] summary exported to {}", path.display());
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
                "each step needs `use:` naming an action (std/http@v1, std/check@v1, std/sleep@v1, std/log@v1, std/file-read@v1, std/file-write@v1); \
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
        let cfg = config.expect("clap requires -c with -f");
        return ExecutionPlan::NativeSteps {
            test,
            config: cfg.run.clone(),
            before: cfg.before.clone(),
            variables: cfg.variables.clone(),
            quiet: args.quiet,
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

/// `--quiet` print policy, applied uniformly to every engine: keep errors,
/// system markers, and the metric summary; drop the per-request firehose.
/// The native engine additionally suppresses these lines at the source (see
/// `run_steps`); for k6/locust this filter is the only layer.
fn should_print(line: &LogLine, quiet: bool) -> bool {
    if !quiet {
        return true;
    }
    !matches!(line.source, LogSource::Stdout) || is_summary_line(&line.text)
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
            quiet: false,
            summary_export: Vec::new(),
            summary_format: None,
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
                ..Default::default()
            },
            report: report_url.map(|url| ReportConfig {
                url: url.to_string(),
            }),
            before: Vec::new(),
            variables: serde_json::Map::new(),
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
    fn resolve_plan_native_passes_quiet_flag() {
        let args = RunArgs {
            file: Some(PathBuf::from("t.yaml")),
            quiet: true,
            ..base_args()
        };
        let config = sample_config(None);
        let plan = resolve_plan(&args, Some(sample_test()), Some(&config));
        match plan {
            ExecutionPlan::NativeSteps { quiet, .. } => assert!(quiet),
            _ => panic!("expected NativeSteps plan"),
        }
    }

    #[test]
    fn should_print_quiet_keeps_summary_errors_and_system_lines() {
        let quiet = true;
        let line = |source, text: &str| LogLine {
            source,
            text: text.into(),
        };

        // Per-request stdout noise → dropped.
        assert!(!should_print(
            &line(LogSource::Stdout, "GET http://x/health → 200 OK (0.2ms)"),
            quiet
        ));
        // Metric summary → kept.
        assert!(should_print(
            &line(LogSource::Stdout, "http_reqs..............: 120 2.00/s"),
            quiet
        ));
        // Errors and system markers → kept.
        assert!(should_print(&line(LogSource::Stderr, "boom"), quiet));
        assert!(should_print(
            &line(LogSource::System, "Starting 10 VUs for 30s (30s)"),
            quiet
        ));
        // Non-quiet prints everything.
        assert!(should_print(&line(LogSource::Stdout, "anything"), false));
    }

    #[test]
    fn plan_meta_reports_engine_and_load_shape() {
        let native = ExecutionPlan::NativeSteps {
            test: sample_test(),
            config: RunConfig {
                vus: 7,
                duration: "45s".into(),
                ..Default::default()
            },
            before: Vec::new(),
            variables: serde_json::Map::new(),
            quiet: false,
        };
        assert_eq!(plan_meta(&native), ("native", Some(7), Some("45s".into())));

        let k6 = ExecutionPlan::K6Script(PathBuf::from("a.js"));
        assert_eq!(plan_meta(&k6), ("k6", None, None));

        let locust = ExecutionPlan::LocustScript {
            path: PathBuf::from("b.py"),
            opts: LocustOpts {
                users: 3,
                spawn_rate: 3,
                duration: "1m".into(),
                host: None,
            },
        };
        assert_eq!(plan_meta(&locust), ("locust", Some(3), Some("1m".into())));
    }

    #[test]
    fn export_format_extension_wins_then_flag_then_json() {
        use std::path::PathBuf;
        let md_path = PathBuf::from("out.md");
        let json_path = PathBuf::from("out.json");
        let bare_path = PathBuf::from("step_summary_a1b2c3");

        // Recognized extensions always win — mixed multi-exports stay sane.
        assert_eq!(export_format(&md_path, None), SummaryFormat::Md);
        assert_eq!(export_format(&json_path, None), SummaryFormat::Json);
        assert_eq!(
            export_format(&json_path, Some(SummaryFormat::Md)),
            SummaryFormat::Json
        );
        // No recognized extension → flag, then JSON default.
        assert_eq!(export_format(&bare_path, None), SummaryFormat::Json);
        assert_eq!(
            export_format(&bare_path, Some(SummaryFormat::Md)),
            SummaryFormat::Md
        );
    }

    #[test]
    fn build_export_parses_summary_and_stamps_meta() {
        let lines = vec![
            "http_req_duration......: avg=0.42ms p(50)=0.31ms p(90)=0.88ms p(95)=1.02ms p(99)=1.90ms min=0.09ms max=3.10ms".to_string(),
            "http_req_failed........: 0.00%".to_string(),
            "http_reqs..............: 120 2.00/s".to_string(),
        ];
        let export = build_export("native", Some(10), Some("30s".into()), &lines);
        assert_eq!(export.meta.engine, "native");
        assert_eq!(export.meta.vus, Some(10));
        assert_eq!(export.meta.perfscale_version, env!("CARGO_PKG_VERSION"));
        assert!(export.meta.timestamp.ends_with('Z'));
        let s = export.summary.expect("summary parsed");
        assert_eq!(s.total_requests, 120);
    }

    #[test]
    fn build_export_without_http_metrics_has_none_summary() {
        let lines = vec!["iterations..............: 10 1.00/s".to_string()];
        let export = build_export("native", Some(1), Some("1s".into()), &lines);
        assert!(export.summary.is_none());
    }

    #[test]
    fn write_summary_export_writes_json_and_md() {
        let dir = tempfile::tempdir().unwrap();
        let export = build_export("native", Some(2), Some("1s".into()), &[]);

        let json_path = dir.path().join("out.json");
        write_summary_export(&json_path, None, &export).unwrap();
        let json = std::fs::read_to_string(&json_path).unwrap();
        assert!(json.contains("\"engine\": \"native\""));

        let md_path = dir.path().join("out.md");
        write_summary_export(&md_path, None, &export).unwrap();
        let md = std::fs::read_to_string(&md_path).unwrap();
        assert!(md.starts_with("### perfscale run summary"));
    }

    #[test]
    fn write_summary_export_unwritable_path_is_cli_error() {
        let export = build_export("native", None, None, &[]);
        let err = write_summary_export(Path::new("/nonexistent-dir/out.json"), None, &export)
            .unwrap_err();
        assert!(err.to_string().contains("failed to write summary export"));
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
