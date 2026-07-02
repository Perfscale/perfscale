//! `perfscale bench` — compare perfscale's engines against the same tools
//! invoked bare, and produce a markdown report.
//!
//! Five scenarios, run sequentially so they never compete for CPU:
//!
//! - `locust-native` / `k6-native` — the tool's own binary invoked directly,
//!   no perfscale involved. The baseline.
//! - `perfscale-k6` / `perfscale-locust` — the same binaries invoked through
//!   `perfscale run --k6` / `--locust`. Compared against the native baseline,
//!   this is perfscale's wrapping overhead (temp files, log piping, summary
//!   translation) — not the tool's own performance.
//! - `perfscale-yaml` — perfscale's own step engine, no external binary.
//!
//! To keep the comparison about *engine overhead* rather than network or
//! target variance, every scenario hits the same in-process HTTP target
//! (axum, `GET /` → 200 "ok").

use std::fmt::Write as _;

use axum::{routing::get, Router};
use perfscale_core::runner::locust::LocustOpts;
use perfscale_core::runner::{self, ExecutionPlan};
use perfscale_core::step::{parse_duration_secs, RunConfig, Step, TestDef};

use crate::cli::BenchArgs;
use crate::error::CliError;

pub async fn bench(args: BenchArgs) -> Result<(), CliError> {
    let vus = args.vus.max(1);
    let duration = args.duration.clone();
    // Validate early so a typo fails before we spend a whole benchmark on it.
    if parse_duration_secs(&duration) == 1 && duration != "1s" && duration != "1" {
        return Err(CliError::new(format!("invalid --duration '{duration}'"))
            .hint("use forms like \"15s\", \"1m\", \"1m30s\"")
            .docs("yaml-reference.md#config--c-configyaml"));
    }

    let target = TargetServer::start().await?;
    eprintln!("[bench] target listening on {}", target.url);

    let mut results: Vec<EngineResult> = Vec::new();

    for engine in &args.engines {
        let result = match engine.as_str() {
            "locust-native" => run_locust_native(&target.url, vus, &duration).await,
            "k6-native" => run_k6_native(&target.url, vus, &duration).await,
            "perfscale-k6" => run_perfscale_k6(&target.url, vus, &duration).await,
            "perfscale-locust" => run_perfscale_locust(&target.url, vus, &duration).await,
            "perfscale-yaml" => run_perfscale_yaml(&target.url, vus, &duration).await,
            other => {
                return Err(CliError::new(format!("unknown engine '{other}'")).hint(
                    "valid engines: locust-native, k6-native, perfscale-k6, perfscale-locust, perfscale-yaml (comma-separated)",
                ).docs("cli/commands.md#perfscale-bench"));
            }
        };
        eprintln!("[bench] {engine}: {}", result.status_line());
        results.push(result);
    }

    target.shutdown();

    let report = render_report(&ReportInput {
        vus,
        duration: duration.clone(),
        target_url: target.url.clone(),
        env: collect_env_info(),
        software: collect_software_versions(),
        results,
    });

    println!("{report}");

    if let Some(path) = &args.output {
        std::fs::write(path, &report).map_err(|e| {
            CliError::new(format!("failed to write report to '{}'", path.display()))
                .cause(e.to_string())
        })?;
        eprintln!("[bench] report written to {}", path.display());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Target server
// ---------------------------------------------------------------------------

struct TargetServer {
    url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl TargetServer {
    async fn start() -> Result<Self, CliError> {
        let app = Router::new().route("/", get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| {
                CliError::new("failed to start bench target server").cause(e.to_string())
            })?;
        let addr = listener.local_addr().map_err(|e| {
            CliError::new("failed to read bench target address").cause(e.to_string())
        })?;

        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Ok(Self {
            url: format!("http://{addr}"),
            handle,
        })
    }

    fn shutdown(&self) {
        self.handle.abort();
    }
}

// ---------------------------------------------------------------------------
// Engine runs
// ---------------------------------------------------------------------------

pub struct EngineResult {
    pub engine: String,
    /// Short version string of the software that actually ran (perfscale's
    /// own version for `perfscale-yaml`, k6's/locust's for the rest) — shown
    /// next to the numbers so a report is self-contained without having to
    /// cross-reference the Software section above it.
    pub version: Option<String>,
    pub outcome: EngineOutcome,
}

pub enum EngineOutcome {
    Completed(EngineMetrics),
    Skipped(String),
}

impl EngineResult {
    fn status_line(&self) -> String {
        match &self.outcome {
            EngineOutcome::Completed(m) => match m.rps {
                Some(rps) => format!("done, {rps:.0} req/s"),
                None => "done (no http metrics found)".into(),
            },
            EngineOutcome::Skipped(why) => format!("skipped — {why}"),
        }
    }
}

/// Standalone locustfile shared by both the native-locust baseline and the
/// perfscale-wrapped run, so the two measure the exact same scenario.
const BENCH_LOCUSTFILE: &str = "from locust import HttpUser, task\n\
     class BenchUser(HttpUser):\n    \
     @task\n    \
     def hit(self):\n        \
     self.client.get('/')\n";

/// A non-zero exit is only a crash if it produced no metrics at all — k6
/// (failed thresholds) and locust (failed requests) both exit non-zero as
/// normal test feedback once they've actually run.
fn crash_check(code: Option<i32>, lines: &[String]) -> Result<(), String> {
    if let Some(code) = code {
        if code != 0 && !lines.iter().any(|l| l.trim_start().starts_with("http_req")) {
            return Err(format!(
                "engine exited with code {code} before producing results"
            ));
        }
    }
    Ok(())
}

async fn collect_lines(plan: ExecutionPlan) -> Result<Vec<String>, String> {
    let output = runner::execute(plan).await?;
    drain(output).await
}

/// Drain a perfscale-wrapped run to completion, applying [`crash_check`].
async fn drain(output: perfscale_core::runner::RunOutput) -> Result<Vec<String>, String> {
    let perfscale_core::runner::RunOutput { mut lines, exit } = output;
    let mut collected = Vec::new();
    while let Some(line) = lines.recv().await {
        collected.push(line.text);
    }
    crash_check(exit.await.ok().flatten(), &collected)?;
    Ok(collected)
}

/// Run `cmd` to completion (no live streaming — the native baselines don't
/// need it) and return its combined stdout+stderr lines plus exit code.
async fn capture_bare(
    mut cmd: tokio::process::Command,
) -> Result<(Vec<String>, Option<i32>), String> {
    let output = cmd.output().await.map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    lines.extend(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .map(str::to_string),
    );
    Ok((lines, output.status.code()))
}

fn skipped(engine: &str, why: impl Into<String>) -> EngineResult {
    EngineResult {
        engine: engine.into(),
        version: None,
        outcome: EngineOutcome::Skipped(why.into()),
    }
}

fn completed(engine: &str, version: Option<String>, lines: &[String]) -> EngineResult {
    EngineResult {
        engine: engine.into(),
        version,
        outcome: EngineOutcome::Completed(parse_summary(lines)),
    }
}

/// Trim a `<bin> --version`-style banner down to `"name x.y.z"` — k6 and
/// locust both tack on build/interpreter details we don't need in a table
/// cell (they're still visible in full in the Software section above).
fn short_version(full: &str) -> String {
    let mut s = full;
    if let Some(idx) = s.find(" from ") {
        s = &s[..idx];
    }
    if let Some(idx) = s.find(" (") {
        s = &s[..idx];
    }
    s.trim().to_string()
}

// ---------------------------------------------------------------------------
// perfscale-yaml — the native step engine (no external binary)
// ---------------------------------------------------------------------------

async fn run_perfscale_yaml(target_url: &str, vus: u32, duration: &str) -> EngineResult {
    const NAME: &str = "perfscale (yaml)";
    let test = TestDef {
        steps: vec![Step {
            name: Some("get".into()),
            action: "std/http@v1".into(),
            with: Some(serde_json::json!({ "method": "GET", "url": format!("{target_url}/") })),
            check: None,
            outputs: None,
        }],
    };
    let config = RunConfig {
        vus,
        duration: duration.to_string(),
    };

    match collect_lines(ExecutionPlan::NativeSteps { test, config }).await {
        Ok(lines) => completed(NAME, Some(env!("CARGO_PKG_VERSION").to_string()), &lines),
        Err(e) => skipped(NAME, e),
    }
}

// ---------------------------------------------------------------------------
// k6: bare binary vs perfscale run --k6 (identical generated script)
// ---------------------------------------------------------------------------

fn k6_bench_script(target_url: &str, vus: u32, duration: &str) -> String {
    format!(
        "import http from 'k6/http';\n\
         export const options = {{ vus: {vus}, duration: '{duration}' }};\n\
         export default function () {{ http.get('{target_url}/'); }}\n"
    )
}

async fn run_k6_native(target_url: &str, vus: u32, duration: &str) -> EngineResult {
    const NAME: &str = "k6 (native)";
    let Some(version) = binary_version("k6", &["version"]) else {
        return skipped(NAME, "k6 not installed");
    };

    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => return skipped(NAME, e.to_string()),
    };
    let script_path = dir.path().join("bench.js");
    if let Err(e) = std::fs::write(&script_path, k6_bench_script(target_url, vus, duration)) {
        return skipped(NAME, e.to_string());
    }

    let mut cmd = tokio::process::Command::new("k6");
    cmd.arg("run").arg("--no-color").arg(&script_path);

    match capture_bare(cmd).await {
        Ok((lines, code)) => match crash_check(code, &lines) {
            Ok(()) => completed(NAME, Some(short_version(&version)), &lines),
            Err(e) => skipped(NAME, e),
        },
        Err(e) => skipped(NAME, e),
    }
}

async fn run_perfscale_k6(target_url: &str, vus: u32, duration: &str) -> EngineResult {
    const NAME: &str = "perfscale (k6)";
    let Some(version) = binary_version("k6", &["version"]) else {
        return skipped(NAME, "k6 not installed");
    };

    let script = k6_bench_script(target_url, vus, duration);
    let result = async {
        let output = perfscale_core::runner::k6::run_streaming(script).await?;
        drain(output).await
    };

    match result.await {
        Ok(lines) => completed(NAME, Some(short_version(&version)), &lines),
        Err(e) => skipped(NAME, e),
    }
}

// ---------------------------------------------------------------------------
// locust: bare binary vs perfscale run --locust (identical locustfile)
// ---------------------------------------------------------------------------

async fn run_locust_native(target_url: &str, vus: u32, duration: &str) -> EngineResult {
    const NAME: &str = "locust (native)";
    let Some(version) = binary_version("locust", &["--version"]) else {
        return skipped(NAME, "locust not installed");
    };

    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => return skipped(NAME, e.to_string()),
    };
    let script_path = dir.path().join("locustfile.py");
    if let Err(e) = std::fs::write(&script_path, BENCH_LOCUSTFILE) {
        return skipped(NAME, e.to_string());
    }
    let csv_prefix = dir.path().join("stats");

    let mut cmd = tokio::process::Command::new("locust");
    cmd.arg("-f")
        .arg(&script_path)
        .arg("--headless")
        .arg("-u")
        .arg(vus.to_string())
        .arg("-r")
        .arg(vus.to_string())
        .arg("-t")
        .arg(duration)
        .arg("--host")
        .arg(target_url)
        .arg("--csv")
        .arg(&csv_prefix);

    let (mut lines, code) = match capture_bare(cmd).await {
        Ok(v) => v,
        Err(e) => return skipped(NAME, e),
    };

    // Reuse the exact CSV parser perfscale's own locust runner uses, so the
    // two rows are computed identically and only wrapper overhead differs.
    match perfscale_core::runner::locust::parse_csv_summary(&csv_prefix).await {
        Ok(summary) => lines.extend(summary),
        Err(e) => lines.push(format!("[bench] failed to read locust stats: {e}")),
    }

    match crash_check(code, &lines) {
        Ok(()) => completed(NAME, Some(short_version(&version)), &lines),
        Err(e) => skipped(NAME, e),
    }
}

async fn run_perfscale_locust(target_url: &str, vus: u32, duration: &str) -> EngineResult {
    const NAME: &str = "perfscale (locust)";
    let Some(version) = binary_version("locust", &["--version"]) else {
        return skipped(NAME, "locust not installed");
    };

    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => return skipped(NAME, e.to_string()),
    };
    let path = dir.path().join("locustfile.py");
    if let Err(e) = std::fs::write(&path, BENCH_LOCUSTFILE) {
        return skipped(NAME, e.to_string());
    }

    let opts = LocustOpts {
        users: vus,
        spawn_rate: vus,
        duration: duration.to_string(),
        host: Some(target_url.to_string()),
    };

    match collect_lines(ExecutionPlan::LocustScript { path, opts }).await {
        Ok(lines) => completed(NAME, Some(short_version(&version)), &lines),
        Err(e) => skipped(NAME, e),
    }
}

// ---------------------------------------------------------------------------
// Summary parsing (works on the k6-compatible block all engines emit)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq)]
pub struct EngineMetrics {
    pub requests: Option<f64>,
    pub rps: Option<f64>,
    pub avg_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p90_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub max_ms: Option<f64>,
    pub failed_pct: Option<f64>,
}

pub fn parse_summary(lines: &[String]) -> EngineMetrics {
    let mut m = EngineMetrics::default();

    for raw in lines {
        let line = raw.trim();
        if let Some(rest) = metric_value(line, "http_reqs") {
            let mut parts = rest.split_whitespace();
            m.requests = parts.next().and_then(|v| v.parse().ok());
            m.rps = parts
                .next()
                .and_then(|v| v.trim_end_matches("/s").parse().ok());
        } else if let Some(rest) = metric_value(line, "http_req_duration") {
            for token in rest.split_whitespace() {
                let Some((key, value)) = token.split_once('=') else {
                    continue;
                };
                let ms = parse_duration_ms(value);
                match key {
                    "avg" => m.avg_ms = ms,
                    "med" | "p(50)" => m.p50_ms = ms,
                    "p(90)" => m.p90_ms = ms,
                    "p(95)" => m.p95_ms = ms,
                    "max" => m.max_ms = ms,
                    _ => {}
                }
            }
        } else if let Some(rest) = metric_value(line, "http_req_failed") {
            m.failed_pct = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.trim_end_matches('%').parse().ok());
        }
    }
    m
}

/// If `line` is `<name>....: <rest>` (dot padding optional), return `<rest>`.
/// Uses an exact prefix match on the metric name so `http_reqs` does not
/// swallow `http_req_duration`.
fn metric_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(name)?;
    let rest = rest.trim_start_matches('.');
    let rest = rest.strip_prefix(':')?;
    Some(rest.trim())
}

/// Parse `"572.74ms"`, `"1.34s"`, `"890µs"`, `"120"` (already ms) into ms.
fn parse_duration_ms(value: &str) -> Option<f64> {
    if let Some(v) = value.strip_suffix("ms") {
        v.parse().ok()
    } else if let Some(v) = value.strip_suffix("µs") {
        v.parse::<f64>().ok().map(|n| n / 1000.0)
    } else if let Some(v) = value.strip_suffix('s') {
        v.parse::<f64>().ok().map(|n| n * 1000.0)
    } else {
        value.parse().ok()
    }
}

// ---------------------------------------------------------------------------
// Environment / software info
// ---------------------------------------------------------------------------

pub struct EnvInfo {
    pub os: String,
    pub arch: String,
    pub cpu: String,
    pub threads: usize,
    pub ram: String,
    pub swap: String,
}

fn collect_env_info() -> EnvInfo {
    use sysinfo::System;

    let mut sys = System::new_all();
    sys.refresh_all();

    let cpu = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    EnvInfo {
        os: System::long_os_version().unwrap_or_else(|| "unknown".into()),
        arch: System::cpu_arch(),
        cpu,
        threads: sys.cpus().len(),
        ram: format_bytes(sys.total_memory()),
        swap: format_bytes(sys.total_swap()),
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.0} MiB", b / MIB)
    } else {
        format!("{bytes} B")
    }
}

pub struct SoftwareVersions {
    pub perfscale: String,
    pub k6: Option<String>,
    pub locust: Option<String>,
}

fn collect_software_versions() -> SoftwareVersions {
    SoftwareVersions {
        perfscale: env!("CARGO_PKG_VERSION").to_string(),
        k6: binary_version("k6", &["version"]),
        locust: binary_version("locust", &["--version"]),
    }
}

/// First line of `<bin> <args>` output, or None if the binary is missing.
fn binary_version(bin: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(bin).args(args).output().ok()?;
    let text = if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    String::from_utf8_lossy(&text)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
}

// ---------------------------------------------------------------------------
// Report rendering
// ---------------------------------------------------------------------------

pub struct ReportInput {
    pub vus: u32,
    pub duration: String,
    pub target_url: String,
    pub env: EnvInfo,
    pub software: SoftwareVersions,
    pub results: Vec<EngineResult>,
}

pub fn render_report(input: &ReportInput) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "# perfscale bench report\n");
    let _ = writeln!(
        out,
        "Workload: `GET {}/` — {} VUs for {} per engine, engines run sequentially against an in-process HTTP target.\n",
        input.target_url, input.vus, input.duration
    );

    let _ = writeln!(out, "## Environment\n");
    let _ = writeln!(out, "| | |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| OS | {} ({}) |", input.env.os, input.env.arch);
    let _ = writeln!(out, "| CPU | {} |", input.env.cpu);
    let _ = writeln!(out, "| Threads | {} |", input.env.threads);
    let _ = writeln!(out, "| RAM | {} |", input.env.ram);
    let _ = writeln!(out, "| Swap | {} |", input.env.swap);
    let _ = writeln!(out);

    let _ = writeln!(out, "## Software\n");
    let _ = writeln!(out, "| | |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| perfscale | {} |", input.software.perfscale);
    let _ = writeln!(
        out,
        "| k6 | {} |",
        input.software.k6.as_deref().unwrap_or("not installed")
    );
    let _ = writeln!(
        out,
        "| locust | {} |",
        input.software.locust.as_deref().unwrap_or("not installed")
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "## Results\n");
    let _ = writeln!(
        out,
        "| Engine | Version | Requests | RPS | avg | p50 | p90 | p95 | max | Failed |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|---|");
    for r in &input.results {
        let version = r.version.as_deref().unwrap_or("—");
        match &r.outcome {
            EngineOutcome::Completed(m) => {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    r.engine,
                    version,
                    fmt_count(m.requests),
                    fmt_num(m.rps, "/s"),
                    fmt_num(m.avg_ms, "ms"),
                    fmt_num(m.p50_ms, "ms"),
                    fmt_num(m.p90_ms, "ms"),
                    fmt_num(m.p95_ms, "ms"),
                    fmt_num(m.max_ms, "ms"),
                    fmt_num(m.failed_pct, "%"),
                );
            }
            EngineOutcome::Skipped(why) => {
                let _ = writeln!(
                    out,
                    "| {} | {} | — | — | — | — | — | — | — | {why} |",
                    r.engine, version
                );
            }
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Notes: numbers measure engine overhead against a trivial local target, not real-world \
         throughput. Compare `*-native` rows against their `perfscale-*` counterpart to see \
         perfscale's wrapping overhead — both run the identical generated script/locustfile. \
         `k6` and `perfscale (yaml)` hit the target in a tight loop; locust adds its own default \
         wait model only if the locustfile defines one (the bench locustfile does not)."
    );

    out
}

fn fmt_num(v: Option<f64>, unit: &str) -> String {
    match v {
        Some(v) => format!("{v:.2}{unit}"),
        None => "—".into(),
    }
}

fn fmt_count(v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{v:.0}"),
        None => "—".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_summary_native_style() {
        let m = parse_summary(&lines(&[
            "vus....................: 5 min=1 max=5",
            "iterations..............: 142 4.73/s",
            "http_req_duration......: avg=213.40ms p(50)=201ms p(90)=280ms p(95)=310ms p(99)=352ms min=180ms max=390ms",
            "http_req_failed........: 0.00%",
            "http_reqs..............: 142 4.73/s",
        ]));
        assert_eq!(m.requests, Some(142.0));
        assert_eq!(m.rps, Some(4.73));
        assert_eq!(m.avg_ms, Some(213.40));
        assert_eq!(m.p50_ms, Some(201.0));
        assert_eq!(m.p90_ms, Some(280.0));
        assert_eq!(m.p95_ms, Some(310.0));
        assert_eq!(m.max_ms, Some(390.0));
        assert_eq!(m.failed_pct, Some(0.0));
    }

    #[test]
    fn parse_summary_k6_style() {
        let m = parse_summary(&lines(&[
            "    http_req_duration..............: avg=572.74ms min=572.74ms med=572.74ms max=1.34s p(90)=572.74ms p(95)=572.74ms",
            "    http_req_failed................: 0.00%  0 out of 1",
            "    http_reqs......................: 1      0.744818/s",
        ]));
        assert_eq!(m.requests, Some(1.0));
        assert_eq!(m.rps, Some(0.744818));
        assert_eq!(m.avg_ms, Some(572.74));
        assert_eq!(m.p50_ms, Some(572.74)); // med= maps to p50
        assert_eq!(m.max_ms, Some(1340.0)); // 1.34s → ms
        assert_eq!(m.failed_pct, Some(0.0));
    }

    #[test]
    fn parse_summary_handles_microseconds() {
        let m = parse_summary(&lines(&[
            "http_req_duration......: avg=890µs p(95)=1.2ms max=2ms",
        ]));
        assert_eq!(m.avg_ms, Some(0.89));
        assert_eq!(m.p95_ms, Some(1.2));
    }

    #[test]
    fn parse_summary_ignores_unrelated_lines_and_missing_metrics() {
        let m = parse_summary(&lines(&[
            "hello world",
            "iterations..............: 10 1.00/s",
        ]));
        assert_eq!(m, EngineMetrics::default());
    }

    #[test]
    fn metric_value_does_not_confuse_http_reqs_with_http_req_duration() {
        // "http_req_duration" starts with "http_req" but its padding is dots,
        // so stripping the shorter name must not yield a value.
        assert!(metric_value("http_req_duration......: avg=1ms", "http_reqs").is_none());
        assert_eq!(
            metric_value("http_reqs..............: 5 1.00/s", "http_reqs"),
            Some("5 1.00/s")
        );
    }

    #[test]
    fn parse_duration_ms_units() {
        assert_eq!(parse_duration_ms("572.74ms"), Some(572.74));
        assert_eq!(parse_duration_ms("1.34s"), Some(1340.0));
        assert_eq!(parse_duration_ms("890µs"), Some(0.89));
        assert_eq!(parse_duration_ms("42"), Some(42.0));
        assert_eq!(parse_duration_ms("n/a"), None);
    }

    #[test]
    fn format_bytes_scales() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(8 * 1024 * 1024), "8 MiB");
        assert_eq!(format_bytes(16 * 1024 * 1024 * 1024), "16.0 GiB");
    }

    #[test]
    fn short_version_trims_k6_build_details() {
        assert_eq!(
            short_version("k6 v1.5.0 (commit/devel, go1.25.5, darwin/arm64)"),
            "k6 v1.5.0"
        );
    }

    #[test]
    fn short_version_trims_locust_interpreter_path() {
        assert_eq!(
            short_version(
                "locust 2.44.4 from /Library/Frameworks/Python.framework/Versions/3.12/lib/python3.12/site-packages/locust (Python 3.12.3)"
            ),
            "locust 2.44.4"
        );
    }

    #[test]
    fn short_version_leaves_plain_strings_unchanged() {
        assert_eq!(short_version("k6 v1.5.0"), "k6 v1.5.0");
    }

    #[test]
    fn render_report_contains_required_sections() {
        let input = ReportInput {
            vus: 10,
            duration: "15s".into(),
            target_url: "http://127.0.0.1:9999".into(),
            env: EnvInfo {
                os: "macOS 15.5".into(),
                arch: "arm64".into(),
                cpu: "Apple M3 Pro".into(),
                threads: 12,
                ram: "36.0 GiB".into(),
                swap: "2.0 GiB".into(),
            },
            software: SoftwareVersions {
                perfscale: "0.1.0".into(),
                k6: Some("k6 v1.0.0".into()),
                locust: None,
            },
            results: vec![
                EngineResult {
                    engine: "perfscale (yaml)".into(),
                    version: Some("0.1.0".into()),
                    outcome: EngineOutcome::Completed(EngineMetrics {
                        requests: Some(1000.0),
                        rps: Some(66.6),
                        avg_ms: Some(1.5),
                        p50_ms: Some(1.0),
                        p90_ms: Some(2.0),
                        p95_ms: Some(3.0),
                        max_ms: Some(9.0),
                        failed_pct: Some(0.0),
                    }),
                },
                EngineResult {
                    engine: "locust (native)".into(),
                    version: None,
                    outcome: EngineOutcome::Skipped("locust not installed".into()),
                },
            ],
        };
        let report = render_report(&input);

        for required in [
            "## Environment",
            "## Software",
            "## Results",
            "| OS | macOS 15.5 (arm64) |",
            "| CPU | Apple M3 Pro |",
            "| Threads | 12 |",
            "| RAM | 36.0 GiB |",
            "| Swap | 2.0 GiB |",
            "| perfscale | 0.1.0 |",
            "| k6 | k6 v1.0.0 |",
            "| locust | not installed |",
            "| perfscale (yaml) | 0.1.0 | 1000 | 66.60/s |",
            "| locust (native) | — | — | — | — | — | — | — | — | locust not installed |",
        ] {
            assert!(
                report.contains(required),
                "missing {required:?} in:\n{report}"
            );
        }
    }
}
