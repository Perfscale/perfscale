//! Native load runner.
//!
//! Spawns N virtual users (tokio tasks), each running the step list in a loop
//! until the configured duration expires.  Metrics are collected in a shared
//! structure and summarised in a k6-compatible text format so downstream
//! parsers (dashboards, `perfscale serve`) work the same for all three engines.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
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

/// Durations are tracked in microseconds: 1µs floor keeps sub-millisecond
/// loopback requests distinguishable, the 1-hour ceiling is far beyond any
/// sane single request.
const HIST_LOW_MICROS: u64 = 1;
const HIST_HIGH_MICROS: u64 = 3_600_000_000;
/// Two significant digits → quantiles within ≤1% of the true value.
const HIST_SIGFIGS: u8 = 2;

/// Per-run HTTP metrics accumulator.
///
/// Durations live in a fixed-size HDR histogram (~tens of KB) instead of one
/// f64 per request: storing raw samples made memory grow 8 bytes per request
/// — a 30-hour soak at 10k RPS would have needed ~26 GB at the final
/// clone-and-sort. The histogram trades that for a ≤1% quantile error,
/// invisible at the 2-decimal precision the summary prints.
///
/// Public only so `benches/` can exercise the hot paths (`record`, quantile
/// computation in `summary_lines`) — not part of the supported API surface.
#[doc(hidden)]
#[derive(Debug)]
pub struct Metrics {
    durations_micros: hdrhistogram::Histogram<u64>,
    failures: u64,
    total: u64,
    /// Custom named counters contributed by actions via the reserved
    /// `metrics` key of their output value — e.g. `pro/fix@v1` emits
    /// `fix_messages_sent`. Summed across VUs/iterations, then reported as
    /// `<name>: <total> <rate>/s` so the same downstream parser handles them.
    counters: std::collections::BTreeMap<String, f64>,
    /// Custom named histograms: an action reports duration samples (ms) as a
    /// JSON *array* under the same reserved `metrics` key — e.g. `std/ws@v1`
    /// emits `ws_msg_rtt: [12.3, 15.1]`. Aggregated with the same HDR
    /// settings as the request-duration histogram and summarised in the same
    /// `avg/p(..)/min/max` shape, plus a sample count.
    hists: std::collections::BTreeMap<String, hdrhistogram::Histogram<u64>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            durations_micros: hdrhistogram::Histogram::new_with_bounds(
                HIST_LOW_MICROS,
                HIST_HIGH_MICROS,
                HIST_SIGFIGS,
            )
            .expect("static histogram bounds are valid"),
            failures: 0,
            total: 0,
            counters: std::collections::BTreeMap::new(),
            hists: std::collections::BTreeMap::new(),
        }
    }
}

impl Metrics {
    pub fn record(&mut self, s: &HttpSample) {
        self.total += 1;
        if s.failed {
            self.failures += 1;
        }
        let micros = (s.duration_ms * 1000.0).round() as u64;
        // Clamped into bounds, so the record cannot fail.
        let _ = self
            .durations_micros
            .record(micros.clamp(HIST_LOW_MICROS, HIST_HIGH_MICROS));
    }

    /// Fold a step's custom `metrics` object into the run aggregates. A
    /// numeric value increments a counter; an array of numbers records each
    /// element as a histogram sample in milliseconds. Anything else is
    /// ignored.
    pub fn add_counters(&mut self, obj: &serde_json::Map<String, Value>) {
        for (name, v) in obj {
            match v {
                Value::Array(samples) => {
                    let h = self.hists.entry(name.clone()).or_insert_with(|| {
                        hdrhistogram::Histogram::new_with_bounds(
                            HIST_LOW_MICROS,
                            HIST_HIGH_MICROS,
                            HIST_SIGFIGS,
                        )
                        .expect("static histogram bounds are valid")
                    });
                    for s in samples.iter().filter_map(|s| s.as_f64()) {
                        let micros = (s * 1000.0).round() as u64;
                        let _ = h.record(micros.clamp(HIST_LOW_MICROS, HIST_HIGH_MICROS));
                    }
                }
                _ => {
                    if let Some(x) = v.as_f64() {
                        *self.counters.entry(name.clone()).or_insert(0.0) += x;
                    }
                }
            }
        }
    }

    /// Emit k6-compatible summary lines.
    ///
    /// ```text
    /// http_req_duration: avg=0.42ms p(50)=0.31ms p(90)=0.88ms p(95)=1.02ms p(99)=1.90ms min=0.09ms max=3.10ms
    /// http_req_failed: 0.00%
    /// http_reqs: 120 2.00/s
    /// ```
    pub fn summary_lines(&self, wall_secs: f64, total_iters: u64, vus: u32) -> Vec<String> {
        let mut lines = Vec::new();

        // Always emit iteration stats (even with no HTTP requests) so
        // downstream parsers can extract metrics from sleep-only runs.
        let iter_rate = total_iters as f64 / wall_secs.max(0.001);
        lines.push(format!("vus....................: {vus} min=1 max={vus}"));
        lines.push(format!(
            "iterations..............: {total_iters} {iter_rate:.2}/s"
        ));

        // Custom action counters (e.g. FIX message rates) — emitted whether or
        // not the run made HTTP-style requests.
        for (name, total) in &self.counters {
            let rate = total / wall_secs.max(0.001);
            lines.push(format!("{name}: {total:.0} {rate:.2}/s"));
        }

        // Custom action histograms (e.g. `ws_msg_rtt`) — same shape as
        // `http_req_duration` plus a sample count, so downstream percentile
        // parsers can reuse one grammar.
        for (name, h) in &self.hists {
            let pct = |q: f64| -> f64 { h.value_at_quantile(q) as f64 / 1000.0 };
            lines.push(format!(
                "{name}: avg={avg:.2}ms p(50)={p50:.2}ms p(90)={p90:.2}ms p(95)={p95:.2}ms p(99)={p99:.2}ms min={min:.2}ms max={max:.2}ms count={count}",
                avg = h.mean() / 1000.0,
                p50 = pct(0.50),
                p90 = pct(0.90),
                p95 = pct(0.95),
                p99 = pct(0.99),
                min = h.min() as f64 / 1000.0,
                max = h.max() as f64 / 1000.0,
                count = h.len(),
            ));
        }

        if self.total == 0 {
            return lines;
        }

        let h = &self.durations_micros;
        let pct = |q: f64| -> f64 { h.value_at_quantile(q) as f64 / 1000.0 };

        let rps = self.total as f64 / wall_secs.max(0.001);
        let err = self.failures as f64 / self.total as f64 * 100.0;

        lines.extend([
            format!(
                "http_req_duration......: avg={avg:.2}ms p(50)={p50:.2}ms p(90)={p90:.2}ms p(95)={p95:.2}ms p(99)={p99:.2}ms min={min:.2}ms max={max:.2}ms",
                avg = h.mean() / 1000.0,
                p50 = pct(0.50),
                p90 = pct(0.90),
                p95 = pct(0.95),
                p99 = pct(0.99),
                min = h.min() as f64 / 1000.0,
                max = h.max() as f64 / 1000.0,
            ),
            format!("http_req_failed........: {err:.2}%"),
            format!("http_reqs..............: {total} {rps:.2}/s", total = self.total),
        ]);
        lines
    }

    /// Total requests recorded so far (used by the periodic stats reporter to
    /// compute the per-window throughput).
    pub fn total_requests(&self) -> u64 {
        self.total
    }

    /// One-line machine-readable snapshot for streaming time-series consumers
    /// (the controlplane parses these out of the OTEL log stream).
    ///
    /// `window_reqs`/`window_secs` yield the instantaneous throughput; the
    /// latency percentiles are cumulative since run start (the HDR histogram
    /// is never reset, so they converge instead of jittering).
    ///
    /// ```text
    /// [stats] ts=1720000000000 rps=246.80 err_pct=0.00 p50=1.20 p90=3.40 p95=4.10 p99=8.20 reqs=1234 iters=456
    /// ```
    pub fn stats_line(&self, ts_ms: u64, window_reqs: u64, window_secs: f64, iters: u64) -> String {
        let rps = window_reqs as f64 / window_secs.max(0.001);
        if self.total == 0 {
            return format!("[stats] ts={ts_ms} rps={rps:.2} reqs=0 iters={iters}");
        }
        let h = &self.durations_micros;
        let pct = |q: f64| -> f64 { h.value_at_quantile(q) as f64 / 1000.0 };
        let err = self.failures as f64 / self.total as f64 * 100.0;
        format!(
            "[stats] ts={ts_ms} rps={rps:.2} err_pct={err:.2} p50={p50:.2} p90={p90:.2} p95={p95:.2} p99={p99:.2} reqs={total} iters={iters}",
            p50 = pct(0.50),
            p90 = pct(0.90),
            p95 = pct(0.95),
            p99 = pct(0.99),
            total = self.total,
        )
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Execute `steps` under `config` load and stream [`LogLine`]s through `tx`.
///
/// Returns once the configured duration has elapsed and all VUs have finished
/// their current iteration.
///
/// With `quiet`, per-iteration success output (request lines, sleep markers,
/// passing checks) is dropped at the source — not just filtered at print time
/// — so a busy loop skips the formatting and channel traffic too. Errors,
/// failing checks, and the final metric summary always come through.
///
/// This is the no-setup entry point: equivalent to [`run_native`] with no
/// `before` steps and no static variables. Kept for callers (and tests) that
/// only have a step list and a run config.
pub async fn run_steps(
    steps: Vec<Step>,
    config: RunConfig,
    quiet: bool,
    tx: mpsc::Sender<LogLine>,
) {
    run_native(steps, Vec::new(), config, Map::new(), quiet, tx).await
}

/// Execute a native test with optional one-time `before` setup and static
/// `variables`.
///
/// `before` steps run once, in order, before any VU is spawned. Each step's
/// `outputs` is collected into a `config` object exposed to every test step as
/// `${{ config.<name>.<field> }}`; `variables` is exposed as `${{ vars.* }}`.
/// If any setup step fails, the run aborts before spawning VUs — a broken
/// setup would make every iteration fail identically, so failing fast is
/// clearer than a wall of downstream errors.
pub async fn run_native(
    steps: Vec<Step>,
    before: Vec<Step>,
    config: RunConfig,
    variables: Map<String, Value>,
    quiet: bool,
    tx: mpsc::Sender<LogLine>,
) {
    let vars = if variables.is_empty() {
        Value::Null
    } else {
        Value::Object(variables)
    };

    // --- One-time setup ---
    let config_seed = match run_before(&before, &vars, &config, quiet, &tx).await {
        Ok(v) => v,
        Err(msg) => {
            emit(
                &tx,
                LogSource::Stderr,
                &format!("Setup failed, aborting run: {msg}"),
            )
            .await;
            emit(&tx, LogSource::System, "Done — setup error").await;
            return;
        }
    };

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
    // Shared, immutable across VUs — cloned into each VU's context once.
    let config_seed = Arc::new(config_seed);
    let vars = Arc::new(vars);
    let mut handles = Vec::with_capacity(vus as usize);

    for vu_id in 1..=vus {
        let steps_ref = Arc::clone(&steps);
        let metrics = Arc::clone(&metrics);
        let iter_count = Arc::clone(&iter_count);
        let config_seed = Arc::clone(&config_seed);
        let vars = Arc::clone(&vars);
        let fs_root = config.fs_root.clone();
        let allow_file_actions = config.allow_file_actions;
        let tx = tx.clone();

        handles.push(tokio::spawn(async move {
            let mut ctx = Context::new();
            ctx.allow_file_actions = allow_file_actions;
            ctx.fs_root = fs_root;
            if !config_seed.is_null() {
                ctx.set("config", (*config_seed).clone());
            }
            if !vars.is_null() {
                ctx.set("vars", (*vars).clone());
            }

            while Instant::now() < deadline {
                iter_count.fetch_add(1, Ordering::Relaxed);
                for step in steps_ref.iter() {
                    execute_step(step, &mut ctx, &tx, &metrics, quiet, vu_id).await;
                    if Instant::now() >= deadline {
                        break;
                    }
                }
                // A Live Connection never outlives its iteration: whatever a
                // scenario left open is dropped here (abrupt TCP drop, no
                // Close handshake — `std/ws-close@v1` is the graceful path).
                ctx.resources.drain();
            }
        }));
    }

    // Periodic [stats] reporter: one machine-readable line every 5s while the
    // VUs run, so downstream consumers can chart latency/throughput over time.
    let reporter = {
        let metrics = Arc::clone(&metrics);
        let iter_count = Arc::clone(&iter_count);
        let tx = tx.clone();
        tokio::spawn(async move {
            const INTERVAL_SECS: u64 = 5;
            let mut interval = tokio::time::interval(Duration::from_secs(INTERVAL_SECS));
            interval.tick().await; // consume the immediate first tick
            let mut prev_total: u64 = 0;
            loop {
                interval.tick().await;
                let ts_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let iters = iter_count.load(Ordering::Relaxed);
                let line = {
                    let m = metrics.lock().unwrap();
                    let total = m.total_requests();
                    let line = m.stats_line(ts_ms, total - prev_total, INTERVAL_SECS as f64, iters);
                    prev_total = total;
                    line
                };
                emit(&tx, LogSource::Stdout, &line).await;
            }
        })
    };

    for h in handles {
        let _ = h.await;
    }
    reporter.abort();

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
// One-time setup (`before`)
// ---------------------------------------------------------------------------

/// Run the `before` steps once in a shared context and return a `config`
/// object mapping each step's `outputs` name to its output value.
///
/// `vars` (the static `variables`) is seeded so setup steps can interpolate
/// `${{ vars.* }}`; each setup step also sees earlier setup outputs under their
/// own `outputs` name. Setup runs regardless of `quiet` but respects it for log
/// suppression. The first failing step short-circuits with an `Err` naming it.
///
/// `config` carries the filesystem policy (`allow_file_actions`, `fs_root`)
/// into setup steps — they run the same actions as test steps, so the same
/// gate applies.
async fn run_before(
    before: &[Step],
    vars: &Value,
    config: &RunConfig,
    quiet: bool,
    tx: &mpsc::Sender<LogLine>,
) -> Result<Value, String> {
    if before.is_empty() {
        return Ok(Value::Null);
    }

    emit(
        tx,
        LogSource::System,
        &format!(
            "Running {} setup step{} (before)",
            before.len(),
            if before.len() == 1 { "" } else { "s" }
        ),
    )
    .await;

    let mut ctx = Context::new();
    ctx.allow_file_actions = config.allow_file_actions;
    ctx.fs_root = config.fs_root.clone();
    if !vars.is_null() {
        ctx.set("vars", vars.clone());
    }

    let mut config = Map::new();
    for step in before {
        let action = &step.action;
        let step_name = step.name.as_deref().unwrap_or(action.as_str());
        let empty = Value::Object(Default::default());
        let params = step.with.as_ref().unwrap_or(&empty);

        let output = execute_action(action, params, &ctx, step_name).await;

        for (tag, text) in &output.logs {
            if quiet && *tag != LogTag::Err {
                continue;
            }
            emit(tx, LogSource::from(*tag), text).await;
        }

        if !output.success {
            return Err(format!("setup step '{step_name}' failed"));
        }

        ctx.set("__last__", output.value.clone());
        if let Some(name) = &step.outputs {
            ctx.set(name, output.value.clone());
            config.insert(name.clone(), output.value);
        }
    }

    Ok(Value::Object(config))
}

// ---------------------------------------------------------------------------
// Per-step execution
// ---------------------------------------------------------------------------

async fn execute_step(
    step: &Step,
    ctx: &mut Context,
    tx: &mpsc::Sender<LogLine>,
    metrics: &Arc<Mutex<Metrics>>,
    quiet: bool,
    _vu_id: u32,
) {
    let action = &step.action;
    let step_name = step.name.as_deref().unwrap_or(action.as_str());
    let empty = Value::Object(Default::default());
    let params = step.with.as_ref().unwrap_or(&empty);

    let output = execute_action(action, params, ctx, step_name).await;

    // Collect HTTP timing and any custom counters the action exposed under the
    // reserved `metrics` key of its output value.
    if output.http_sample.is_some() || output.value.get("metrics").is_some() {
        let mut m = metrics.lock().unwrap();
        if let Some(ref sample) = output.http_sample {
            m.record(sample);
        }
        if let Some(obj) = output.value.get("metrics").and_then(|v| v.as_object()) {
            m.add_counters(obj);
        }
    }

    // Stream log lines (quiet drops everything except errors)
    for (tag, text) in &output.logs {
        if quiet && *tag != LogTag::Err {
            continue;
        }
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
            if quiet && *tag != LogTag::Err {
                continue;
            }
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
    async fn run_and_collect(steps: Vec<Step>, config: RunConfig, quiet: bool) -> Vec<LogLine> {
        let (tx, mut rx) = mpsc::channel(512);
        let handle = tokio::spawn(run_steps(steps, config, quiet, tx));

        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        handle.await.unwrap();
        lines
    }

    /// The histogram must stay within its promised ≤1% quantile error and
    /// keep sub-millisecond resolution — the properties that let it replace
    /// exact per-request storage.
    #[test]
    fn metrics_histogram_quantiles_within_one_percent() {
        let mut m = Metrics::default();
        for i in 1..=10_000u64 {
            m.record(&HttpSample {
                duration_ms: i as f64 / 10.0, // 0.1ms .. 1000ms, uniform
                status: 200,
                failed: false,
            });
        }

        let lines = m.summary_lines(10.0, 10_000, 1);
        let dur = lines
            .iter()
            .find(|l| l.starts_with("http_req_duration"))
            .unwrap();

        let get = |key: &str| -> f64 {
            let start = dur.find(key).unwrap() + key.len();
            dur[start..].split("ms").next().unwrap().parse().unwrap()
        };

        let within =
            |actual: f64, expected: f64| (actual - expected).abs() <= expected * 0.011 + 0.01;
        assert!(within(get("p(50)="), 500.0), "p50: {dur}");
        assert!(within(get("p(90)="), 900.0), "p90: {dur}");
        assert!(within(get("p(99)="), 990.0), "p99: {dur}");
        assert!(within(get("avg="), 500.05), "avg: {dur}");
        assert!(within(get("max="), 1000.0), "max: {dur}");
        // Sub-millisecond floor survives (0.1ms recorded as 100µs).
        assert!(get("min=") <= 0.11, "min: {dur}");
    }

    /// Custom action counters accumulate and surface as `<name>: total rate/s`
    /// summary lines the downstream parser understands.
    #[test]
    fn metrics_custom_counters_appear_in_summary() {
        let mut m = Metrics::default();
        let obj = json!({ "fix_messages_sent": 3.0, "fix_messages_received": 2.0 });
        m.add_counters(obj.as_object().unwrap());
        m.add_counters(obj.as_object().unwrap()); // accumulate a second step

        let lines = m.summary_lines(2.0, 4, 1);
        let sent = lines
            .iter()
            .find(|l| l.starts_with("fix_messages_sent"))
            .expect("counter line present");
        // 3+3 = 6 total, 6/2s = 3.00/s
        assert!(sent.contains("6") && sent.contains("3.00/s"), "{sent}");
        assert!(lines.iter().any(|l| l.starts_with("fix_messages_received")));
    }

    /// Array values under the `metrics` key are histogram samples (ms):
    /// aggregated across steps and summarised in the `avg/p(..)` shape.
    #[test]
    fn metrics_custom_histograms_appear_in_summary() {
        let mut m = Metrics::default();
        m.add_counters(json!({ "ws_msg_rtt": [10.0, 20.0] }).as_object().unwrap());
        m.add_counters(json!({ "ws_msg_rtt": [30.0, 40.0] }).as_object().unwrap());

        let lines = m.summary_lines(2.0, 4, 1);
        let rtt = lines
            .iter()
            .find(|l| l.starts_with("ws_msg_rtt"))
            .expect("histogram line present");

        // The HDR histogram promises ≤1% quantile error — assert within it.
        let get = |key: &str| -> f64 {
            let start = rtt.find(key).unwrap() + key.len();
            rtt[start..].split("ms").next().unwrap().parse().unwrap()
        };
        let within = |actual: f64, expected: f64| (actual - expected).abs() <= expected * 0.011;
        assert!(within(get("avg="), 25.0), "{rtt}");
        assert!(within(get("min="), 10.0), "{rtt}");
        assert!(within(get("max="), 40.0), "{rtt}");
        assert!(rtt.contains("count=4"), "{rtt}");
        // A histogram is not double-counted as a counter.
        assert!(!rtt.contains("/s"), "{rtt}");
    }

    /// One `metrics` object can mix counters and histogram samples.
    #[test]
    fn metrics_mixed_counters_and_histograms() {
        let mut m = Metrics::default();
        let obj = json!({ "ws_msgs_sent": 5.0, "ws_msg_rtt": [12.5] });
        m.add_counters(obj.as_object().unwrap());

        let lines = m.summary_lines(1.0, 1, 1);
        assert!(lines.iter().any(|l| l.starts_with("ws_msgs_sent: 5")));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("ws_msg_rtt") && l.contains("count=1")));
    }

    #[test]
    fn metrics_stats_line_reports_window_rate_and_percentiles() {
        let mut m = Metrics::default();
        for _ in 0..10 {
            m.record(&HttpSample {
                duration_ms: 2.0,
                status: 200,
                failed: false,
            });
        }
        // 10 requests in a 5s window → 2.00 rps
        let line = m.stats_line(1_720_000_000_000, 10, 5.0, 42);
        assert!(line.starts_with("[stats] ts=1720000000000 "), "{line}");
        assert!(line.contains("rps=2.00"), "{line}");
        assert!(line.contains("p50="), "{line}");
        assert!(line.contains("p99="), "{line}");
        assert!(line.contains("reqs=10"), "{line}");
        assert!(line.contains("iters=42"), "{line}");
    }

    #[test]
    fn metrics_stats_line_without_requests_omits_percentiles() {
        let m = Metrics::default();
        let line = m.stats_line(1, 0, 5.0, 3);
        assert!(line.contains("reqs=0"), "{line}");
        assert!(!line.contains("p50="), "{line}");
    }

    /// Out-of-range values must clamp, not panic or vanish.
    #[test]
    fn metrics_histogram_clamps_extreme_durations() {
        let mut m = Metrics::default();
        for ms in [0.0, 10_000_000.0] {
            m.record(&HttpSample {
                duration_ms: ms,
                status: 200,
                failed: false,
            });
        }
        let lines = m.summary_lines(1.0, 2, 1);
        let dur = lines.iter().find(|l| l.starts_with("http_reqs")).unwrap();
        assert!(dur.contains("2 "), "both samples counted: {dur}");
    }

    #[tokio::test]
    async fn run_steps_sleep_only_emits_start_and_done_markers() {
        let config = RunConfig {
            vus: 1,
            duration: "1s".into(),
            ..Default::default()
        };
        let lines = run_and_collect(vec![sleep_step(10)], config, false).await;

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
            ..Default::default()
        };
        let lines = run_and_collect(steps, config, false).await;

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
            ..Default::default()
        };
        let lines = run_and_collect(steps, config, false).await;

        let check_line = lines
            .iter()
            .find(|l| l.text.contains("[check]"))
            .expect("check log line present");
        assert_eq!(check_line.source, LogSource::Stderr);
        assert!(check_line.text.contains("FAIL"));
    }

    #[tokio::test]
    async fn run_steps_quiet_drops_request_lines_but_keeps_summary() {
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
            sleep_step(50),
        ];
        let config = RunConfig {
            vus: 1,
            duration: "1s".into(),
            ..Default::default()
        };
        let lines = run_and_collect(steps, config, true).await;

        assert!(
            !lines.iter().any(|l| l.text.contains("→ 200")),
            "per-request lines must be suppressed under quiet"
        );
        assert!(
            !lines.iter().any(|l| l.text.contains("sleep 50ms")),
            "sleep markers must be suppressed under quiet"
        );
        assert!(lines
            .iter()
            .any(|l| l.text.starts_with("http_req_duration")));
        assert!(lines.iter().any(|l| l.text.starts_with("http_reqs")));
    }

    #[tokio::test]
    async fn run_steps_quiet_still_reports_check_failures() {
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
            ..Default::default()
        };
        let lines = run_and_collect(steps, config, true).await;

        let check_line = lines
            .iter()
            .find(|l| l.text.contains("[check]"))
            .expect("failing check must survive quiet mode");
        assert_eq!(check_line.source, LogSource::Stderr);
        assert!(check_line.text.contains("FAIL"));
    }

    #[tokio::test]
    async fn run_steps_multiple_vus_reports_correct_count() {
        let config = RunConfig {
            vus: 3,
            duration: "1s".into(),
            ..Default::default()
        };
        let lines = run_and_collect(vec![sleep_step(5)], config, false).await;
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
            ..Default::default()
        };
        let lines = run_and_collect(steps, config, false).await;
        assert!(lines.iter().any(|l| l.text == "status was 200"));
    }

    #[tokio::test]
    async fn run_steps_zero_vus_is_clamped_to_one() {
        let config = RunConfig {
            vus: 0,
            duration: "1s".into(),
            ..Default::default()
        };
        let lines = run_and_collect(vec![sleep_step(5)], config, false).await;
        assert!(lines.iter().any(|l| l.text.starts_with("Starting 1 VU")));
    }

    /// End-to-end WebSocket flow through the VU loop: a Live Connection is
    /// usable across steps, custom ws metrics fold into the summary, and the
    /// iteration-end drain lets every iteration reconnect cleanly.
    #[tokio::test]
    async fn run_steps_websocket_live_connection_and_metrics() {
        use futures_util::{SinkExt as _, StreamExt as _};

        // Minimal echo server (accept loop → per-connection echo).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                    while let Some(Ok(msg)) = ws.next().await {
                        match msg {
                            tokio_tungstenite::tungstenite::Message::Text(t) => {
                                let echo = tokio_tungstenite::tungstenite::Message::Text(t);
                                if ws.send(echo).await.is_err() {
                                    break;
                                }
                            }
                            tokio_tungstenite::tungstenite::Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });

        let steps = vec![
            Step {
                name: Some("open".into()),
                action: "std/ws-connect@v1".into(),
                with: Some(json!({ "url": url })),
                check: None,
                outputs: Some("feed".into()),
            },
            Step {
                name: Some("sub".into()),
                action: "std/ws-send@v1".into(),
                with: Some(json!({ "id": "${{ feed.id }}", "send": "sub-${seq}" })),
                check: None,
                outputs: None,
            },
            Step {
                name: Some("wait".into()),
                action: "std/ws-recv@v1".into(),
                with: Some(json!({ "id": "${{ feed.id }}", "until_contains": "sub-1" })),
                check: Some(json!({ "message_contains": "sub-1" })),
                outputs: None,
            },
            // No explicit close — the iteration-end drain must handle it.
            sleep_step(50),
        ];
        let config = RunConfig {
            vus: 1,
            duration: "1s".into(),
            ..Default::default()
        };
        let lines = run_and_collect(steps, config, false).await;

        assert!(
            !lines
                .iter()
                .any(|l| l.text.contains("[check]") && l.text.contains("FAIL")),
            "no failing checks expected: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.text.starts_with("ws_msgs_sent")),
            "custom counter in summary: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.text.starts_with("ws_msg_rtt") && l.text.contains("count=")),
            "RTT histogram in summary: {lines:?}"
        );
        // The handshake feeds the shared latency histogram.
        assert!(lines
            .iter()
            .any(|l| l.text.starts_with("http_req_duration")));
    }

    /// End-to-end gRPC flow through the VU loop: a Live Channel + stream are
    /// usable across steps (schema via reflection), and the custom grpc
    /// metrics fold into the summary lines.
    #[tokio::test]
    async fn run_steps_grpc_live_channel_and_metrics() {
        let port = crate::testsupport::start_echo_server().await;
        let url = format!("grpc://127.0.0.1:{port}");

        let steps = vec![
            Step {
                name: Some("connect".into()),
                action: "std/grpc-connect@v1".into(),
                with: Some(json!({ "url": url, "reflection": true })),
                check: None,
                outputs: Some("conn".into()),
            },
            Step {
                name: Some("unary".into()),
                action: "std/grpc-call@v1".into(),
                with: Some(json!({
                    "id": "${{ conn.id }}",
                    "method": "perfscale.test.v1.Echo/Unary",
                    "payload": { "message": "ping-${seq}" },
                })),
                check: None,
                outputs: None,
            },
            Step {
                name: Some("open".into()),
                action: "std/grpc-stream-open@v1".into(),
                with: Some(json!({
                    "id": "${{ conn.id }}",
                    "method": "perfscale.test.v1.Echo/Bidi",
                })),
                check: None,
                outputs: Some("stream".into()),
            },
            Step {
                name: Some("send".into()),
                action: "std/grpc-stream-send@v1".into(),
                with: Some(json!({
                    "id": "${{ stream.id }}",
                    "payload": { "message": "evt-${seq}" },
                    "repeat": 5,
                })),
                check: None,
                outputs: None,
            },
            Step {
                name: Some("recv".into()),
                action: "std/grpc-stream-recv@v1".into(),
                with: Some(json!({
                    "id": "${{ stream.id }}",
                    "until_contains": "evt-5",
                    "timeout": 5000,
                })),
                check: Some(json!({ "messages_count_gte": 5 })),
                outputs: None,
            },
            Step {
                name: Some("close".into()),
                action: "std/grpc-stream-close@v1".into(),
                with: Some(json!({ "id": "${{ stream.id }}" })),
                check: None,
                outputs: None,
            },
        ];
        let config = RunConfig {
            vus: 1,
            duration: "1s".into(),
            ..Default::default()
        };
        let lines = run_and_collect(steps, config, false).await;

        assert!(
            !lines
                .iter()
                .any(|l| l.text.contains("[check]") && l.text.contains("FAIL")),
            "no failing checks expected: {lines:?}"
        );
        for metric in [
            "grpc_req_duration",
            "grpc_msgs_sent",
            "grpc_msgs_received",
            "grpc_req_failed",
        ] {
            assert!(
                lines.iter().any(|l| l.text.starts_with(metric)),
                "{metric} in summary: {lines:?}"
            );
        }
        assert!(
            lines
                .iter()
                .any(|l| l.text.starts_with("grpc_msg_rtt") && l.text.contains("count=")),
            "RTT histogram in summary: {lines:?}"
        );
    }

    // -----------------------------------------------------------------
    // run_native — before / variables
    // -----------------------------------------------------------------

    fn log_step(name: &str, message: &str, outputs: Option<&str>) -> Step {
        Step {
            name: Some(name.into()),
            action: "std/log@v1".into(),
            with: Some(json!({ "message": message })),
            check: None,
            outputs: outputs.map(str::to_owned),
        }
    }

    async fn run_native_and_collect(
        steps: Vec<Step>,
        before: Vec<Step>,
        variables: Map<String, Value>,
        config: RunConfig,
    ) -> Vec<LogLine> {
        let (tx, mut rx) = mpsc::channel(512);
        let handle = tokio::spawn(run_native(steps, before, config, variables, false, tx));
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        handle.await.unwrap();
        lines
    }

    /// A `before` step's `outputs` is exposed to test steps under `config.<name>`.
    #[tokio::test]
    async fn before_output_flows_into_test_steps_as_config() {
        // file-write is a convenient action whose output has known fields, but
        // a std/http against a mock is closer to the real story. Use file-read
        // to seed a value, then reference it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.txt");
        std::fs::write(&path, "example.com").unwrap();

        let before = vec![Step {
            name: Some("load host".into()),
            action: "std/file-read@v1".into(),
            with: Some(json!({ "path": path.to_str().unwrap() })),
            check: None,
            outputs: Some("cfg".into()),
        }];
        // Test step logs the config value interpolated from the before output.
        let steps = vec![log_step("show", "host=${{ config.cfg.content }}", None)];

        let lines = run_native_and_collect(
            steps,
            before,
            Map::new(),
            RunConfig {
                vus: 1,
                duration: "1s".into(),
                // The `before` step reads a file — opt in explicitly (file
                // actions are fail-closed by default).
                allow_file_actions: true,
                ..Default::default()
            },
        )
        .await;

        assert!(
            lines.iter().any(|l| l.text == "host=example.com"),
            "config.cfg.content must interpolate into the test step: {lines:?}"
        );
    }

    /// Static `variables` are exposed to test steps under `vars.*`.
    #[tokio::test]
    async fn variables_flow_into_test_steps_as_vars() {
        let mut vars = Map::new();
        vars.insert("region".into(), json!("eu-west"));
        let steps = vec![log_step("show", "region=${{ vars.region }}", None)];

        let lines = run_native_and_collect(
            steps,
            Vec::new(),
            vars,
            RunConfig {
                vus: 1,
                duration: "1s".into(),
                ..Default::default()
            },
        )
        .await;
        assert!(lines.iter().any(|l| l.text == "region=eu-west"));
    }

    /// A `before` step can read `${{ vars.* }}`, and later `before` steps see
    /// earlier setup outputs under their own name.
    #[tokio::test]
    async fn before_steps_see_vars_and_prior_outputs() {
        let mut vars = Map::new();
        vars.insert("greeting".into(), json!("hello"));
        // Setup emits a marker referencing vars; we assert the setup log line.
        let before = vec![log_step("greet", "setup ${{ vars.greeting }}", Some("g"))];
        let steps = vec![sleep_step(1)];

        let lines = run_native_and_collect(
            steps,
            before,
            vars,
            RunConfig {
                vus: 1,
                duration: "1s".into(),
                ..Default::default()
            },
        )
        .await;
        assert!(lines.iter().any(|l| l.text == "setup hello"));
        assert!(lines.iter().any(|l| l.text.contains("setup step")));
    }

    /// A failing `before` step aborts the run before any VU starts.
    #[tokio::test]
    async fn failing_before_step_aborts_before_vus() {
        // std/http to an unlistenable port fails → setup fails.
        let before = vec![Step {
            name: Some("bad setup".into()),
            action: "std/http@v1".into(),
            with: Some(json!({ "url": "http://127.0.0.1:0/", "timeout": 1000 })),
            check: None,
            outputs: None,
        }];
        let steps = vec![log_step("should-not-run", "MUST NOT APPEAR", None)];

        let lines = run_native_and_collect(
            steps,
            before,
            Map::new(),
            RunConfig {
                vus: 5,
                duration: "1s".into(),
                ..Default::default()
            },
        )
        .await;

        assert!(
            lines.iter().any(|l| l.text.contains("Setup failed")),
            "expected a setup-failure line: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.text == "MUST NOT APPEAR"),
            "test steps must not run after setup failure"
        );
        assert!(
            !lines.iter().any(|l| l.text.starts_with("Starting")),
            "no VUs must be spawned after setup failure"
        );
    }

    /// `run_steps` is `run_native` with no setup — no setup banner, VUs run.
    #[tokio::test]
    async fn run_steps_is_run_native_without_setup() {
        let lines = run_and_collect(
            vec![sleep_step(1)],
            RunConfig {
                vus: 1,
                duration: "1s".into(),
                ..Default::default()
            },
            false,
        )
        .await;
        assert!(!lines.iter().any(|l| l.text.contains("setup step")));
        assert!(lines.iter().any(|l| l.text.starts_with("Starting 1 VU")));
    }
}
