//! Native load runner.
//!
//! Spawns N virtual users (tokio tasks), each running the step list in a loop
//! until the configured duration expires.  Metrics are collected in a shared
//! structure and summarised in a k6-compatible text format so downstream
//! parsers (dashboards, `perfscale serve`) work the same for all three engines.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::runner::{LogLine, LogSource};
use crate::step::{
    actions::{execute_action, HttpSample, LogTag},
    context::Context,
    process::ProcessRegistry,
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
    /// Per-invocation failure samples as `(total, failed)` pairs, recorded by
    /// the runner for every histogram metric a step emits (see
    /// [`execute_step`]): `db_query_duration` → `db_query_failed`. These are
    /// NOT stored in `hists` — the HDR histogram clamps samples to ≥1µs,
    /// which would turn every 0 (success) into a 1 (failure). Summarised as
    /// `<name>: <pct>%` like k6's `http_req_failed`, and the source of the
    /// `rate` aggregate for `std/thresholds@v1`.
    rates: std::collections::BTreeMap<String, (u64, u64)>,
    /// Outcomes of `std/thresholds@v1` steps (run in `after:`), combined into
    /// the run summary's `thresholds` field at the end of the run.
    threshold_results: Vec<crate::step::thresholds::ThresholdsSummary>,
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
            rates: std::collections::BTreeMap::new(),
            threshold_results: Vec::new(),
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

    /// Record one invocation outcome for a failure-rate metric: `failed`
    /// is 1 when the step invocation failed, 0 when it succeeded. `rate`
    /// over these samples is exactly failed/total invocations.
    pub fn record_rate(&mut self, name: &str, failed: bool) {
        let e = self.rates.entry(name.to_string()).or_insert((0, 0));
        e.0 += 1;
        if failed {
            e.1 += 1;
        }
    }

    /// Store the outcome of a `std/thresholds@v1` step for the run summary.
    pub fn record_threshold_result(&mut self, result: crate::step::thresholds::ThresholdsSummary) {
        self.threshold_results.push(result);
    }

    /// All gate outcomes combined (worst status wins), for the run summary's
    /// `thresholds` field. `None` when no thresholds step ran.
    pub fn thresholds_summary(&self) -> Option<crate::step::thresholds::ThresholdsSummary> {
        crate::step::thresholds::combine(self.threshold_results.clone())
    }

    /// Snapshot every metric the run collected, for `std/thresholds@v1`
    /// evaluation. Percentiles come from the same HDR histograms (and the
    /// same accessors) as [`Metrics::summary_lines`], so gate numbers match
    /// the printed summary.
    ///
    /// A counter is shadowed by a same-named failure-rate metric (the gRPC
    /// actions emit a `grpc_req_failed` counter AND the runner derives
    /// `grpc_req_failed` rate samples from `grpc_req_duration`): the rate
    /// view wins, since `rate`/`count` on it answer both use cases.
    pub fn metric_snapshot(
        &self,
    ) -> std::collections::BTreeMap<String, crate::step::thresholds::MetricAgg> {
        use crate::step::thresholds::MetricAgg;
        let mut out = std::collections::BTreeMap::new();
        if self.total > 0 {
            out.insert(
                "http_req_duration".to_string(),
                MetricAgg::sample(&self.durations_micros),
            );
            out.insert(
                "http_req_failed".to_string(),
                MetricAgg::rate(self.total, self.failures),
            );
        }
        for (name, h) in &self.hists {
            out.insert(name.clone(), MetricAgg::sample(h));
        }
        for (name, v) in &self.counters {
            if !self.rates.contains_key(name) {
                out.insert(name.clone(), MetricAgg::counter(*v));
            }
        }
        for (name, (total, failed)) in &self.rates {
            out.insert(name.clone(), MetricAgg::rate(*total, *failed));
        }
        out
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
        // not the run made HTTP-style requests. A counter shadowed by a
        // same-named failure-rate metric (e.g. `grpc_req_failed`) is skipped:
        // the rate line below carries the same information in k6's `%` shape.
        for (name, total) in &self.counters {
            if self.rates.contains_key(name) {
                continue;
            }
            let rate = total / wall_secs.max(0.001);
            lines.push(format!("{name}: {total:.0} {rate:.2}/s"));
        }

        // Failure-rate metrics (`db_query_failed`, …) — k6's
        // `http_req_failed: 0.00%` shape.
        for (name, (total, failed)) in &self.rates {
            let pct = *failed as f64 / (*total).max(1) as f64 * 100.0;
            lines.push(format!("{name}: {pct:.2}%"));
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

/// What a native run concluded, beyond the streamed log lines.
#[derive(Debug, Default)]
pub struct NativeRunOutcome {
    /// Combined result of every `std/thresholds@v1` gate that ran in
    /// `after:` — `None` when the config had no thresholds step.
    pub thresholds: Option<crate::step::thresholds::ThresholdsSummary>,
}

impl NativeRunOutcome {
    /// True when a thresholds gate evaluated to `fail` — the CLI turns this
    /// into a non-zero exit code for CI.
    pub fn thresholds_failed(&self) -> bool {
        self.thresholds
            .as_ref()
            .is_some_and(|t| t.status == "fail")
    }
}

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
/// `before`/`after` steps and no static variables. Kept for callers (and
/// tests) that only have a step list and a run config.
pub async fn run_steps(
    steps: Vec<Step>,
    config: RunConfig,
    quiet: bool,
    tx: mpsc::Sender<LogLine>,
) {
    run_native(steps, Vec::new(), Vec::new(), config, Map::new(), quiet, tx).await;
}

/// Execute a native test with optional one-time `before` setup and `after`
/// teardown, plus static `variables`.
///
/// `before` steps run once, in order, before any VU is spawned. Each step's
/// `outputs` is collected into a `config` object exposed to every test step as
/// `${{ config.<name>.<field> }}`; `variables` is exposed as `${{ vars.* }}`.
/// If any setup step fails, the run aborts before spawning VUs — a broken
/// setup would make every iteration fail identically, so failing fast is
/// clearer than a wall of downstream errors.
///
/// `after` steps run once on every exit path — normal finish, failed
/// `before`, or interrupted run — best-effort: a failing teardown step is
/// logged but does not abort the rest. After them, any managed child
/// processes still alive are stopped automatically.
///
/// The first SIGINT/SIGTERM asks the VU loop to stop (it notices between
/// steps) and the run proceeds through the usual teardown; a second signal
/// exits the process immediately.
///
/// Returns a [`NativeRunOutcome`]: the combined `std/thresholds@v1` gate
/// result, so the caller (CLI) can exit non-zero when a `severity: fail`
/// gate was violated.
pub async fn run_native(
    steps: Vec<Step>,
    before: Vec<Step>,
    after: Vec<Step>,
    config: RunConfig,
    variables: Map<String, Value>,
    quiet: bool,
    tx: mpsc::Sender<LogLine>,
) -> NativeRunOutcome {
    let vars = if variables.is_empty() {
        Value::Null
    } else {
        Value::Object(variables)
    };

    // Managed child processes live for the whole run — whether they were
    // spawned in `before`, a test step or `after`, everything still alive at
    // the end is stopped via `shutdown_all` on every exit path.
    let registry = Arc::new(ProcessRegistry::new());

    // Created up front (not after setup) so `after:` thresholds steps get a
    // metrics handle on every exit path — including a failed `before`, where
    // it is simply empty.
    let metrics = Arc::new(Mutex::new(Metrics::default()));

    // First SIGINT/SIGTERM flips this flag (soft stop); a second one exits
    // the process outright. The handler task is aborted when the run ends.
    let stop = Arc::new(AtomicBool::new(false));
    let interrupt_handler = spawn_interrupt_handler(Arc::clone(&stop), tx.clone());

    let after_shared = AfterShared {
        registry: Arc::clone(&registry),
        metrics: Arc::clone(&metrics),
    };

    // --- One-time setup ---
    let config_seed = match run_before(&before, &vars, &config, &registry, quiet, &tx).await {
        Ok(v) => v,
        Err(msg) => {
            emit(
                &tx,
                LogSource::Stderr,
                &format!("Setup failed, aborting run: {msg}"),
            )
            .await;
            // Teardown still runs: a `before` step may have started a process
            // (or grabbed anything else `after` exists to clean up) before
            // the one that failed.
            run_after(&after, &Value::Null, &vars, &config, &after_shared, quiet, &tx).await;
            registry.shutdown_all().await;
            emit(&tx, LogSource::System, "Done — setup error").await;
            interrupt_handler.abort();
            return NativeRunOutcome::default();
        }
    };

    let duration_secs = config.duration_secs();
    let vus = config.vus.max(1);
    let deadline = Instant::now() + Duration::from_secs(duration_secs);
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
        let allow_process_actions = config.allow_process_actions;
        let processes = Arc::clone(&registry);
        let stop = Arc::clone(&stop);
        let tx = tx.clone();

        handles.push(tokio::spawn(async move {
            let mut ctx = Context::new();
            ctx.allow_file_actions = allow_file_actions;
            ctx.allow_process_actions = allow_process_actions;
            ctx.fs_root = fs_root;
            ctx.processes = Some(processes);
            ctx.log_tx = Some(tx.clone());
            if !config_seed.is_null() {
                ctx.set("config", (*config_seed).clone());
            }
            if !vars.is_null() {
                ctx.set("vars", (*vars).clone());
            }

            while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                iter_count.fetch_add(1, Ordering::Relaxed);
                for step in steps_ref.iter() {
                    execute_step(step, &mut ctx, &tx, &metrics, quiet, vu_id).await;
                    if Instant::now() >= deadline || stop.load(Ordering::Relaxed) {
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

    // Teardown on every non-setup-failure path: `after` steps (best-effort),
    // then stop whatever managed processes are still alive.
    run_after(&after, &config_seed, &vars, &config, &after_shared, quiet, &tx).await;
    registry.shutdown_all().await;

    let wall_secs = started.elapsed().as_secs_f64();
    let total_iters = iter_count.load(Ordering::Relaxed);
    let (lines, thresholds) = {
        let m = metrics.lock().unwrap();
        (m.summary_lines(wall_secs, total_iters, vus), m.thresholds_summary())
    };
    for line in &lines {
        emit(&tx, LogSource::Stdout, line).await;
    }
    // Machine-readable gate result for the run summary JSON / CI: one line,
    // consumed like the other summary lines downstream.
    if let Some(ref t) = thresholds {
        emit(
            &tx,
            LogSource::Stdout,
            &format!(
                "thresholds: {}",
                serde_json::to_string(t).expect("thresholds summary is always serializable")
            ),
        )
        .await;
    }
    emit(
        &tx,
        LogSource::System,
        &format!("Done — {wall_secs:.1}s wall clock"),
    )
    .await;
    interrupt_handler.abort();
    NativeRunOutcome { thresholds }
}

// ---------------------------------------------------------------------------
// Interrupt handling (SIGINT/SIGTERM)
// ---------------------------------------------------------------------------

/// Install the two-stage interrupt handler for a native run.
///
/// The first SIGINT/SIGTERM flips `stop`: the VU loop notices between steps
/// and the run proceeds through the usual teardown (`after` steps, process
/// shutdown, summary). A second signal exits the process immediately —
/// teardown itself may be wedged, and the operator clearly wants out.
///
/// Returns the handler task; the caller aborts it when the run ends so a
/// long-lived embedding process (agent mode) stops intercepting signals once
/// perfscale is done.
fn spawn_interrupt_handler(
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<LogLine>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // SIGTERM only exists on unix; elsewhere Ctrl-C (SIGINT) is all we get.
        #[cfg(unix)]
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

        loop {
            #[cfg(unix)]
            {
                tokio::select! {
                    r = tokio::signal::ctrl_c() => {
                        if r.is_err() {
                            return;
                        }
                    }
                    r = async {
                        match sigterm.as_mut() {
                            Some(s) => s.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if r.is_none() {
                            return;
                        }
                    }
                }
            }
            #[cfg(not(unix))]
            {
                if tokio::signal::ctrl_c().await.is_err() {
                    return;
                }
            }

            if stop.swap(true, Ordering::SeqCst) {
                // Second interrupt: hard exit (128 + SIGINT).
                std::process::exit(130);
            }
            emit(
                &tx,
                LogSource::System,
                "Interrupt received — stopping load, running teardown (interrupt again to force-quit)",
            )
            .await;
        }
    })
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
/// and the process policy (`allow_process_actions`, shared `registry`) into
/// setup steps — they run the same actions as test steps, so the same gates
/// apply.
async fn run_before(
    before: &[Step],
    vars: &Value,
    config: &RunConfig,
    registry: &Arc<ProcessRegistry>,
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
    ctx.allow_process_actions = config.allow_process_actions;
    ctx.fs_root = config.fs_root.clone();
    ctx.processes = Some(Arc::clone(registry));
    ctx.log_tx = Some(tx.clone());
    if !vars.is_null() {
        ctx.set("vars", vars.clone());
    }

    let mut config = Map::new();
    for step in before {
        let action = &step.action;
        let step_name = step.name.as_deref().unwrap_or(action.as_str());
        let params = step_params(step);

        let output = execute_action(action, &params, &ctx, step_name).await;

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
            // A process step becomes killable by its outputs name too.
            registry.alias(step_name, name);
        }
    }

    Ok(Value::Object(config))
}

// ---------------------------------------------------------------------------
// One-time teardown (`after`)
// ---------------------------------------------------------------------------

/// Run-scoped shared state handed to `after:` steps: the managed-process
/// registry (a typical teardown step kills what `before` started) and the
/// run's metrics (what `std/thresholds@v1` gates evaluate over).
struct AfterShared {
    registry: Arc<ProcessRegistry>,
    metrics: Arc<Mutex<Metrics>>,
}

/// Run the `after` steps once, best-effort: a failing step is logged but does
/// not abort the remaining ones — teardown exists for cleanup, and partial
/// cleanup beats none.
///
/// The context sees the same `config` seed (from `run_before`) and `vars` as
/// test steps did, plus the shared process registry — the typical `after`
/// step is a `std/kill_process@v1` for a server that `before` started.
///
/// The run's shared [`Metrics`] handle is seeded into the context so
/// `std/thresholds@v1` gates can evaluate over everything the run collected
/// (empty when setup failed before any VU ran).
async fn run_after(
    after: &[Step],
    config_seed: &Value,
    vars: &Value,
    config: &RunConfig,
    shared: &AfterShared,
    quiet: bool,
    tx: &mpsc::Sender<LogLine>,
) {
    if after.is_empty() {
        return;
    }

    emit(
        tx,
        LogSource::System,
        &format!(
            "Running {} teardown step{} (after)",
            after.len(),
            if after.len() == 1 { "" } else { "s" }
        ),
    )
    .await;

    let mut ctx = Context::new();
    ctx.allow_file_actions = config.allow_file_actions;
    ctx.allow_process_actions = config.allow_process_actions;
    ctx.fs_root = config.fs_root.clone();
    ctx.processes = Some(Arc::clone(&shared.registry));
    ctx.log_tx = Some(tx.clone());
    ctx.run_metrics = Some(Arc::clone(&shared.metrics));
    if !vars.is_null() {
        ctx.set("vars", vars.clone());
    }
    if !config_seed.is_null() {
        ctx.set("config", config_seed.clone());
    }

    for step in after {
        let action = &step.action;
        let step_name = step.name.as_deref().unwrap_or(action.as_str());
        let params = step_params(step);

        let output = execute_action(action, &params, &ctx, step_name).await;

        for (tag, text) in &output.logs {
            if quiet && *tag != LogTag::Err {
                continue;
            }
            emit(tx, LogSource::from(*tag), text).await;
        }

        if !output.success {
            emit(
                tx,
                LogSource::Stderr,
                &format!("teardown step '{step_name}' failed (continuing)"),
            )
            .await;
        }

        ctx.set("__last__", output.value.clone());
        if let Some(name) = &step.outputs {
            ctx.set(name, output.value.clone());
            shared.registry.alias(step_name, name);
        }
    }
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
    let params = step_params(step);

    let output = execute_action(action, &params, ctx, step_name).await;

    // Collect HTTP timing and any custom counters the action exposed under the
    // reserved `metrics` key of its output value.
    if output.http_sample.is_some() || output.value.get("metrics").is_some() {
        let mut m = metrics.lock().unwrap();
        if let Some(ref sample) = output.http_sample {
            m.record(sample);
        }
        if let Some(obj) = output.value.get("metrics").and_then(|v| v.as_object()) {
            m.add_counters(obj);
            // Failure sampling: one 0/1 sample per invocation for every
            // family-duration metric the invocation emitted, so
            // `std/thresholds@v1` can compute `rate` = failed/total
            // invocations. `db_query_duration` → `db_query_failed`,
            // `ws_msg_rtt` → `ws_msg_failed`, matching the HTTP family's
            // native `http_req_failed`.
            let failed = !output.success;
            for (name, v) in obj {
                if v.is_array() {
                    let family = name
                        .strip_suffix("_duration")
                        .or_else(|| name.strip_suffix("_rtt"))
                        .unwrap_or(name);
                    m.record_rate(&format!("{family}_failed"), failed);
                }
            }
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
        // A process step becomes killable by its outputs name too.
        if let Some(reg) = &ctx.processes {
            reg.alias(step_name, name);
        }
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

/// The params handed to an action: the step's `with` object, with the
/// step-level `severity`/`message` fields (used by `std/thresholds@v1`)
/// merged in so actions read one flat parameter set. Borrows unless a merge
/// is actually needed, keeping the per-iteration hot path allocation-free.
fn step_params(step: &Step) -> std::borrow::Cow<'_, Value> {
    if step.severity.is_none() && step.message.is_none() {
        return match &step.with {
            Some(w) => std::borrow::Cow::Borrowed(w),
            None => std::borrow::Cow::Owned(Value::Object(Map::new())),
        };
    }
    let mut obj = step
        .with
        .as_ref()
        .and_then(|w| w.as_object())
        .cloned()
        .unwrap_or_default();
    if let Some(severity) = &step.severity {
        obj.insert("severity".to_string(), Value::String(severity.clone()));
    }
    if let Some(message) = &step.message {
        obj.insert("message".to_string(), Value::String(message.clone()));
    }
    std::borrow::Cow::Owned(Value::Object(obj))
}

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
            severity: None,
            message: None,
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
                severity: None,
                message: None,
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
            severity: None,
            message: None,
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
                severity: None,
                message: None,
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
            severity: None,
            message: None,
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
                severity: None,
                message: None,
            },
            Step {
                name: Some("report".into()),
                action: "std/log@v1".into(),
                with: Some(json!({ "message": "status was ${{ resp.status }}" })),
                check: None,
                outputs: None,
                severity: None,
                message: None,
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
                severity: None,
                message: None,
            },
            Step {
                name: Some("sub".into()),
                action: "std/ws-send@v1".into(),
                with: Some(json!({ "id": "${{ feed.id }}", "send": "sub-${seq}" })),
                check: None,
                outputs: None,
                severity: None,
                message: None,
            },
            Step {
                name: Some("wait".into()),
                action: "std/ws-recv@v1".into(),
                with: Some(json!({ "id": "${{ feed.id }}", "until_contains": "sub-1" })),
                check: Some(json!({ "message_contains": "sub-1" })),
                outputs: None,
                severity: None,
                message: None,
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
                severity: None,
                message: None,
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
                severity: None,
                message: None,
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
                severity: None,
                message: None,
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
                severity: None,
                message: None,
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
                severity: None,
                message: None,
            },
            Step {
                name: Some("close".into()),
                action: "std/grpc-stream-close@v1".into(),
                with: Some(json!({ "id": "${{ stream.id }}" })),
                check: None,
                outputs: None,
                severity: None,
                message: None,
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
            severity: None,
            message: None,
        }
    }

    async fn run_native_and_collect(
        steps: Vec<Step>,
        before: Vec<Step>,
        after: Vec<Step>,
        variables: Map<String, Value>,
        config: RunConfig,
    ) -> Vec<LogLine> {
        run_native_full(steps, before, after, variables, config).await.0
    }

    /// Like [`run_native_and_collect`] but also returns the run outcome
    /// (thresholds gate results).
    async fn run_native_full(
        steps: Vec<Step>,
        before: Vec<Step>,
        after: Vec<Step>,
        variables: Map<String, Value>,
        config: RunConfig,
    ) -> (Vec<LogLine>, NativeRunOutcome) {
        let (tx, mut rx) = mpsc::channel(512);
        let handle = tokio::spawn(run_native(
            steps, before, after, config, variables, false, tx,
        ));
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        let outcome = handle.await.unwrap();
        (lines, outcome)
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
            severity: None,
            message: None,
        }];
        // Test step logs the config value interpolated from the before output.
        let steps = vec![log_step("show", "host=${{ config.cfg.content }}", None)];

        let lines = run_native_and_collect(
            steps,
            before,
            Vec::new(),
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
            Vec::new(),
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
            severity: None,
            message: None,
        }];
        let steps = vec![log_step("should-not-run", "MUST NOT APPEAR", None)];

        let lines = run_native_and_collect(
            steps,
            before,
            Vec::new(),
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

    // -----------------------------------------------------------------
    // run_native — after / process cleanup
    // -----------------------------------------------------------------

    /// `after` steps run after the load, see `config.*` and `vars.*`, and a
    /// failing teardown step is logged but does not abort the rest
    /// (best-effort, unlike fail-fast `before`).
    #[tokio::test]
    async fn after_steps_run_after_load_with_config_and_vars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.txt");
        std::fs::write(&path, "example.com").unwrap();

        let before = vec![Step {
            name: Some("load host".into()),
            action: "std/file-read@v1".into(),
            with: Some(json!({ "path": path.to_str().unwrap() })),
            check: None,
            outputs: Some("cfg".into()),
            severity: None,
            message: None,
        }];
        let mut vars = Map::new();
        vars.insert("region".into(), json!("eu"));
        let after = vec![
            log_step(
                "teardown",
                "after host=${{ config.cfg.content }} region=${{ vars.region }}",
                None,
            ),
            // A failing teardown step must not abort the remaining ones.
            Step {
                name: Some("boom".into()),
                action: "std/http@v1".into(),
                with: Some(json!({ "url": "http://127.0.0.1:0/", "timeout": 500 })),
                check: None,
                outputs: None,
                severity: None,
                message: None,
            },
            log_step("teardown-2", "AFTER STILL RUNS", None),
        ];
        let steps = vec![sleep_step(1)];

        let lines = run_native_and_collect(
            steps,
            before,
            after,
            vars,
            RunConfig {
                vus: 1,
                duration: "1s".into(),
                // The `before` step reads a file — opt in explicitly.
                allow_file_actions: true,
                ..Default::default()
            },
        )
        .await;

        assert!(
            lines
                .iter()
                .any(|l| l.text == "after host=example.com region=eu"),
            "after sees config and vars: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.text.contains("teardown step 'boom' failed (continuing)")),
            "failing teardown is logged: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.text == "AFTER STILL RUNS"),
            "teardown continues after a failure: {lines:?}"
        );
        // Order: teardown happens after the VUs ran, before the final marker.
        let pos = |needle: &str| lines.iter().position(|l| l.text.contains(needle));
        let started = pos("Starting 1 VU").unwrap();
        let teardown = pos("after host=example.com").unwrap();
        let done = pos("Done —").unwrap();
        assert!(started < teardown && teardown < done);
    }

    /// `after` runs even when `before` fails — the failing setup may already
    /// have started something the teardown exists to clean up.
    #[tokio::test]
    async fn after_runs_even_when_before_fails() {
        let before = vec![Step {
            name: Some("bad setup".into()),
            action: "std/http@v1".into(),
            with: Some(json!({ "url": "http://127.0.0.1:0/", "timeout": 500 })),
            check: None,
            outputs: None,
            severity: None,
            message: None,
        }];
        let after = vec![log_step("teardown", "AFTER ON SETUP FAILURE", None)];

        let lines = run_native_and_collect(
            vec![sleep_step(1)],
            before,
            after,
            Map::new(),
            RunConfig {
                vus: 1,
                duration: "1s".into(),
                ..Default::default()
            },
        )
        .await;

        assert!(lines.iter().any(|l| l.text.contains("Setup failed")));
        assert!(lines.iter().any(|l| l.text == "AFTER ON SETUP FAILURE"));
        assert!(lines.iter().any(|l| l.text == "Done — setup error"));
        // Still no VUs — teardown must not resurrect the run.
        assert!(!lines.iter().any(|l| l.text.starts_with("Starting")));
    }

    /// Extract the pid a `sh -c 'echo pid=$$; ...'` child echoes into the run
    /// log (lines arrive with the `{step}: ` prefix).
    #[cfg(unix)]
    fn echoed_pid(lines: &[LogLine], prefix: &str) -> i32 {
        lines
            .iter()
            .find_map(|l| {
                l.text
                    .strip_prefix(prefix)
                    .and_then(|p| p.trim().parse().ok())
            })
            .unwrap_or_else(|| panic!("child echoed its pid with prefix '{prefix}': {lines:?}"))
    }

    /// `kill(pid, 0)` liveness probe — ESRCH (or an error other than EPERM)
    /// means the process is really gone, not a zombie.
    #[cfg(unix)]
    fn pid_alive(pid: i32) -> bool {
        unsafe {
            if libc::kill(pid, 0) == 0 {
                true
            } else {
                std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            }
        }
    }

    /// A process started in `before` is stopped automatically at the end of
    /// the run even without an explicit `kill_process` step.
    #[cfg(unix)]
    #[tokio::test]
    async fn child_process_is_auto_killed_after_the_run() {
        let before = vec![Step {
            name: Some("keeper".into()),
            action: "std/child_process@v1".into(),
            with: Some(json!({
                "command": "sh",
                "args": ["-c", "echo pid=$$; sleep 300"],
            })),
            check: None,
            outputs: Some("keeper".into()),
            severity: None,
            message: None,
        }];
        let steps = vec![sleep_step(1)];

        let lines = run_native_and_collect(
            steps,
            before,
            Vec::new(),
            Map::new(),
            RunConfig {
                vus: 1,
                duration: "1s".into(),
                allow_process_actions: true,
                ..Default::default()
            },
        )
        .await;

        let pid = echoed_pid(&lines, "keeper: pid=");
        assert!(!pid_alive(pid), "pid {pid} survived the run");
        assert!(lines.iter().any(|l| l.text.starts_with("Done —")));
    }

    /// The typical process lifecycle: `before` starts a server, `after` stops
    /// it by its `outputs` name (the runner aliases the registry entry).
    #[cfg(unix)]
    #[tokio::test]
    async fn after_can_kill_a_process_by_its_outputs_name() {
        let before = vec![Step {
            name: Some("start server".into()),
            action: "std/child_process@v1".into(),
            with: Some(json!({
                "command": "sh",
                "args": ["-c", "echo pid=$$; sleep 300"],
            })),
            check: None,
            outputs: Some("keeper".into()),
            severity: None,
            message: None,
        }];
        let after = vec![Step {
            name: Some("stop server".into()),
            action: "std/kill_process@v1".into(),
            with: Some(json!({ "name": "keeper" })),
            check: None,
            outputs: None,
            severity: None,
            message: None,
        }];
        let steps = vec![sleep_step(1)];

        let lines = run_native_and_collect(
            steps,
            before,
            after,
            Map::new(),
            RunConfig {
                vus: 1,
                duration: "1s".into(),
                allow_process_actions: true,
                ..Default::default()
            },
        )
        .await;

        let pid = echoed_pid(&lines, "start server: pid=");
        assert!(
            lines
                .iter()
                .any(|l| l.text.contains("sent TERM to 'keeper'")),
            "kill log line: {lines:?}"
        );
        assert!(!pid_alive(pid), "pid {pid} survived the run");
    }

    /// The run-level gate reaches `before` steps: without
    /// `allow_process_actions` a `child_process` setup step fails (and thus
    /// aborts the run before any VU).
    #[cfg(unix)]
    #[tokio::test]
    async fn child_process_in_before_requires_the_gate() {
        let before = vec![Step {
            name: Some("keeper".into()),
            action: "std/child_process@v1".into(),
            with: Some(json!({
                "command": "sh",
                "args": ["-c", "sleep 300"],
            })),
            check: None,
            outputs: None,
            severity: None,
            message: None,
        }];

        let lines = run_native_and_collect(
            vec![sleep_step(1)],
            before,
            Vec::new(),
            Map::new(),
            RunConfig {
                vus: 1,
                duration: "1s".into(),
                ..Default::default()
            },
        )
        .await;

        assert!(
            lines
                .iter()
                .any(|l| l.text.contains("allow_process_actions")),
            "gate error: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.text.contains("Setup failed")));
        assert!(!lines.iter().any(|l| l.text.starts_with("Starting")));
    }

    // -----------------------------------------------------------------
    // run_native — std/thresholds@v1 gates (SQLite end to end)
    // -----------------------------------------------------------------

    /// Rate metrics are 0/1 invocation outcomes, NOT histogram samples (the
    /// HDR clamp would turn every 0 into a 1): they accumulate as
    /// total/failed pairs and print in k6's `http_req_failed` shape.
    #[test]
    fn metrics_rate_lines_and_counter_shadowing() {
        let mut m = Metrics::default();
        for failed in [false, false, false, true] {
            m.record_rate("db_query_failed", failed);
        }
        // A counter sharing the name is shadowed by the rate line.
        m.add_counters(json!({ "grpc_req_failed": 1.0 }).as_object().unwrap());
        m.record_rate("grpc_req_failed", true);

        let lines = m.summary_lines(2.0, 4, 1);
        assert!(
            lines.iter().any(|l| l == "db_query_failed: 25.00%"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "grpc_req_failed: 100.00%"),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("grpc_req_failed: 1 ")),
            "shadowed counter line must be skipped: {lines:?}"
        );
    }

    /// The thresholds snapshot sees every metric kind: sample aggregates from
    /// the same histograms the summary prints, counters, and rate metrics.
    #[test]
    fn metric_snapshot_covers_samples_counters_and_rates() {
        let mut m = Metrics::default();
        m.record(&HttpSample {
            duration_ms: 10.0,
            status: 500,
            failed: true,
        });
        m.record(&HttpSample {
            duration_ms: 30.0,
            status: 200,
            failed: false,
        });
        m.add_counters(
            json!({ "db_query_duration": [12.0, 24.0], "db_errors": 2.0 })
                .as_object()
                .unwrap(),
        );
        m.record_rate("db_query_failed", false);
        m.record_rate("db_query_failed", true);

        let snap = m.metric_snapshot();
        let http = &snap["http_req_duration"];
        assert_eq!(http.kind, crate::step::thresholds::MetricKind::Sample);
        assert_eq!(http.count, 2.0);
        assert!((http.avg - 20.0).abs() < 0.5, "{http:?}");
        assert_eq!(snap["http_req_failed"].rate, 0.5);
        assert_eq!(snap["db_query_duration"].count, 2.0);
        assert_eq!(snap["db_errors"].count, 2.0);
        let rate = &snap["db_query_failed"];
        assert_eq!(rate.rate, 0.5);
        assert_eq!(rate.count, 2.0);
    }

    /// Per-invocation failure samples: the runner derives `db_query_failed`
    /// from `db_query_duration` emissions, 1 on failure / 0 on success.
    #[test]
    fn failed_samples_track_invocation_outcomes() {
        let mut m = Metrics::default();
        // 3 successes, 1 failure — the rate must equal failed/total
        // invocations, not anything sample-weighted.
        for failed in [false, false, false, true] {
            m.record_rate("db_query_failed", failed);
        }
        let snap = m.metric_snapshot();
        assert_eq!(snap["db_query_failed"].rate, 0.25);
        assert_eq!(snap["db_query_failed"].count, 4.0);
    }

    /// A `std/db-*` scenario on in-memory SQLite: connect + query per
    /// iteration. `query` is the SQL every iteration runs.
    fn sqlite_db_steps(query: &str) -> Vec<Step> {
        let step = |action: &str, with: Value, outputs: Option<&str>| Step {
            name: None,
            action: action.into(),
            with: Some(with),
            check: None,
            outputs: outputs.map(str::to_owned),
            severity: None,
            message: None,
        };
        vec![
            step(
                "std/db-connect@v1",
                json!({ "driver": "sqlite", "dsn": "sqlite::memory:" }),
                Some("conn"),
            ),
            step(
                "std/db-query@v1",
                json!({ "id": "${{ conn.id }}", "query": query }),
                None,
            ),
            // Throttle the loop so a 1s run makes dozens, not thousands, of
            // queries — the suite runs many tests in parallel.
            sleep_step(20),
        ]
    }

    fn thresholds_step(with: Value, severity: Option<&str>, message: Option<&str>) -> Step {
        Step {
            name: Some("slo gate".into()),
            action: "std/thresholds@v1".into(),
            with: Some(with),
            check: None,
            outputs: None,
            severity: severity.map(str::to_owned),
            message: message.map(str::to_owned),
        }
    }

    fn one_second() -> RunConfig {
        RunConfig {
            vus: 1,
            duration: "1s".into(),
            ..Default::default()
        }
    }

    /// Passing gate: all queries succeed, every threshold met, the run
    /// summary gains a `thresholds: {"status":"pass",…}` line.
    #[tokio::test]
    async fn thresholds_passing_gate_on_sqlite() {
        let after = vec![thresholds_step(
            json!({
                "db_query_duration": ["p95<5000", "max<5000"],
                "db_query_failed": ["rate<0.05"],
                "db_errors": ["count==0"],
            }),
            None,
            None,
        )];
        let (lines, outcome) =
            run_native_full(sqlite_db_steps("SELECT 1"), Vec::new(), after, Map::new(), one_second())
                .await;

        let t = outcome.thresholds.as_ref().expect("gate outcome present");
        assert_eq!(t.status, "pass", "{}", t.message);
        assert!(!outcome.thresholds_failed());
        assert!(t.violations.is_empty());

        // The failure-rate metric the runner derived is in the text summary…
        assert!(
            lines.iter().any(|l| l.text == "db_query_failed: 0.00%"),
            "{lines:?}"
        );
        // …and the machine-readable gate result follows the metric summary.
        let tline = lines
            .iter()
            .find(|l| l.text.starts_with("thresholds: "))
            .expect("thresholds summary line");
        let parsed = crate::summary::parse_thresholds(&tline.text).unwrap();
        assert_eq!(parsed.status, "pass");
        assert!(lines.iter().any(|l| l.text.contains("thresholds PASS")));
    }

    /// Failing gate: every query errors, so `rate<0.05` and `count==0` are
    /// violated — the outcome fails and the summary JSON carries status,
    /// message (with the custom suffix) and structured violations.
    #[tokio::test]
    async fn thresholds_failing_gate_fails_the_run() {
        let after = vec![thresholds_step(
            json!({
                "db_query_failed": ["rate<0.05"],
                "db_errors": ["count==0"],
            }),
            None, // default severity: fail
            Some("checkout SLO"),
        )];
        let (lines, outcome) = run_native_full(
            sqlite_db_steps("SELECT * FROM missing_table"),
            Vec::new(),
            after,
            Map::new(),
            one_second(),
        )
        .await;

        let t = outcome.thresholds.as_ref().expect("gate outcome present");
        assert_eq!(t.status, "fail");
        assert!(outcome.thresholds_failed());
        assert_eq!(t.violations.len(), 2, "{:?}", t.violations);
        // rate over the derived db_query_failed samples is exactly
        // failed/total invocations — every invocation failed → 1.0.
        let rate_violation = t
            .violations
            .iter()
            .find(|v| v.metric == "db_query_failed")
            .unwrap();
        assert_eq!(rate_violation.expr, "rate<0.05");
        assert_eq!(rate_violation.actual, 1.0);
        assert!(t.message.contains("db_query_failed rate=1 ≥ 0.05"), "{}", t.message);
        assert!(t.message.contains("checkout SLO"), "{}", t.message);

        let tline = lines
            .iter()
            .find(|l| l.text.starts_with("thresholds: "))
            .expect("thresholds summary line");
        let parsed = crate::summary::parse_thresholds(&tline.text).unwrap();
        assert_eq!(parsed.status, "fail");
        assert_eq!(parsed.violations.len(), 2);
        assert!(
            lines
                .iter()
                .any(|l| l.text == "db_query_failed: 100.00%"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.text.contains("thresholds FAIL")),
            "{lines:?}"
        );
    }

    /// Warn gate: violations are reported but the run does not fail (exit
    /// zero semantics for CI advisories).
    #[tokio::test]
    async fn thresholds_warn_gate_does_not_fail_the_run() {
        let after = vec![thresholds_step(
            json!({ "db_errors": ["count==0"] }),
            Some("warn"),
            None,
        )];
        let (_lines, outcome) = run_native_full(
            sqlite_db_steps("SELECT * FROM missing_table"),
            Vec::new(),
            after,
            Map::new(),
            one_second(),
        )
        .await;

        let t = outcome.thresholds.as_ref().expect("gate outcome present");
        assert_eq!(t.status, "warn");
        assert!(!outcome.thresholds_failed(), "warn must exit zero");
    }

    /// A gate on a metric the run never emitted is a hard config error — it
    /// fails the run rather than silently passing.
    #[tokio::test]
    async fn thresholds_unknown_metric_fails_the_run() {
        let after = vec![thresholds_step(
            json!({ "no_such_metric": ["p95<1"] }),
            None,
            None,
        )];
        let (lines, outcome) = run_native_full(
            sqlite_db_steps("SELECT 1"),
            Vec::new(),
            after,
            Map::new(),
            one_second(),
        )
        .await;

        let t = outcome.thresholds.as_ref().expect("gate outcome present");
        assert_eq!(t.status, "fail");
        assert!(outcome.thresholds_failed());
        assert!(t.message.contains("unknown metric 'no_such_metric'"), "{}", t.message);
        // The error lists the metrics that ARE present.
        assert!(t.message.contains("db_query_duration"), "{}", t.message);
        assert!(
            lines.iter().any(|l| l.text.contains("unknown metric")),
            "{lines:?}"
        );
    }

    /// The `severity`/`message` step-level fields reach the action (merged
    /// into its params by the runner), and `message` is interpolated.
    #[tokio::test]
    async fn thresholds_step_level_severity_and_interpolated_message() {
        let mut vars = Map::new();
        vars.insert("gate_name".into(), json!("nightly SLO"));
        let after = vec![thresholds_step(
            json!({ "db_errors": ["count==0"] }),
            Some("info"),
            Some("${{ vars.gate_name }}"),
        )];
        let (_lines, outcome) = run_native_full(
            sqlite_db_steps("SELECT * FROM missing_table"),
            Vec::new(),
            after,
            vars,
            one_second(),
        )
        .await;

        let t = outcome.thresholds.as_ref().expect("gate outcome present");
        assert_eq!(t.status, "info");
        assert!(!outcome.thresholds_failed());
        assert!(t.message.contains("nightly SLO"), "{}", t.message);
    }
}

