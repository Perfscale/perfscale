use std::path::{Path, PathBuf};
use std::time::Duration;

use perfscale_core::import::{self, ImportOptions};
use perfscale_core::runner::jmeter::JMeterOpts;
use perfscale_core::runner::locust::LocustOpts;
use perfscale_core::runner::{self, ExecutionPlan, LogLine, LogSource, RunOutput};
use perfscale_core::step::TestDef;
use perfscale_core::summary::{iso8601_utc, ExportMeta, SummaryExport};
use perfscale_core::yaml::ConfigFile;

use crate::cli::{RunArgs, SummaryFormat};
use crate::error::CliError;

pub async fn run(args: RunArgs) -> Result<(), CliError> {
    let import_opts = ImportOptions {
        allow_remote: args.allow_remote_import,
        refresh: args.refresh_imports,
        ..Default::default()
    };
    let config = load_config(args.config.as_deref(), &import_opts).await?;
    let native_test = match &args.file {
        Some(path) => Some(load_test_def(path, &import_opts).await?),
        None => None,
    };

    // Reporter specs fail fast, before the run starts (issue #9): unknown
    // reporter, missing destination, unresolved `${VAR}` header refs.
    let (flag_url, flag_reporters) = parse_report_flags(&args.report)?;
    let labels = parse_labels(&args.label)?;

    // Typed inputs (issue #11): -i overrides and -e env files resolve
    // against the config's `inputs:` declarations — fail fast on unknown
    // keys, type mismatches, out-of-bounds values, missing required inputs.
    let input_overrides = parse_input_overrides(&args.input)?;
    let env_inputs = load_env_files(&args.env_file)?;

    let mut plan = resolve_plan(
        &args,
        native_test,
        config.as_ref(),
        &flag_reporters,
        &labels,
        &input_overrides,
        &env_inputs,
    )?;
    let (engine, vus, duration) = plan_meta(&plan);
    let report_url = resolve_report_url(flag_url.as_ref(), config.as_ref());
    let streaming = wire_streaming(&mut plan, report_url.as_ref())?;
    // The reporters the run will deliver at the end (config + CLI flags).
    let reporters = plan_reporters(&plan, &flag_reporters);
    if !reporters.is_empty() {
        perfscale_core::report::validate_reporters(&reporters).map_err(|msg| {
            CliError::new(format!("invalid report configuration: {msg}"))
                .hint("reporters are declared in the config's `report.reporters` block or via --report json:PATH / junit:PATH / html:PATH")
                .docs("yaml-reference.md#config--c-configyaml")
        })?;
    }

    let RunOutput {
        mut lines, exit, ..
    } = runner::execute(plan).await.map_err(CliError::from_engine)?;
    let mut summary_lines: Vec<String> = Vec::new();
    // The native engine's machine-readable `report: {…}` line (report@v1) is
    // captured here, never forwarded into the summary lines or the serve
    // summary POST.
    let mut report_line: Option<String> = None;

    while let Some(line) = lines.recv().await {
        if matches!(line.source, LogSource::Stdout) && line.text.starts_with("report: {") {
            report_line = Some(line.text.clone());
            continue;
        }
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

    // Streaming reporters (issue #12): the engine dropped its snapshot
    // sender when the VU phase ended, closing the channel — each reporter's
    // delivery worker is now running its bounded final flush. Wait for all
    // of them (bounded) so the terminal cumulative values land before exit;
    // a reporter failure here is a warning, never a run failure.
    if !streaming.handles.is_empty() {
        let handles = streaming.handles;
        let wait = async move {
            for h in handles {
                let _ = h.await;
            }
        };
        if tokio::time::timeout(Duration::from_secs(45), wait)
            .await
            .is_err()
        {
            eprintln!("[report] warning: a streaming reporter's final flush exceeded its bound — the run's results are unaffected");
        }
        if let Some(counters) = &streaming.counters {
            for (metric, reporter, value) in counters.snapshot() {
                if value > 0 {
                    eprintln!("[report] {metric}{{reporter=\"{reporter}\"}}: {value}");
                }
            }
        }
    }

    if let Some(url) = report_url.as_ref() {
        report_summary(url, &summary_lines).await;
    }

    // End-of-run reporters (issue #9): render the canonical report@v1
    // document once, deliver per reporter. Runtime delivery errors are
    // best-effort — logged and counted, the run still completes.
    if !reporters.is_empty() {
        let mut report = build_report(
            report_line.as_ref(),
            engine,
            vus,
            duration.clone(),
            &summary_lines,
            &labels,
        );
        if let Some(counters) = &streaming.counters {
            fold_reporter_counters(&mut report, counters);
        }
        let errors = deliver_reports(&reporters, &report).await;
        // `perfscale serve` keeps report artifacts: push the document next
        // to the summary lines when a target is connected.
        if let Some(url) = report_url.as_ref() {
            push_report_artifact(url, &report).await;
        }
        if errors > 0 {
            eprintln!("[report] {errors} reporter error(s) (engine_report_errors)");
        }
    }

    if !args.summary_export.is_empty() {
        let export = build_export(engine, vus, duration, &summary_lines);
        for path in &args.summary_export {
            write_summary_export(path, args.summary_format, &export)?;
        }
    }

    // A violated `severity: fail` thresholds gate fails the run for CI.
    // Checked last so the report/summary export above still happen.
    if let Some(t) = perfscale_core::summary::parse_thresholds(&summary_lines.join("\n")) {
        if t.status == "fail" {
            return Err(CliError::new(format!("thresholds failed: {}", t.message))
                .hint("one or more `std/thresholds@v1` gates evaluated to `fail` — the violations above name the metric, the threshold, and the actual value")
                .docs("core/actions.md#stdthresholdsv1"));
        }
    }

    Ok(())
}

/// Engine name and load shape for the export metadata. k6 and JMeter own
/// their load shape inside the script/plan, so vus/duration are unknown to
/// the CLI there; staged/arrival native runs have no fixed VU count either —
/// `vus` is null and `duration` is the summed stage length.
fn plan_meta(plan: &ExecutionPlan) -> (&'static str, Option<u32>, Option<String>) {
    match plan {
        ExecutionPlan::K6Script(_) => ("k6", None, None),
        ExecutionPlan::JMeterScript { .. } => ("jmeter", None, None),
        ExecutionPlan::LocustScript { opts, .. } => {
            ("locust", Some(opts.users), Some(opts.duration.clone()))
        }
        ExecutionPlan::NativeSteps { config, .. } => match config.resolve_schedule() {
            Ok(
                schedule @ (perfscale_core::step::schedule::Schedule::RampingVus { .. }
                | perfscale_core::step::schedule::Schedule::ArrivalRate { .. }),
            ) => (
                "native",
                None,
                Some(format!("{}s", schedule.total_secs() as u64)),
            ),
            // Fixed profile — or a broken one (the engine reports that error
            // itself at run time; the metadata falls back to the raw fields).
            _ => ("native", Some(config.vus), Some(config.duration.clone())),
        },
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
            perfscale_version: crate::version::VERSION.to_string(),
            engine: engine.to_string(),
            vus,
            duration,
            timestamp: iso8601_utc(secs),
        },
        summary: perfscale_core::summary::parse_summary(&summary_lines.join("\n")),
        thresholds: perfscale_core::summary::parse_thresholds(&summary_lines.join("\n")),
        gpu: perfscale_core::summary::parse_gpu_summary(&summary_lines.join("\n")),
        libraries: perfscale_core::summary::parse_libraries_summary(&summary_lines.join("\n")),
        llm: perfscale_core::summary::parse_llm_summary(&summary_lines.join("\n")),
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
    const MARKERS: [&str; 12] = [
        "vus",
        "iterations",
        "iteration_duration",
        "http_req",
        "http_reqs",
        "data_received",
        "data_sent",
        "checks",
        "thresholds",
        "gpu",
        "libraries",
        "llm....",
    ];
    let trimmed = text.trim_start();
    if MARKERS.iter().any(|m| trimmed.starts_with(m)) {
        return true;
    }

    // Native protocol metrics: e.g. `grpc_req_duration: avg=... count=...`,
    // `ws_msgs_sent: 50 5.00/s`, `fix_messages_received: 100 10.00/s`.
    // The name before the colon must be a valid metric identifier.
    if let Some(colon) = trimmed.find(':') {
        let name = &trimmed[..colon];
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            let after = trimmed[colon + 1..].trim_start();
            return after.starts_with("avg=") || after.contains("/s");
        }
    }

    false
}

/// Hint for import failures: only the remote-blocked error needs the flag
/// callout; everything else keeps the generic document hint.
fn import_hint(err: &str, fallback: &'static str) -> &'static str {
    if err.contains("--allow-remote-import") {
        "network imports are opt-in: add --allow-remote-import if you trust every import in the chain"
    } else {
        fallback
    }
}

async fn load_config(
    path: Option<&Path>,
    opts: &ImportOptions,
) -> Result<Option<ConfigFile>, CliError> {
    match path {
        Some(path) => {
            let config = import::load_config_file(path, opts).await.map_err(|e| {
                let title = if e.starts_with("failed to read") {
                    format!("failed to read config file '{}'", path.display())
                } else {
                    format!("invalid config file '{}'", path.display())
                };
                let hint = import_hint(
                    &e,
                    "valid fields: `vus` (integer), `duration` (\"30s\"/\"5m\"/\"1h\"), `stages` (ramping VUs) or `arrival` (arrival-rate), optional `report.url`, optional `import`",
                );
                CliError::new(title)
                    .cause(e)
                    .hint(hint)
                    .docs("yaml-reference.md#config--c-configyaml")
            })?;
            Ok(Some(config))
        }
        None => Ok(None),
    }
}

async fn load_test_def(path: &Path, opts: &ImportOptions) -> Result<TestDef, CliError> {
    import::load_test_file(path, opts).await.map_err(|e| {
        let title = if e.starts_with("failed to read") {
            format!("failed to read test file '{}'", path.display())
        } else {
            format!("invalid test file '{}'", path.display())
        };
        let hint = import_hint(
            &e,
            "each step needs `use:` naming an action (std/http@v1, std/check@v1, std/sleep@v1, std/log@v1, std/file-read@v1, std/file-write@v1); \
             parameters go under `with:`",
        );
        CliError::new(title)
            .cause(e)
            .hint(hint)
            .docs("yaml-reference.md#test-definition--f-testyaml")
    })
}

/// Resolve CLI flags + parsed config into an [`ExecutionPlan`]. Pure — no I/O.
///
/// `native_test` must be `Some` iff `args.file` is set; the caller loads and
/// parses it beforehand since that step needs the filesystem.
///
/// `flag_reporters`/`labels` (parsed from `--report json:PATH` / `--label`)
/// fold into the native run config's report block so the engine knows to
/// emit the report@v1 document; delivery itself happens CLI-side.
fn resolve_plan(
    args: &RunArgs,
    native_test: Option<TestDef>,
    config: Option<&ConfigFile>,
    flag_reporters: &[perfscale_core::report::ReporterConfig],
    labels: &serde_json::Map<String, serde_json::Value>,
    input_overrides: &[(String, String)],
    env_inputs: &serde_json::Map<String, serde_json::Value>,
) -> Result<ExecutionPlan, CliError> {
    if let Some(script) = &args.k6 {
        return Ok(ExecutionPlan::K6Script(script.clone()));
    }

    if let Some(plan) = &args.jmeter {
        // JMeter plans own their load shape (thread groups, timers); the CLI
        // contributes `-J` properties, `report:` (streaming + end-of-run
        // reporters + labels) and `after:` `std/thresholds@v1` gates.
        let mut opts = JMeterOpts {
            properties: parse_jmeter_properties(&args.jmeter_property)?,
            ..Default::default()
        };
        if let Some(cfg) = config {
            opts.thresholds = jmeter_config_gates(cfg)?;
            if let Some(report) = &cfg.report {
                opts.report = Some(report.to_run_config());
            }
        }
        if !flag_reporters.is_empty() || !labels.is_empty() {
            let report = opts.report.get_or_insert_with(Default::default);
            report.reporters.extend(flag_reporters.iter().cloned());
            for (k, v) in labels {
                report.labels.insert(k.clone(), v.clone());
            }
        }
        return Ok(ExecutionPlan::JMeterScript {
            path: plan.clone(),
            opts,
        });
    }

    if let Some(script) = &args.locust {
        let opts = match config {
            Some(cfg) => LocustOpts::from_run_config(&cfg.run, args.host.clone()).map_err(|e| {
                CliError::new(e)
                    .hint("`stages:`/`arrival:` need the native engine: `perfscale run -f test.yaml -c config.yaml`")
                    .docs("core/runners.md#native-step-engine")
            })?,
            None => LocustOpts {
                host: args.host.clone(),
                ..LocustOpts::default()
            },
        };
        return Ok(ExecutionPlan::LocustScript {
            path: script.clone(),
            opts,
        });
    }

    if args.file.is_some() {
        let test = native_test.expect("caller must load the test def when --file is set");
        // `-f` requires `-c` (enforced by clap), so config is always present here.
        let cfg = config.expect("clap requires -c with -f");
        let mut run = cfg.run.clone();
        // Fold the `report:` block into the engine's run config — the engine
        // reads `report.during_run` to decide whether to pump snapshots, and
        // `report.reporters` to decide whether to emit the report@v1 line.
        // CLI `--report json:PATH` / `--label` specs merge in on top.
        if let Some(report) = &cfg.report {
            run.report = Some(report.to_run_config());
        }
        if !flag_reporters.is_empty() || !labels.is_empty() {
            let report = run.report.get_or_insert_with(Default::default);
            report.reporters.extend(flag_reporters.iter().cloned());
            for (k, v) in labels {
                report.labels.insert(k.clone(), v.clone());
            }
        }
        // Fold the `webrtc:` block in too (RFC 007) — pro/webrtc-* actions
        // read ICE servers and the peer-connection guardrail off the context.
        run.webrtc = cfg.webrtc.clone();
        // Typed inputs (issue #11): declarations validate, then values
        // resolve with `-i` > env file > `default:` precedence. Everything
        // is fail-fast here — the engine receives the resolved map only.
        match &cfg.inputs {
            Some(decls) => {
                perfscale_core::inputs::validate_inputs(decls).map_err(|msg| {
                    CliError::new(format!("invalid inputs configuration: {msg}"))
                        .docs("yaml-reference.md#config--c-configyaml")
                })?;
                run.input_values = perfscale_core::inputs::resolve_inputs(
                    decls,
                    input_overrides,
                    env_inputs,
                )
                .map_err(|msg| {
                    CliError::new(format!("invalid inputs: {msg}"))
                        .hint("declare the input in the config's `inputs:` block, or pass `-i name=value`")
                        .docs("yaml-reference.md#config--c-configyaml")
                })?;
            }
            None if !input_overrides.is_empty() => {
                return Err(CliError::new(format!(
                    "unknown input '{}' — the config declares no `inputs:` block",
                    input_overrides[0].0
                ))
                .hint("declare inputs in the config file first: `inputs: { name: string }`")
                .docs("yaml-reference.md#config--c-configyaml"));
            }
            None => {}
        }
        // Config extension blocks (issue #10): the config document's
        // library-claimed top-level keys. Validated against the claiming
        // libraries at run start (engine-side).
        run.config_extensions = cfg.extensions.clone();
        // `libraries:` from the config and the test file concatenate (RFC
        // 005); a duplicate alias across them is a validation error at run
        // start (engine-side, so the agent path gets the same check).
        let libraries = {
            let mut libs = cfg.libraries.clone().unwrap_or_default();
            libs.extend(test.libraries.clone().unwrap_or_default());
            libs
        };
        return Ok(ExecutionPlan::NativeSteps {
            test,
            config: Box::new(run),
            before: cfg.before.clone(),
            after: cfg.after.clone(),
            variables: cfg.variables.clone(),
            shared_variables: cfg.shared_variables.clone(),
            libraries,
            quiet: args.quiet,
            // Set later by the caller once the report URL is resolved.
            metrics_tx: None,
        });
    }

    unreachable!("clap ArgGroup guarantees exactly one of --k6/--locust/--jmeter/-f")
}

/// Parse the repeatable `-J`/`--jmeter-property` values. Split on the first
/// `=` (values may contain `=`); an empty key is a CLI error. Property
/// values are never logged — they can carry credentials.
fn parse_jmeter_properties(values: &[String]) -> Result<Vec<(String, String)>, CliError> {
    let mut out = Vec::new();
    for v in values {
        let (k, val) = v.split_once('=').ok_or_else(|| {
            CliError::new("invalid -J/--jmeter-property value")
                .hint("expected KEY=VALUE, e.g. `-Jvus=50` or `--jmeter-property host=example.com`")
        })?;
        if k.is_empty() {
            return Err(CliError::new("invalid -J/--jmeter-property value")
                .hint("the property key must be non-empty, e.g. `-Jvus=50`"));
        }
        out.push((k.to_string(), val.to_string()));
    }
    Ok(out)
}

/// What a `-c` config may contribute to a `--jmeter` run: `after:`
/// `std/thresholds@v1` gates (returned as parameter sets with step-level
/// `severity`/`message` merged in, like the native runner's step params).
///
/// Everything load-shaping or native-engine-only is rejected outright rather
/// than silently ignored — the plan owns the load shape, and a config that
/// looks applied but isn't is worse than no config.
fn jmeter_config_gates(cfg: &ConfigFile) -> Result<Vec<serde_json::Value>, CliError> {
    let run = &cfg.run;
    if run.vus != 1 || run.duration != "1m" || !run.stages.is_empty() || run.arrival.is_some() {
        return Err(
            CliError::new("the config's load settings (vus/duration/stages/arrival) do not apply to --jmeter — the plan owns the load shape")
                .hint("parameterize the plan with -J properties (`-Jvus=50` → `${__P(vus,5)}` in the .jmx); vus/duration/stages/arrival need the native engine (-f)")
                .docs("core/jmeter.md"),
        );
    }
    if run.gpu.is_some() || run.engine_metrics.is_some() {
        return Err(CliError::new(
            "`gpu:`/`engine_metrics:` are native-engine only and do not apply to --jmeter",
        )
        .hint("drop them from the config for JMeter runs, or use the native engine (-f)")
        .docs("core/jmeter.md"));
    }
    if !cfg.before.is_empty() {
        return Err(CliError::new("`before:` setup steps do not apply to --jmeter — only `after:` thresholds gates are supported")
            .hint("move setup into the plan (setUp Thread Group) or use the native engine (-f)")
            .docs("core/jmeter.md"));
    }
    if !cfg.variables.is_empty()
        || !cfg.shared_variables.is_empty()
        || cfg.inputs.is_some()
        || cfg.libraries.is_some()
        || !cfg.extensions.is_empty()
        || cfg.webrtc.is_some()
    {
        return Err(CliError::new("the config's variables/shared_variables/inputs/libraries/extensions/webrtc blocks are native-engine only and do not apply to --jmeter")
            .hint("with --jmeter, only `report:` and `after:` `std/thresholds@v1` gates apply; parameterize the plan with -J properties")
            .docs("core/jmeter.md"));
    }

    let mut gates = Vec::new();
    for step in &cfg.after {
        if !perfscale_core::step::thresholds::is_thresholds_action(&step.action) {
            return Err(CliError::new(format!(
                "`after:` action '{}' does not apply to --jmeter — only std/thresholds@v1 gates are supported",
                step.action
            ))
            .hint("other after: actions need the native engine (-f)")
            .docs("core/jmeter.md"));
        }
        if step.abort_on_fail || step.http_debug.is_some() {
            return Err(CliError::new("`abort_on_fail`/`http_debug` are not supported on --jmeter thresholds gates")
                .hint("jmeter owns the run, so the gate can only evaluate after it finishes — drop `abort_on_fail` (`severity: fail` still exits non-zero)")
                .docs("core/jmeter.md"));
        }
        let mut obj = step
            .with
            .as_ref()
            .and_then(|w| w.as_object())
            .cloned()
            .ok_or_else(|| {
                CliError::new("a thresholds gate needs a `with:` map of metric → expressions")
                    .hint("e.g. `with: { http_req_duration: [\"p95<500\"] }`")
                    .docs("core/actions.md#stdthresholdsv1")
            })?;
        if let Some(severity) = &step.severity {
            obj.insert(
                "severity".into(),
                serde_json::Value::String(severity.clone()),
            );
        }
        if let Some(message) = &step.message {
            obj.insert("message".into(), serde_json::Value::String(message.clone()));
        }
        gates.push(serde_json::Value::Object(obj));
    }
    Ok(gates)
}

/// The serve target: a `--report http(s)://…` flag wins over the config
/// file's `report.url`. An empty config `url` (a block that exists only for
/// `reporters`/`labels`) counts as unset.
fn resolve_report_url(flag_url: Option<&String>, config: Option<&ConfigFile>) -> Option<String> {
    flag_url.cloned().or_else(|| {
        config.and_then(|c| {
            c.report
                .as_ref()
                .map(|r| r.url.clone())
                .filter(|u| !u.is_empty())
        })
    })
}

/// Parse the repeatable `-i/--input` values into key/value overrides.
/// Both `-i users=10 -i env=prod` and `-i users=10,env=prod` work; a literal
/// comma in a value is escaped as `\,` (and `\\` is a literal backslash).
pub(crate) fn parse_input_overrides(values: &[String]) -> Result<Vec<(String, String)>, CliError> {
    let mut out = Vec::new();
    for v in values {
        for item in split_escaped_commas(v) {
            let (k, val) = item.split_once('=').ok_or_else(|| {
                CliError::new(format!("invalid -i value '{item}'"))
                    .hint("expected KEY=VALUE, e.g. `-i users=10` or `-i users=10,env=prod`")
            })?;
            if k.is_empty() {
                return Err(CliError::new(format!("invalid -i value '{item}'"))
                    .hint("the key must be non-empty, e.g. `-i users=10`"));
            }
            out.push((k.to_string(), val.to_string()));
        }
    }
    Ok(out)
}

/// Split on commas, honoring the `\,` escape (and `\\` for a literal
/// backslash). Any other backslash sequence keeps both characters.
fn split_escaped_commas(s: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(',') => cur.push(','),
                Some('\\') => cur.push('\\'),
                Some(other) => {
                    cur.push('\\');
                    cur.push(other);
                }
                None => cur.push('\\'),
            },
            ',' => items.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    items.push(cur);
    items
}

/// Load the `-e/--env-file` inputs: KEY=VALUE files merged in order (later
/// files override earlier ones).
pub(crate) fn load_env_files(
    paths: &[PathBuf],
) -> Result<serde_json::Map<String, serde_json::Value>, CliError> {
    let mut out = serde_json::Map::new();
    for path in paths {
        let text = std::fs::read_to_string(path).map_err(|e| {
            CliError::new(format!("failed to read env file '{}': {e}", path.display()))
        })?;
        let parsed = perfscale_core::inputs::parse_env_file(&text)
            .map_err(|e| CliError::new(format!("invalid env file '{}': {e}", path.display())))?;
        out.extend(parsed);
    }
    Ok(out)
}

/// Split the repeatable `--report` values into the serve URL and reporter
/// specs. `http(s)://…` values keep the historical serve-URL meaning (at
/// most one); `json:PATH` / `junit:PATH` / `html:PATH` declare an end-of-run
/// reporter. Anything else is a configuration error — fail fast, before the
/// run.
fn parse_report_flags(
    values: &[String],
) -> Result<(Option<String>, Vec<perfscale_core::report::ReporterConfig>), CliError> {
    let mut url: Option<String> = None;
    let mut reporters = Vec::new();
    for v in values {
        if v.starts_with("http://") || v.starts_with("https://") {
            if url.is_some() {
                return Err(CliError::new(
                    "only one --report URL (perfscale serve target) may be given",
                ));
            }
            url = Some(v.clone());
            continue;
        }
        let (name, path) = v.split_once(':').ok_or_else(|| {
            CliError::new(format!("invalid --report value '{v}'"))
                .hint("expected an http(s):// URL or a reporter spec like `json:out.json` / `junit:out.xml` / `html:out.html`")
        })?;
        let Some(kind) = perfscale_core::report::reporter_kind(name) else {
            return Err(CliError::new(format!("unknown reporter '{name}'"))
                .hint("built-in reporters: json, junit, html (config file: std/report/json@v1, std/report/junit@v1, std/report/html@v1)"));
        };
        if !kind.capabilities().final_ {
            return Err(CliError::new(format!(
                "--report {name} is a streaming reporter, not a final one"
            ))
            .hint("streaming reporters (std/report/serve@v1, std/report/prometheus-rw@v1, std/report/otel@v1) are configured in the config file's `report.reporters` block with `during_run: true`"));
        }
        if path.is_empty() {
            return Err(
                CliError::new(format!("--report {name}: needs a destination path"))
                    .hint("e.g. `--report junit:out.xml`"),
            );
        }
        reporters.push(perfscale_core::report::ReporterConfig {
            use_: name.to_string(),
            during_run: false,
            interval_ms: None,
            labels: Default::default(),
            max_series: None,
            max_samples: None,
            with: perfscale_core::report::ReporterWith {
                path: Some(path.to_string()),
                ..Default::default()
            },
        });
    }
    Ok((url, reporters))
}

/// Parse `--label KEY=VALUE` values into the report's run labels.
fn parse_labels(values: &[String]) -> Result<serde_json::Map<String, serde_json::Value>, CliError> {
    let mut out = serde_json::Map::new();
    for v in values {
        let (k, val) = v.split_once('=').ok_or_else(|| {
            CliError::new(format!("invalid --label value '{v}'"))
                .hint("expected KEY=VALUE, e.g. `--label testid=smoke`")
        })?;
        if k.is_empty() {
            return Err(CliError::new(format!("invalid --label value '{v}'"))
                .hint("the key must be non-empty, e.g. `--label env=staging`"));
        }
        if perfscale_core::report::reporters::RESERVED_LABELS.contains(&k) {
            return Err(CliError::new(format!("invalid --label value '{v}'"))
                .hint("`run_id` and `worker` are reserved — the engine always sets them"));
        }
        out.insert(k.to_string(), serde_json::Value::String(val.to_string()));
    }
    Ok(out)
}

/// The reporters the resolved plan will deliver: the native run config's
/// merged list, the JMeter opts' report block (config + CLI flags, folded at
/// plan resolution), or the CLI's own specs for k6/locust (their configs
/// never carry a report block).
fn plan_reporters(
    plan: &ExecutionPlan,
    flag_reporters: &[perfscale_core::report::ReporterConfig],
) -> Vec<perfscale_core::report::ReporterConfig> {
    match plan {
        ExecutionPlan::NativeSteps { config, .. } => config
            .report
            .as_ref()
            .map(|r| r.reporters.clone())
            .unwrap_or_default(),
        ExecutionPlan::JMeterScript { opts, .. } => opts
            .report
            .as_ref()
            .map(|r| r.reporters.clone())
            .unwrap_or_default(),
        // k6/locust have no config report block; their reporters come from
        // the CLI flags alone, and the report is built from the parsed
        // summary — no per-step section.
        _ => flag_reporters.to_vec(),
    }
}

/// Streaming reporters (issue #12): every reporter whose `during_run` is set
/// gets its own bounded delivery worker; one engine snapshot pump fans out to
/// all of them. The historical `report.url` + `report.during_run` path maps
/// to an implicit `std/report/serve@v1` reporter (skipped when the config
/// already declares one) with the run-level knobs it always used.
///
/// The CLI ships unauthenticated — authenticated streaming is the agent's
/// path. Returns the fan-out + worker join handles so the caller can await
/// the final flush before exit, plus the run's reporter-counter registry.
fn wire_streaming(
    plan: &mut ExecutionPlan,
    report_url: Option<&String>,
) -> Result<StreamingWiring, CliError> {
    // Both engines that stream carry the same pieces: a `report` slot, the
    // snapshot channel to fill, and the interpolation namespace for reporter
    // labels (typed inputs are native-only).
    let (report_slot, metrics_tx, inputs) = match plan {
        ExecutionPlan::NativeSteps {
            config, metrics_tx, ..
        } => (
            &mut config.report,
            metrics_tx,
            perfscale_core::inputs::inputs_namespace(&config.input_values),
        ),
        ExecutionPlan::JMeterScript { opts, .. } => (
            &mut opts.report,
            &mut opts.metrics_tx,
            serde_json::Value::Null,
        ),
        _ => return Ok(StreamingWiring::default()),
    };
    let Some(report) = report_slot.as_ref() else {
        return Ok(StreamingWiring::default());
    };

    use perfscale_core::report::{reporter_kind, ReporterConfig, ReporterKind, ReporterWith};

    // The streaming reporters: config-declared ones with `during_run`, plus
    // the legacy `report.url` mapping.
    let mut streamers: Vec<ReporterConfig> = report
        .reporters
        .iter()
        .filter(|r| r.during_run && reporter_kind(&r.use_).is_some_and(|k| k.capabilities().stream))
        .cloned()
        .collect();
    let has_explicit_serve = streamers
        .iter()
        .any(|r| reporter_kind(&r.use_) == Some(ReporterKind::Serve));
    if let Some(url) = report_url.filter(|_| report.during_run && !has_explicit_serve) {
        streamers.push(ReporterConfig {
            use_: "std/report/serve@v1".into(),
            during_run: true,
            interval_ms: None,
            labels: Default::default(),
            max_series: None,
            max_samples: None,
            with: ReporterWith {
                url: Some(url.clone()),
                ..Default::default()
            },
        });
    }
    if streamers.is_empty() {
        return Ok(StreamingWiring::default());
    }

    // Per-run reporter counters (issue #12): the fan-out injects them into
    // every snapshot as counter series, so each backend receives them, and
    // the run end folds them into the report. Never process-global —
    // concurrent runs must not contaminate each other.
    let counters =
        std::sync::Arc::new(perfscale_core::report::delivery::ReporterCounters::default());

    // Reporter labels resolve once, before the run (`${{ inputs.* }}` /
    // `${{ env.* }}`), like step and run labels do engine-side.
    let run_interval = report.interval();
    let report_cfg = report.clone();

    // The engine pump ticks at the fastest requested interval; each
    // reporter's worker seals batches at its own.
    let min_interval_ms = streamers
        .iter()
        .map(|r| r.interval(run_interval).as_millis() as u64)
        .min()
        .unwrap_or(report_cfg.interval_ms);
    {
        let report = report_slot.as_mut().expect("checked above");
        report.during_run = true;
        report.interval_ms = min_interval_ms.max(perfscale_core::report::MIN_INTERVAL_MS);
    }
    let (tx, rx) = tokio::sync::mpsc::channel(512);
    *metrics_tx = Some(tx);
    let cpu = std::sync::Arc::new(perfscale_core::report::cpu::CpuReader::new());
    let mut queues = Vec::new();
    let mut handles = Vec::new();
    for r in &streamers {
        let kind = reporter_kind(&r.use_).expect("validated: stream-capable built-in");
        let labels = perfscale_core::report::stream::interpolate_label_values(&r.labels, &inputs);
        let retry = match &r.with.retry {
            Some(rc) => {
                let (min, max) = rc
                    .backoff_range()
                    .map_err(|e| CliError::new(format!("reporter '{}': {e}", r.use_)))?;
                perfscale_core::report::delivery::RetryPolicy {
                    max_attempts: rc.max_attempts,
                    backoff_min: min,
                    backoff_max: max,
                }
            }
            None => perfscale_core::report::delivery::RetryPolicy::default(),
        };
        let final_timeout = match r.with.final_timeout.as_deref() {
            Some(v) => Duration::from_millis(
                perfscale_core::step::parse_duration_ms_strict(v).map_err(|e| {
                    CliError::new(format!(
                        "reporter '{}': invalid `with.final_timeout`: {e}",
                        r.use_
                    ))
                })?,
            ),
            None => Duration::from_secs(30),
        };
        let cfg = perfscale_core::report::delivery::DeliveryConfig {
            interval: r.interval(run_interval),
            // Per-reporter `with.batch_size`, falling back to the run-level
            // `report.batch_size`.
            batch_size: r.with.batch_size.unwrap_or(report_cfg.batch_size),
            max_pending: r.with.max_pending.unwrap_or(report_cfg.max_pending),
            retry,
            final_timeout,
            // The CPU gate is the legacy serve knob (`report.max_cpu_percent`).
            cpu_gate: if kind == ReporterKind::Serve && report_cfg.max_cpu_percent > 0.0 {
                let cpu = std::sync::Arc::clone(&cpu);
                Some((
                    report_cfg.max_cpu_percent,
                    std::sync::Arc::new(move || cpu.sample())
                        as perfscale_core::report::delivery::CpuReader,
                ))
            } else {
                None
            },
            counters: Some(std::sync::Arc::clone(&counters)),
        };
        let queue = std::sync::Arc::new(
            perfscale_core::report::delivery::SnapshotQueue::new(r.use_.clone(), cfg.max_pending)
                .with_counters(std::sync::Arc::clone(&counters)),
        );
        match kind {
            ReporterKind::Serve => {
                let sink = perfscale_core::report::serve::ServeSink::new(
                    r.with.url.clone().expect("validated"),
                    labels,
                    None,
                    None,
                    r.max_series(),
                )
                .with_counters_registry(std::sync::Arc::clone(&counters));
                handles.push(perfscale_core::report::delivery::spawn(
                    sink,
                    cfg,
                    std::sync::Arc::clone(&queue),
                ));
            }
            ReporterKind::PrometheusRw => {
                let sink = perfscale_core::report::prometheus_rw::PromRwSink::new(
                    r.with.url.clone().expect("validated"),
                    &r.with.headers,
                    labels,
                    r.max_series(),
                    r.with.histogram.as_deref(),
                    "0".to_string(),
                )
                .map_err(|e| CliError::new(format!("reporter '{}': {e}", r.use_)))?
                .with_counters_registry(std::sync::Arc::clone(&counters));
                handles.push(perfscale_core::report::delivery::spawn(
                    sink,
                    cfg,
                    std::sync::Arc::clone(&queue),
                ));
            }
            ReporterKind::Otel => {
                let sink = perfscale_core::report::otel::OtelSink::new(
                    r.with.endpoint.clone().expect("validated"),
                    labels,
                    r.max_series(),
                    "0".to_string(),
                )
                .with_counters_registry(std::sync::Arc::clone(&counters));
                handles.push(perfscale_core::report::delivery::spawn(
                    sink,
                    cfg,
                    std::sync::Arc::clone(&queue),
                ));
            }
            _ => unreachable!("only streaming kinds collected above"),
        }
        queues.push(queue);
    }
    handles.push(perfscale_core::report::delivery::spawn_fanout(
        rx,
        queues,
        Some(std::sync::Arc::clone(&counters)),
    ));
    eprintln!(
        "[report] streaming metrics to {} reporter(s): {}",
        streamers.len(),
        streamers
            .iter()
            .map(|r| r.use_.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(StreamingWiring {
        handles,
        counters: Some(counters),
    })
}

/// What [`wire_streaming`] attached to the plan: the task handles to await
/// and the per-run reporter-counter registry (issue #12).
#[derive(Default)]
struct StreamingWiring {
    handles: Vec<tokio::task::JoinHandle<()>>,
    counters: Option<std::sync::Arc<perfscale_core::report::delivery::ReporterCounters>>,
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

/// The report@v1 document for this run: parsed from the native engine's
/// `report: {…}` line when present, otherwise built minimal CLI-side
/// (script engines carry no per-step/metric detail; verdicts still come
/// from the parsed thresholds line).
fn build_report(
    report_line: Option<&String>,
    engine: &str,
    vus: Option<u32>,
    duration: Option<String>,
    summary_lines: &[String],
    labels: &serde_json::Map<String, serde_json::Value>,
) -> perfscale_core::report::v1::ReportV1 {
    use perfscale_core::report::v1;

    if let Some(line) = report_line {
        if let Ok(report) =
            serde_json::from_str::<v1::ReportV1>(line.trim_start_matches("report: "))
        {
            return report;
        }
        eprintln!(
            "[report] could not parse the engine's report line — falling back to a minimal report"
        );
    }

    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let thresholds = perfscale_core::summary::parse_thresholds(&summary_lines.join("\n"));
    v1::ReportV1 {
        schema_version: v1::SCHEMA_VERSION.to_string(),
        run: v1::RunIdentity {
            run_id: uuid::Uuid::new_v4().to_string(),
            engine: engine.to_string(),
            perfscale_version: crate::version::VERSION.to_string(),
            vus,
            started_at: iso8601_utc(secs),
            duration_s: duration
                .as_deref()
                .map(perfscale_core::step::parse_duration_ms)
                .map(|ms| ms as f64 / 1000.0)
                .unwrap_or(0.0),
            labels: labels.clone(),
        },
        steps: Vec::new(),
        verdicts: thresholds
            .map(|t| {
                t.checks
                    .iter()
                    .map(|c| v1::Verdict {
                        threshold: c.metric.clone(),
                        expression: c.expr.clone(),
                        pass: c.pass,
                        observed: c.actual,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        metrics: Vec::new(),
        seq: None,
        samples: Vec::new(),
    }
}

/// Fold the per-run reporter delivery counters (issue #12) into the
/// `report@v1` metrics section: one series per nonzero counter, labeled by
/// reporter. Registered family names carry no `_total` suffix (the
/// streaming exporters add it on the wire); the report's `series` values
/// are plain counts.
fn fold_reporter_counters(
    report: &mut perfscale_core::report::v1::ReportV1,
    counters: &perfscale_core::report::delivery::ReporterCounters,
) {
    use perfscale_core::report::v1;
    let by_metric: std::collections::BTreeMap<String, Vec<(String, u64)>> = counters
        .snapshot()
        .into_iter()
        .filter(|(_, _, v)| *v > 0)
        .fold(Default::default(), |mut acc, (m, r, v)| {
            acc.entry(m).or_default().push((r, v));
            acc
        });
    for (metric, reporters) in by_metric {
        let name = metric.strip_suffix("_total").unwrap_or(&metric).to_string();
        let series = reporters
            .into_iter()
            .map(|(reporter, value)| v1::MetricSeries {
                labels: serde_json::Map::from_iter([(
                    "reporter".to_string(),
                    serde_json::Value::String(reporter),
                )]),
                value: Some(value as f64),
                ..v1::MetricSeries::default()
            })
            .collect();
        // Merge into the family's existing entry when present (the engine
        // emits the family with `recorded: false` otherwise).
        match report.metrics.iter_mut().find(|f| f.name == name) {
            Some(f) => {
                f.recorded = true;
                f.series.extend(series);
            }
            None => report.metrics.push(v1::MetricFamilyReport {
                name,
                family: "report".to_string(),
                kind: "counter".to_string(),
                unit: "count".to_string(),
                description: String::new(),
                recorded: true,
                series,
            }),
        }
    }
}

/// Render and deliver each configured reporter. Returns the number of
/// delivery errors — the `engine_report_errors` count. Best-effort: errors
/// are logged, never propagated.
async fn deliver_reports(
    reporters: &[perfscale_core::report::ReporterConfig],
    report: &perfscale_core::report::v1::ReportV1,
) -> u64 {
    let mut errors = 0u64;
    for r in reporters {
        let mut doc = report.clone();
        doc.truncate_samples(r.max_samples());
        // WASM reporters (issue #9): the guest renders and writes via the
        // host's emit-file — there is no host-side body or destination.
        if let Some(module) = perfscale_core::report::wasm_reporter_module(&r.use_) {
            match deliver_wasm_reporter(r, module, &doc).await {
                Ok(summary) => eprintln!("[report] {}: {summary}", r.use_),
                Err(e) => {
                    errors += 1;
                    eprintln!("[report] reporter {} failed: {e}", r.use_);
                }
            }
            continue;
        }
        let body = match perfscale_core::report::reporter_kind(&r.use_) {
            Some(perfscale_core::report::ReporterKind::Json) => doc.to_json(),
            Some(perfscale_core::report::ReporterKind::Junit) => doc.to_junit_xml(),
            Some(perfscale_core::report::ReporterKind::Html) => doc.to_html(),
            // Streaming reporters (serve/prometheus-rw/otel) never receive
            // the end-of-run document; unknown ids were rejected pre-run.
            _ => continue,
        };
        let result = if let Some(path) = &r.with.path {
            std::fs::write(path, &body)
                .map(|_| {
                    eprintln!("[report] {} report written to {path}", r.use_);
                })
                .map_err(|e| e.to_string())
        } else if let Some(url) = &r.with.url {
            push_report_http(r, url, &body).await.map(|_| {
                eprintln!("[report] {} report pushed to {url}", r.use_);
            })
        } else {
            continue; // validated before the run started
        };
        if let Err(e) = result {
            errors += 1;
            eprintln!("[report] reporter {} failed: {e}", r.use_);
        }
    }
    errors
}

/// WASM reporter delivery: load the component, hand it the report@v1
/// document plus its `with.config`, and let it write through `emit-file`.
/// The module was already compile-checked by `validate_reporters` before the
/// run; a guest error here is best-effort (counted, never run-killing).
async fn deliver_wasm_reporter(
    r: &perfscale_core::report::ReporterConfig,
    module: &str,
    doc: &perfscale_core::report::v1::ReportV1,
) -> Result<String, String> {
    let module = module.to_string();
    let output = r.with.output.clone();
    let config_json = serde_json::to_string(
        &r.with
            .config
            .clone()
            .unwrap_or_else(|| serde_json::json!({})),
    )
    .map_err(|e| e.to_string())?;
    let report_json = doc.to_json();
    // wasmtime is sync; keep the CLI runtime free while the guest renders.
    tokio::task::spawn_blocking(move || {
        let reporter = perfscale_core::report::wasm::WasmReporter::load(
            Path::new(&module),
            output.as_deref(),
        )?;
        reporter.run(&report_json, &config_json)
    })
    .await
    .map_err(|e| format!("reporter task failed to join: {e}"))?
}

/// HTTP push delivery. `${VAR}` references in header values resolve
/// host-side here; the resolved values are never logged.
async fn push_report_http(
    r: &perfscale_core::report::ReporterConfig,
    url: &str,
    body: &str,
) -> Result<(), String> {
    let content_type = match perfscale_core::report::reporter_kind(&r.use_) {
        Some(perfscale_core::report::ReporterKind::Junit) => "application/xml",
        Some(perfscale_core::report::ReporterKind::Html) => "text/html; charset=utf-8",
        _ => "application/json",
    };
    let client = reqwest::Client::new();
    let mut req = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(body.to_string())
        .timeout(Duration::from_secs(10));
    for (k, v) in &r.with.headers {
        let resolved = perfscale_core::report::resolve_env_refs(v)?;
        req = req.header(k, resolved);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => Err(format!("{url} returned {}", resp.status())),
        Err(e) => Err(format!("failed to reach {url}: {e}")),
    }
}

/// Hand the report@v1 artifact to a connected `perfscale serve` — the
/// dashboard stores it as a run artifact. Best-effort, like the summary
/// POST.
async fn push_report_artifact(url: &str, report: &perfscale_core::report::v1::ReportV1) {
    let endpoint = format!("{}/api/v1/report", url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    match client
        .post(&endpoint)
        .json(report)
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
            jmeter: None,
            jmeter_property: Vec::new(),
            file: None,
            config: None,
            host: None,
            report: Vec::new(),
            label: Vec::new(),
            quiet: false,
            summary_export: Vec::new(),
            summary_format: None,
            input: Vec::new(),
            env_file: Vec::new(),
            allow_remote_import: false,
            refresh_imports: false,
        }
    }

    fn sample_test() -> TestDef {
        TestDef {
            import: None,
            libraries: None,
            steps: vec![],
        }
    }

    fn sample_config(report_url: Option<&str>) -> ConfigFile {
        ConfigFile {
            import: None,
            libraries: None,
            run: RunConfig {
                vus: 4,
                duration: "2m".into(),
                ..Default::default()
            },
            report: report_url.map(|url| ReportConfig {
                url: url.to_string(),
                ..ReportConfig::default()
            }),
            before: Vec::new(),
            after: Vec::new(),
            variables: serde_json::Map::new(),
            inputs: None,
            extensions: serde_json::Map::new(),
            shared_variables: serde_json::Map::new(),
            webrtc: None,
        }
    }

    #[test]
    fn parse_input_overrides_repeat_comma_and_escape() {
        // Repeated flags and comma-separated forms both work.
        let out = parse_input_overrides(&["users=10".into(), "env=prod".into()]).unwrap();
        assert_eq!(
            out,
            vec![
                ("users".to_string(), "10".to_string()),
                ("env".to_string(), "prod".to_string())
            ]
        );
        let out = parse_input_overrides(&["users=10,env=prod".into()]).unwrap();
        assert_eq!(out.len(), 2);
        // `\,` is a literal comma inside a value.
        let out = parse_input_overrides(&[r"note=a\,b".into()]).unwrap();
        assert_eq!(out, vec![("note".to_string(), "a,b".to_string())]);
        // Missing '=' or empty key are CLI errors.
        assert!(parse_input_overrides(&["users".into()]).is_err());
        assert!(parse_input_overrides(&["=10".into()]).is_err());
    }

    #[test]
    fn load_env_files_merges_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.env");
        let b = dir.path().join("b.env");
        std::fs::write(&a, "# comment\nusers=7\nhost=a\n").unwrap();
        std::fs::write(&b, "host=b\n").unwrap();
        let out = load_env_files(&[a, b]).unwrap();
        assert_eq!(out["users"], serde_json::json!("7"));
        assert_eq!(out["host"], serde_json::json!("b"), "later file wins");
        let err = load_env_files(&[dir.path().join("missing.env")]).unwrap_err();
        assert!(err.to_string().contains("failed to read env file"), "{err}");
    }

    #[test]
    fn resolve_plan_resolves_inputs_into_the_run_config() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("inputs.env");
        std::fs::write(&env_file, "users=5\n").unwrap();
        let mut config = sample_config(None);
        config.inputs = Some(
            serde_json::from_value(serde_json::json!({
                "users": "number",
                "env": { "type": "choice", "options": ["staging", "prod"], "default": "staging" }
            }))
            .unwrap(),
        );
        let args = RunArgs {
            file: Some(PathBuf::from("t.yaml")),
            ..base_args()
        };
        // -i beats the env file; defaults fill the rest.
        let plan = resolve_plan(
            &args,
            Some(sample_test()),
            Some(&config),
            &[],
            &serde_json::Map::new(),
            &[("users".to_string(), "10".to_string())],
            &load_env_files(&[env_file]).unwrap(),
        )
        .unwrap();
        match plan {
            ExecutionPlan::NativeSteps { config, .. } => {
                assert_eq!(config.input_values["users"], serde_json::json!(10));
                assert_eq!(config.input_values["env"], serde_json::json!("staging"));
            }
            _ => panic!("expected NativeSteps plan"),
        }

        // Missing required input fails before the run.
        let err = resolve_plan(
            &args,
            Some(sample_test()),
            Some(&config),
            &[],
            &serde_json::Map::new(),
            &[],
            &serde_json::Map::new(),
        )
        .err()
        .expect("must fail");
        assert!(err.to_string().contains("required"), "{err}");

        // -i with no declared inputs fails too.
        let plain = sample_config(None);
        let err = resolve_plan(
            &args,
            Some(sample_test()),
            Some(&plain),
            &[],
            &serde_json::Map::new(),
            &[("users".to_string(), "10".to_string())],
            &serde_json::Map::new(),
        )
        .err()
        .expect("must fail");
        assert!(err.to_string().contains("no `inputs:` block"), "{err}");
    }

    #[test]
    fn resolve_plan_picks_k6_when_k6_flag_set() {
        let args = RunArgs {
            k6: Some(PathBuf::from("a.js")),
            ..base_args()
        };
        let plan = resolve_plan(
            &args,
            None,
            None,
            &[],
            &serde_json::Map::new(),
            &[],
            &serde_json::Map::new(),
        )
        .unwrap();
        assert!(matches!(plan, ExecutionPlan::K6Script(p) if p == Path::new("a.js")));
    }

    #[test]
    fn resolve_plan_picks_jmeter_when_jmeter_flag_set() {
        let args = RunArgs {
            jmeter: Some(PathBuf::from("plan.jmx")),
            ..base_args()
        };
        let plan = resolve_plan(
            &args,
            None,
            None,
            &[],
            &serde_json::Map::new(),
            &[],
            &serde_json::Map::new(),
        )
        .unwrap();
        match plan {
            ExecutionPlan::JMeterScript { path, opts } => {
                assert_eq!(path, Path::new("plan.jmx"));
                assert!(opts.properties.is_empty());
                assert!(opts.thresholds.is_empty());
                assert!(opts.report.is_none());
            }
            _ => panic!("expected JMeterScript plan"),
        }
    }

    #[test]
    fn parse_jmeter_properties_splits_on_first_equals() {
        let props =
            parse_jmeter_properties(&["vus=50".into(), "query=a=b".into(), "empty=".into()])
                .unwrap();
        assert_eq!(
            props,
            vec![
                ("vus".to_string(), "50".to_string()),
                ("query".to_string(), "a=b".to_string()),
                ("empty".to_string(), String::new()),
            ]
        );
        assert!(
            parse_jmeter_properties(&["=50".into()]).is_err(),
            "empty key"
        );
        assert!(parse_jmeter_properties(&["vus".into()]).is_err(), "no '='");
    }

    #[test]
    fn resolve_plan_jmeter_carries_properties_and_threshold_gates() {
        let args = RunArgs {
            jmeter: Some(PathBuf::from("plan.jmx")),
            jmeter_property: vec!["vus=50".into()],
            ..base_args()
        };
        let mut config = sample_config(None);
        config.run = RunConfig::default();
        config.after = vec![perfscale_core::step::Step {
            name: None,
            action: "std/thresholds@v1".into(),
            with: Some(serde_json::json!({ "http_req_duration": ["p95<500"] })),
            check: None,
            outputs: None,
            severity: Some("warn".into()),
            message: Some("SLO".into()),
            abort_on_fail: false,
            http_debug: None,
            condition: None,
            labels: None,
        }];
        let plan = resolve_plan(
            &args,
            None,
            Some(&config),
            &[],
            &serde_json::Map::new(),
            &[],
            &serde_json::Map::new(),
        )
        .unwrap();
        match plan {
            ExecutionPlan::JMeterScript { opts, .. } => {
                assert_eq!(opts.properties, vec![("vus".to_string(), "50".to_string())]);
                assert_eq!(opts.thresholds.len(), 1);
                let gate = &opts.thresholds[0];
                assert_eq!(gate["http_req_duration"], serde_json::json!(["p95<500"]));
                assert_eq!(gate["severity"], serde_json::json!("warn"));
                assert_eq!(gate["message"], serde_json::json!("SLO"));
            }
            _ => panic!("expected JMeterScript plan"),
        }
    }

    #[test]
    fn resolve_plan_jmeter_rejects_native_only_config() {
        let args = RunArgs {
            jmeter: Some(PathBuf::from("plan.jmx")),
            ..base_args()
        };
        let try_resolve = |config: &ConfigFile| {
            resolve_plan(
                &args,
                None,
                Some(config),
                &[],
                &serde_json::Map::new(),
                &[],
                &serde_json::Map::new(),
            )
        };
        // Load shape.
        let mut c = sample_config(None);
        c.run.vus = 4;
        let err = try_resolve(&c).err().expect("must fail");
        assert!(err.to_string().contains("load settings"), "{err}");
        let mut c = sample_config(None);
        c.run.stages = vec![perfscale_core::step::VuStage {
            duration: "30s".into(),
            target: 10,
        }];
        assert!(try_resolve(&c).is_err());
        // before: steps.
        let mut c = sample_config(None);
        c.run = RunConfig::default();
        c.before = vec![perfscale_core::step::Step {
            name: None,
            action: "std/log@v1".into(),
            with: None,
            check: None,
            outputs: None,
            severity: None,
            message: None,
            abort_on_fail: false,
            http_debug: None,
            condition: None,
            labels: None,
        }];
        let err = try_resolve(&c).err().expect("must fail");
        assert!(err.to_string().contains("before:"), "{err}");
        // Non-thresholds after: actions.
        let mut c = sample_config(None);
        c.run = RunConfig::default();
        c.after = vec![perfscale_core::step::Step {
            name: None,
            action: "std/log@v1".into(),
            with: Some(serde_json::json!({ "message": "hi" })),
            check: None,
            outputs: None,
            severity: None,
            message: None,
            abort_on_fail: false,
            http_debug: None,
            condition: None,
            labels: None,
        }];
        let err = try_resolve(&c).err().expect("must fail");
        assert!(err.to_string().contains("std/log@v1"), "{err}");
        // abort_on_fail is rejected, not silently honored.
        let mut c = sample_config(None);
        c.run = RunConfig::default();
        c.after = vec![perfscale_core::step::Step {
            name: None,
            action: "std/thresholds@v1".into(),
            with: Some(serde_json::json!({ "http_req_duration": ["p95<500"] })),
            check: None,
            outputs: None,
            severity: None,
            message: None,
            abort_on_fail: true,
            http_debug: None,
            condition: None,
            labels: None,
        }];
        let err = try_resolve(&c).err().expect("must fail");
        assert!(err.to_string().contains("abort_on_fail"), "{err}");
    }

    #[test]
    fn resolve_plan_jmeter_folds_report_flags_and_labels() {
        let (flag_url, flag_reporters) = parse_report_flags(&["json:out.json".into()]).unwrap();
        assert!(flag_url.is_none());
        let args = RunArgs {
            jmeter: Some(PathBuf::from("plan.jmx")),
            ..base_args()
        };
        let mut labels = serde_json::Map::new();
        labels.insert("env".into(), serde_json::json!("staging"));
        let plan = resolve_plan(
            &args,
            None,
            None,
            &flag_reporters,
            &labels,
            &[],
            &serde_json::Map::new(),
        )
        .unwrap();
        match &plan {
            ExecutionPlan::JMeterScript { opts, .. } => {
                let report = opts.report.as_ref().expect("report config folded");
                assert_eq!(report.reporters.len(), 1);
                assert_eq!(report.labels["env"], serde_json::json!("staging"));
            }
            _ => panic!("expected JMeterScript plan"),
        }
        // End-of-run delivery reads the opts' reporter list, not the flags.
        assert_eq!(plan_reporters(&plan, &[]).len(), 1);
    }

    #[tokio::test]
    async fn wire_during_run_attaches_channel_for_jmeter() {
        let url = "http://localhost:7999".to_string();
        let jmeter_plan =
            |report: Option<perfscale_core::report::ReportRunConfig>| ExecutionPlan::JMeterScript {
                path: PathBuf::from("plan.jmx"),
                opts: JMeterOpts {
                    report,
                    ..Default::default()
                },
            };

        // during_run + URL → channel attached, serve worker + fan-out.
        let mut plan = jmeter_plan(Some(perfscale_core::report::ReportRunConfig {
            during_run: true,
            ..Default::default()
        }));
        let wiring = wire_streaming(&mut plan, Some(&url)).unwrap();
        assert_eq!(wiring.handles.len(), 2);
        match &plan {
            ExecutionPlan::JMeterScript { opts, .. } => {
                assert!(opts.metrics_tx.is_some());
                assert!(opts.report.as_ref().unwrap().during_run);
            }
            _ => panic!("expected JMeterScript plan"),
        }
        for h in wiring.handles {
            h.abort();
        }

        // during_run off → untouched.
        let mut plan = jmeter_plan(Some(perfscale_core::report::ReportRunConfig::default()));
        assert!(wire_streaming(&mut plan, Some(&url))
            .unwrap()
            .handles
            .is_empty());
        match &plan {
            ExecutionPlan::JMeterScript { opts, .. } => assert!(opts.metrics_tx.is_none()),
            _ => panic!("expected JMeterScript plan"),
        }
    }

    #[test]
    fn resolve_plan_locust_without_config_uses_defaults() {
        let args = RunArgs {
            locust: Some(PathBuf::from("b.py")),
            host: Some("https://example.com".into()),
            ..base_args()
        };
        let plan = resolve_plan(
            &args,
            None,
            None,
            &[],
            &serde_json::Map::new(),
            &[],
            &serde_json::Map::new(),
        )
        .unwrap();
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
        let plan = resolve_plan(
            &args,
            None,
            Some(&config),
            &[],
            &serde_json::Map::new(),
            &[],
            &serde_json::Map::new(),
        )
        .unwrap();
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
        let plan = resolve_plan(
            &args,
            Some(sample_test()),
            Some(&config),
            &[],
            &serde_json::Map::new(),
            &[],
            &serde_json::Map::new(),
        )
        .unwrap();
        match plan {
            ExecutionPlan::NativeSteps { config, .. } => assert_eq!(config.vus, 4),
            _ => panic!("expected NativeSteps plan"),
        }
    }

    #[test]
    fn resolve_plan_native_maps_report_block_into_run_config() {
        let args = RunArgs {
            file: Some(PathBuf::from("t.yaml")),
            ..base_args()
        };
        let mut config = sample_config(Some("http://localhost:7999"));
        config.report.as_mut().unwrap().during_run = true;
        let plan = resolve_plan(
            &args,
            Some(sample_test()),
            Some(&config),
            &[],
            &serde_json::Map::new(),
            &[],
            &serde_json::Map::new(),
        )
        .unwrap();
        match plan {
            ExecutionPlan::NativeSteps {
                config, metrics_tx, ..
            } => {
                let report = config.report.as_ref().expect("report block mapped");
                assert!(report.during_run);
                assert_eq!(report.url.as_deref(), Some("http://localhost:7999"));
                // The channel is attached later, once the URL is resolved.
                assert!(metrics_tx.is_none());
            }
            _ => panic!("expected NativeSteps plan"),
        }
    }

    #[tokio::test]
    async fn wire_during_run_attaches_channel_only_for_opted_in_native_runs() {
        let url = "http://localhost:7999".to_string();
        let native_plan = |report: Option<perfscale_core::report::ReportRunConfig>| {
            let mut config = RunConfig {
                vus: 1,
                duration: "1s".into(),
                ..Default::default()
            };
            config.report = report;
            ExecutionPlan::NativeSteps {
                test: sample_test(),
                config: Box::new(config),
                before: Vec::new(),
                after: Vec::new(),
                variables: serde_json::Map::new(),
                shared_variables: serde_json::Map::new(),
                libraries: Vec::new(),
                quiet: false,
                metrics_tx: None,
            }
        };
        let has_channel = |plan: &ExecutionPlan| match plan {
            ExecutionPlan::NativeSteps { metrics_tx, .. } => metrics_tx.is_some(),
            _ => false,
        };

        // Opted in + URL resolved → channel attached, workers spawned.
        let mut plan = native_plan(Some(perfscale_core::report::ReportRunConfig {
            during_run: true,
            ..Default::default()
        }));
        let wiring = wire_streaming(&mut plan, Some(&url)).unwrap();
        // Legacy mapping: the serve reporter + its worker + the fan-out.
        assert_eq!(wiring.handles.len(), 2);
        assert!(wiring.counters.is_some());
        assert!(has_channel(&plan));
        for h in wiring.handles {
            h.abort();
        }

        // during_run off → untouched.
        let mut plan = native_plan(Some(perfscale_core::report::ReportRunConfig::default()));
        assert!(wire_streaming(&mut plan, Some(&url))
            .unwrap()
            .handles
            .is_empty());
        assert!(!has_channel(&plan));

        // Opted in but no URL anywhere → untouched.
        let mut plan = native_plan(Some(perfscale_core::report::ReportRunConfig {
            during_run: true,
            ..Default::default()
        }));
        assert!(wire_streaming(&mut plan, None).unwrap().handles.is_empty());
        assert!(!has_channel(&plan));

        // Non-native engines never stream.
        let mut plan = ExecutionPlan::K6Script(PathBuf::from("a.js"));
        assert!(wire_streaming(&mut plan, Some(&url))
            .unwrap()
            .handles
            .is_empty());
    }

    #[tokio::test]
    async fn wire_streaming_fans_out_to_config_reporters() {
        let reporter = |use_: &str, during_run: bool| perfscale_core::report::ReporterConfig {
            use_: use_.into(),
            during_run,
            interval_ms: None,
            labels: Default::default(),
            max_series: None,
            max_samples: None,
            with: perfscale_core::report::ReporterWith {
                url: Some("http://localhost:7999".into()),
                ..Default::default()
            },
        };
        let mut config = RunConfig {
            vus: 1,
            duration: "1s".into(),
            ..Default::default()
        };
        config.report = Some(perfscale_core::report::ReportRunConfig {
            reporters: vec![
                reporter("std/report/serve@v1", true),
                reporter("std/report/prometheus-rw@v1", true),
                // A final-only reporter never streams.
                reporter("std/report/json@v1", false),
                // A streaming reporter without during_run stays idle.
                reporter("std/report/serve@v1", false),
            ],
            ..Default::default()
        });
        let mut plan = ExecutionPlan::NativeSteps {
            test: sample_test(),
            config: Box::new(config),
            before: Vec::new(),
            after: Vec::new(),
            variables: serde_json::Map::new(),
            shared_variables: serde_json::Map::new(),
            libraries: Vec::new(),
            quiet: false,
            metrics_tx: None,
        };
        let wiring = wire_streaming(&mut plan, None).unwrap();
        // Two streaming reporters → two workers + the fan-out.
        assert_eq!(wiring.handles.len(), 3);
        for h in wiring.handles {
            h.abort();
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
        let _ = resolve_plan(
            &args,
            None,
            Some(&config),
            &[],
            &serde_json::Map::new(),
            &[],
            &serde_json::Map::new(),
        );
    }

    #[test]
    fn resolve_report_url_prefers_cli_flag_over_config() {
        let (flag_url, _) = parse_report_flags(&["http://cli-wins".into()]).unwrap();
        let config = sample_config(Some("http://config-loses"));
        assert_eq!(
            resolve_report_url(flag_url.as_ref(), Some(&config)).as_deref(),
            Some("http://cli-wins")
        );
    }

    #[test]
    fn resolve_report_url_falls_back_to_config() {
        let config = sample_config(Some("http://from-config"));
        assert_eq!(
            resolve_report_url(None, Some(&config)).as_deref(),
            Some("http://from-config")
        );
    }

    #[test]
    fn resolve_report_url_none_when_neither_set() {
        assert_eq!(resolve_report_url(None, None), None);
    }

    #[test]
    fn resolve_plan_native_passes_quiet_flag() {
        let args = RunArgs {
            file: Some(PathBuf::from("t.yaml")),
            quiet: true,
            ..base_args()
        };
        let config = sample_config(None);
        let plan = resolve_plan(
            &args,
            Some(sample_test()),
            Some(&config),
            &[],
            &serde_json::Map::new(),
            &[],
            &serde_json::Map::new(),
        )
        .unwrap();
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
            config: Box::new(RunConfig {
                vus: 7,
                duration: "45s".into(),
                ..Default::default()
            }),
            before: Vec::new(),
            after: Vec::new(),
            variables: serde_json::Map::new(),
            shared_variables: serde_json::Map::new(),
            libraries: Vec::new(),
            quiet: false,
            metrics_tx: None,
        };
        assert_eq!(plan_meta(&native), ("native", Some(7), Some("45s".into())));

        let k6 = ExecutionPlan::K6Script(PathBuf::from("a.js"));
        assert_eq!(plan_meta(&k6), ("k6", None, None));

        let jmeter = ExecutionPlan::JMeterScript {
            path: PathBuf::from("plan.jmx"),
            opts: JMeterOpts::default(),
        };
        assert_eq!(plan_meta(&jmeter), ("jmeter", None, None));

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

    /// Staged/arrival native runs have no fixed VU count: like k6, `vus` is
    /// null and `duration` reports the summed stage length.
    #[test]
    fn plan_meta_staged_native_run_has_null_vus() {
        let mut config = RunConfig {
            vus: 7,
            duration: "45s".into(),
            ..Default::default()
        };
        config.stages = vec![
            perfscale_core::step::VuStage {
                duration: "1s".into(),
                target: 5,
            },
            perfscale_core::step::VuStage {
                duration: "2s".into(),
                target: 0,
            },
        ];
        let staged = ExecutionPlan::NativeSteps {
            test: sample_test(),
            config: Box::new(config),
            before: Vec::new(),
            after: Vec::new(),
            variables: serde_json::Map::new(),
            shared_variables: serde_json::Map::new(),
            libraries: Vec::new(),
            quiet: false,
            metrics_tx: None,
        };
        assert_eq!(plan_meta(&staged), ("native", None, Some("3s".into())));
    }

    /// `--locust` combined with a native-only load profile fails at plan
    /// time with a pointer at the native engine.
    #[test]
    fn resolve_plan_locust_rejects_stages_config() {
        let args = RunArgs {
            locust: Some(PathBuf::from("b.py")),
            ..base_args()
        };
        let mut config = sample_config(None);
        config.run.stages = vec![perfscale_core::step::VuStage {
            duration: "30s".into(),
            target: 10,
        }];
        let err = resolve_plan(
            &args,
            None,
            Some(&config),
            &[],
            &serde_json::Map::new(),
            &[],
            &serde_json::Map::new(),
        )
        .err()
        .expect("stages config must be rejected for --locust");
        assert!(err.to_string().contains("native engine"), "{err}");
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
        assert_eq!(export.meta.perfscale_version, crate::version::VERSION);
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
    fn build_export_picks_up_thresholds_line() {
        let lines = vec![
            "http_reqs..............: 120 2.00/s".to_string(),
            r#"thresholds: {"status":"fail","message":"db_errors count=3 ≠ 0","violations":[{"metric":"db_errors","expr":"count==0","actual":3.0}]}"#.to_string(),
        ];
        let export = build_export("native", Some(1), Some("1s".into()), &lines);
        let t = export
            .thresholds
            .expect("thresholds parsed from summary lines");
        assert_eq!(t.status, "fail");
        assert_eq!(t.violations.len(), 1);
    }

    #[test]
    fn build_export_picks_up_gpu_line() {
        let lines = vec![
            "http_reqs..............: 120 2.00/s".to_string(),
            "gpu: 1 device, 2 samples every 1000ms (nvidia-smi)".to_string(),
            r#"gpu: {"source":"nvidia-smi","interval_ms":1000,"devices":[{"index":0,"samples":[{"ts_ms":1,"index":0,"utilization_pct":42.0}],"avg_utilization_pct":42.0,"max_utilization_pct":42.0}]}"#.to_string(),
        ];
        let export = build_export("native", Some(1), Some("1s".into()), &lines);
        let g = export.gpu.expect("gpu parsed from summary lines");
        assert_eq!(g.source, "nvidia-smi");
        assert_eq!(g.devices[0].max_utilization_pct, Some(42.0));
    }

    #[test]
    fn is_summary_line_accepts_llm_digest() {
        assert!(is_summary_line(
            "llm....................: reqs=2 1.00/s failed=0 ttft_p50=120ms ttft_p95=120ms"
        ));
        // The per-request step log is not a summary line.
        assert!(!is_summary_line("LLM openai llama3 → 200, 8 chunks"));
    }

    #[test]
    fn is_summary_line_accepts_gpu_lines() {
        assert!(is_summary_line(
            r#"gpu: {"source":"nvidia-smi","interval_ms":1000,"devices":[]}"#
        ));
        assert!(is_summary_line(
            "gpu: 1 device, 2 samples every 1000ms (nvidia-smi)"
        ));
        assert!(is_summary_line(
            "gpu0: util avg=42.0% max=98.0% vram max=12288/24576MiB temp max=61.0C power max=250.4W"
        ));
    }

    #[test]
    fn is_summary_line_accepts_thresholds_lines() {
        assert!(is_summary_line(
            r#"thresholds: {"status":"pass","message":"all thresholds met"}"#
        ));
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
        assert!(is_summary_line(
            "http_req_duration......: avg=1.00ms p(50)=1ms p(90)=1ms p(95)=1ms p(99)=1ms min=1ms max=1ms"
        ));
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
        // Native protocol metrics (gRPC, WebSocket, FIX, …).
        assert!(is_summary_line(
            "grpc_req_duration: avg=12.34ms p(50)=10.00ms count=100"
        ));
        assert!(is_summary_line("grpc_msgs_sent: 100 10.00/s"));
        assert!(is_summary_line("ws_msg_rtt: avg=8.00ms count=50"));
        assert!(is_summary_line("ws_msgs_sent: 50 5.00/s"));
    }

    #[test]
    fn is_summary_line_rejects_per_iteration_log_output() {
        assert!(!is_summary_line("loop-test"));
        assert!(!is_summary_line("GET https://example.com → 200 OK (12ms)"));
        assert!(!is_summary_line("[check] homepage: status==200 → PASS"));
        assert!(!is_summary_line(""));
    }
}
