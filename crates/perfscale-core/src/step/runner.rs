//! Native load runner.
//!
//! Spawns N virtual users (tokio tasks), each running the step list in a loop
//! until the configured duration expires.  Metrics are collected in a shared
//! structure and summarised in a k6-compatible text format so downstream
//! parsers (dashboards, `perfscale serve`) work the same for all three engines.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::runner::{LogLine, LogSource};
use crate::step::{
    actions::{execute_action, HttpSample, LogTag},
    context::Context,
    RunConfig, Step,
};

impl From<LogTag> for LogSource {
    fn from(t: LogTag) -> Self {
        match t {
            LogTag::Out => LogSource::Stdout,
            LogTag::Err => LogSource::Stderr,
            LogTag::Sys => LogSource::System,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared metrics
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Metrics {
    durations: Vec<u64>, // ms per HTTP request
    failures: u64,
    total: u64,
}

impl Metrics {
    fn record(&mut self, s: &HttpSample) {
        self.total += 1;
        if s.failed {
            self.failures += 1;
        }
        self.durations.push(s.duration_ms);
    }

    /// Emit k6-compatible summary lines.
    ///
    /// ```text
    /// http_req_duration: avg=42.00ms p(50)=40ms p(90)=60ms p(95)=68ms p(99)=85ms min=12ms max=120ms
    /// http_req_failed: 0.00%
    /// http_reqs: 120 2.00/s
    /// ```
    fn summary_lines(&self, wall_secs: f64, total_iters: u64, vus: u32) -> Vec<String> {
        let mut lines = Vec::new();

        // Always emit iteration stats (even with no HTTP requests) so
        // downstream parsers can extract metrics from sleep-only runs.
        let iter_rate = total_iters as f64 / wall_secs.max(0.001);
        lines.push(format!("vus....................: {vus} min=1 max={vus}"));
        lines.push(format!(
            "iterations..............: {total_iters} {iter_rate:.2}/s"
        ));

        if self.total == 0 {
            return lines;
        }

        let mut sorted = self.durations.clone();
        sorted.sort_unstable();
        let n = sorted.len();

        let avg = sorted.iter().sum::<u64>() as f64 / n as f64;
        let pct = |p: f64| -> u64 {
            let idx = ((p / 100.0) * n as f64).floor() as usize;
            sorted[idx.min(n - 1)]
        };

        let rps = self.total as f64 / wall_secs.max(0.001);
        let err = self.failures as f64 / self.total as f64 * 100.0;

        lines.extend([
            format!(
                "http_req_duration......: avg={avg:.2}ms p(50)={p50}ms p(90)={p90}ms p(95)={p95}ms p(99)={p99}ms min={min}ms max={max}ms",
                avg = avg,
                p50 = pct(50.0),
                p90 = pct(90.0),
                p95 = pct(95.0),
                p99 = pct(99.0),
                min = sorted[0],
                max = sorted[n - 1],
            ),
            format!("http_req_failed........: {err:.2}%"),
            format!("http_reqs..............: {total} {rps:.2}/s", total = self.total),
        ]);
        lines
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Execute `steps` under `config` load and stream [`LogLine`]s through `tx`.
///
/// Returns once the configured duration has elapsed and all VUs have finished
/// their current iteration.
pub async fn run_steps(steps: Vec<Step>, config: RunConfig, tx: mpsc::Sender<LogLine>) {
    let duration_secs = config.duration_secs();
    let vus = config.vus.max(1);
    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let metrics = Arc::new(Mutex::new(Metrics::default()));
    let iter_count = Arc::new(AtomicU64::new(0));
    let started = Instant::now();

    emit(
        &tx,
        LogSource::System,
        &format!(
            "Starting {vus} VU{} for {} ({duration_secs}s)",
            if vus == 1 { "" } else { "s" },
            config.duration
        ),
    )
    .await;

    let steps = Arc::new(steps);
    let mut handles = Vec::with_capacity(vus as usize);

    for vu_id in 1..=vus {
        let steps_ref = Arc::clone(&steps);
        let metrics = Arc::clone(&metrics);
        let iter_count = Arc::clone(&iter_count);
        let tx = tx.clone();

        handles.push(tokio::spawn(async move {
            let mut ctx = Context::new();

            while Instant::now() < deadline {
                iter_count.fetch_add(1, Ordering::Relaxed);
                for step in steps_ref.iter() {
                    execute_step(step, &mut ctx, &tx, &metrics, vu_id).await;
                    if Instant::now() >= deadline {
                        break;
                    }
                }
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let wall_secs = started.elapsed().as_secs_f64();
    let total_iters = iter_count.load(Ordering::Relaxed);
    let lines = metrics
        .lock()
        .unwrap()
        .summary_lines(wall_secs, total_iters, vus);
    for line in &lines {
        emit(&tx, LogSource::Stdout, line).await;
    }
    emit(
        &tx,
        LogSource::System,
        &format!("Done — {wall_secs:.1}s wall clock"),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Per-step execution
// ---------------------------------------------------------------------------

async fn execute_step(
    step: &Step,
    ctx: &mut Context,
    tx: &mpsc::Sender<LogLine>,
    metrics: &Arc<Mutex<Metrics>>,
    _vu_id: u32,
) {
    let action = &step.action;
    let step_name = step.name.as_deref().unwrap_or(action.as_str());
    let empty = Value::Object(Default::default());
    let params = step.with.as_ref().unwrap_or(&empty);

    let output = execute_action(action, params, ctx, step_name).await;

    // Collect HTTP timing
    if let Some(ref sample) = output.http_sample {
        metrics.lock().unwrap().record(sample);
    }

    // Stream log lines
    for (tag, text) in &output.logs {
        emit(tx, LogSource::from(*tag), text).await;
    }

    // Store output for later interpolation / checks
    if let Some(ref name) = step.outputs {
        ctx.set(name, output.value.clone());
    }
    // Always store as __last__ for inline checks
    ctx.set("__last__", output.value.clone());

    // Inline checks (step.check field)
    if let Some(checks) = &step.check {
        let check_out = execute_action("std/check@v1", checks, ctx, step_name).await;
        for (tag, text) in &check_out.logs {
            emit(tx, LogSource::from(*tag), text).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

async fn emit(tx: &mpsc::Sender<LogLine>, source: LogSource, text: &str) {
    let _ = tx
        .send(LogLine {
            source,
            text: text.to_string(),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn sleep_step(ms: u64) -> Step {
        Step {
            name: None,
            action: "std/sleep@v1".into(),
            with: Some(json!({ "ms": ms })),
            check: None,
            outputs: None,
        }
    }

    /// Run `run_steps` in the background and drain its channel concurrently.
    ///
    /// The channel is bounded (512), and a busy loop can easily emit more
    /// lines than that within a 1s test run — draining only *after* awaiting
    /// `run_steps` to completion would deadlock (producer blocks on a full
    /// channel with nobody consuming). `runner::execute` avoids this the same
    /// way: spawn the producer, consume from the caller.
    async fn run_and_collect(steps: Vec<Step>, config: RunConfig) -> Vec<LogLine> {
        let (tx, mut rx) = mpsc::channel(512);
        let handle = tokio::spawn(run_steps(steps, config, tx));

        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        handle.await.unwrap();
        lines
    }

    #[tokio::test]
    async fn run_steps_sleep_only_emits_start_and_done_markers() {
        let config = RunConfig {
            vus: 1,
            duration: "1s".into(),
        };
        let lines = run_and_collect(vec![sleep_step(10)], config).await;

        assert!(lines.first().unwrap().text.starts_with("Starting 1 VU"));
        assert!(lines.last().unwrap().text.starts_with("Done"));
        assert!(lines.iter().any(|l| l.text.starts_with("vus")));
        assert!(lines.iter().any(|l| l.text.starts_with("iterations")));
    }

    #[tokio::test]
    async fn run_steps_records_http_metrics() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let steps = vec![
            Step {
                name: Some("hit".into()),
                action: "std/http@v1".into(),
                with: Some(json!({ "url": format!("{}/ok", server.uri()) })),
                check: None,
                outputs: None,
            },
            // Throttle the loop so a 1s run makes a handful of requests, not
            // thousands — the suite runs many tests in parallel.
            sleep_step(50),
        ];
        let config = RunConfig {
            vus: 1,
            duration: "1s".into(),
        };
        let lines = run_and_collect(steps, config).await;

        // The exact error rate is deliberately not asserted: under full-suite
        // load a single loopback request can spuriously fail. What matters is
        // that HTTP timing was recorded and summarised.
        assert!(lines
            .iter()
            .any(|l| l.text.starts_with("http_req_duration")));
        assert!(lines.iter().any(|l| l.text.starts_with("http_req_failed")));
        assert!(lines.iter().any(|l| l.text.starts_with("http_reqs")));
    }

    #[tokio::test]
    async fn run_steps_inline_check_failure_streams_as_stderr() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/fail"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let steps = vec![Step {
            name: Some("hit".into()),
            action: "std/http@v1".into(),
            with: Some(json!({ "url": format!("{}/fail", server.uri()) })),
            check: Some(json!({ "status": 200 })),
            outputs: None,
        }];
        let config = RunConfig {
            vus: 1,
            duration: "1s".into(),
        };
        let lines = run_and_collect(steps, config).await;

        let check_line = lines
            .iter()
            .find(|l| l.text.contains("[check]"))
            .expect("check log line present");
        assert_eq!(check_line.source, LogSource::Stderr);
        assert!(check_line.text.contains("FAIL"));
    }

    #[tokio::test]
    async fn run_steps_multiple_vus_reports_correct_count() {
        let config = RunConfig {
            vus: 3,
            duration: "1s".into(),
        };
        let lines = run_and_collect(vec![sleep_step(5)], config).await;
        assert!(lines
            .iter()
            .any(|l| l.text == "vus....................: 3 min=1 max=3"));
    }

    #[tokio::test]
    async fn run_steps_propagates_outputs_between_steps() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let steps = vec![
            Step {
                name: Some("fetch".into()),
                action: "std/http@v1".into(),
                with: Some(json!({ "url": format!("{}/data", server.uri()) })),
                check: None,
                outputs: Some("resp".into()),
            },
            Step {
                name: Some("report".into()),
                action: "std/log@v1".into(),
                with: Some(json!({ "message": "status was ${{ resp.status }}" })),
                check: None,
                outputs: None,
            },
        ];
        let config = RunConfig {
            vus: 1,
            duration: "1s".into(),
        };
        let lines = run_and_collect(steps, config).await;
        assert!(lines.iter().any(|l| l.text == "status was 200"));
    }

    #[tokio::test]
    async fn run_steps_zero_vus_is_clamped_to_one() {
        let config = RunConfig {
            vus: 0,
            duration: "1s".into(),
        };
        let lines = run_and_collect(vec![sleep_step(5)], config).await;
        assert!(lines.iter().any(|l| l.text.starts_with("Starting 1 VU")));
    }
}
