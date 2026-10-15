//! The run metrics store: every step's timing and failure folds into one
//! shared [`Metrics`] accumulator.
//!
//! Durations live in a fixed-size HDR histogram (~tens of KB) instead of one
//! f64 per request: storing raw samples made memory grow 8 bytes per request
//! — a 30-hour soak at 10k RPS would have needed ~26 GB at the final
//! clone-and-sort. The histogram trades that for a ≤1% quantile error,
//! invisible at the 2-decimal precision the summary prints.
//!
//! Presentation (the end-of-run summary and the periodic `[stats]` line) is
//! family-driven: the generic loops below render every unregistered name in
//! the historical k6-compatible shapes, and each registered
//! [`MetricsFamily`](super::MetricsFamily) appends its own lines/extras after
//! them. The generic fallback is byte-stable — external consumers (the
//! controlplane's stats parser, `crate::summary`) parse these lines with
//! fixed keys, and the golden tests in `step::runner::tests` pin the exact
//! output.

use serde_json::{Map, Value};

/// Durations are tracked in microseconds: 1µs floor keeps sub-millisecond
/// loopback requests distinguishable, the 1-hour ceiling is far beyond any
/// sane single request.
const HIST_LOW_MICROS: u64 = 1;
const HIST_HIGH_MICROS: u64 = 3_600_000_000;
/// Two significant digits → quantiles within ≤1% of the true value.
const HIST_SIGFIGS: u8 = 2;

/// Per-run HTTP metrics accumulator.
///
/// Public only so `benches/` can exercise the hot paths (`record`, quantile
/// computation in `summary_lines`) — not part of the supported API surface.
#[doc(hidden)]
#[derive(Debug)]
pub struct Metrics {
    pub(crate) durations_micros: hdrhistogram::Histogram<u64>,
    pub(crate) failures: u64,
    pub(crate) total: u64,
    /// Custom named counters contributed by actions via the reserved
    /// `metrics` key of their output value — e.g. `pro/fix@v1` emits
    /// `fix_messages_sent`. Summed across VUs/iterations, then reported as
    /// `<name>: <total> <rate>/s` so the same downstream parser handles them.
    pub(crate) counters: std::collections::BTreeMap<String, f64>,
    /// Custom named histograms: an action reports duration samples (ms) as a
    /// JSON *array* under the same reserved `metrics` key — e.g. `std/ws@v1`
    /// emits `ws_msg_rtt: [12.3, 15.1]`. Aggregated with the same HDR
    /// settings as the request-duration histogram and summarised in the same
    /// `avg/p(..)/min/max` shape, plus a sample count.
    pub(crate) hists: std::collections::BTreeMap<String, hdrhistogram::Histogram<u64>>,
    /// Per-invocation failure samples as `(total, failed)` pairs, recorded by
    /// the runner for every histogram metric a step emits (see the runner's
    /// step fold): `db_query_duration` → `db_query_failed`. These are NOT
    /// stored in `hists` — the HDR histogram clamps samples to ≥1µs, which
    /// would turn every 0 (success) into a 1 (failure). Summarised as
    /// `<name>: <pct>%` like k6's `http_req_failed`, and the source of the
    /// `rate` aggregate for `std/thresholds@v1`.
    pub(crate) rates: std::collections::BTreeMap<String, (u64, u64)>,
    /// Point-in-time values ([`MetricKind::Gauge`](super::MetricKind::Gauge))
    /// — last write wins, unlike counters. What engine self-observability
    /// (`engine_metrics:`) uses for `engine_log_channel_depth_max`. Rendered
    /// as `<name>: <value>` — no `/s` rate, a gauge is not a flow.
    pub(crate) gauges: std::collections::BTreeMap<String, f64>,
    /// Outcomes of `std/thresholds@v1` steps (run in `after:`), combined into
    /// the run summary's `thresholds` field at the end of the run.
    pub(crate) threshold_results: Vec<crate::step::thresholds::ThresholdsSummary>,
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
            gauges: std::collections::BTreeMap::new(),
            threshold_results: Vec::new(),
        }
    }
}

impl Metrics {
    pub fn record(&mut self, s: &crate::step::actions::HttpSample) {
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
    pub fn add_counters(&mut self, obj: &Map<String, Value>) {
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

    /// Record one duration sample (milliseconds) into a named histogram —
    /// the programmatic equivalent of emitting `[ms]` under a step's
    /// `metrics` key. Engine-internal producers (the `engine_metrics:` probe)
    /// use it directly instead of building a throwaway JSON object per tick.
    pub(crate) fn record_hist_sample(&mut self, name: &str, ms: f64) {
        let h = self.hists.entry(name.to_string()).or_insert_with(|| {
            hdrhistogram::Histogram::new_with_bounds(
                HIST_LOW_MICROS,
                HIST_HIGH_MICROS,
                HIST_SIGFIGS,
            )
            .expect("static histogram bounds are valid")
        });
        let micros = (ms * 1000.0).round() as u64;
        let _ = h.record(micros.clamp(HIST_LOW_MICROS, HIST_HIGH_MICROS));
    }

    /// Set a gauge's current value — last write wins, unlike
    /// [`Metrics::add_counters`]' summing.
    pub(crate) fn set_gauge(&mut self, name: &str, value: f64) {
        self.gauges.insert(name.to_string(), value);
    }

    /// Fold another store's contents into this one — the per-shard merge
    /// behind [`super::sharded::ShardedMetrics::merged`]. Exact, not
    /// approximate: the histograms share bounds and significant figures, so
    /// HDR `add` sums identical bucket layouts; counters/rates sum; gauges
    /// take the max (the gauges in use are peak readings like
    /// `engine_log_channel_depth_max`); threshold outcomes concatenate.
    pub(crate) fn merge_from(&mut self, other: &Metrics) {
        self.durations_micros
            .add(&other.durations_micros)
            .expect("all run histograms share the same bounds");
        self.failures += other.failures;
        self.total += other.total;
        for (name, v) in &other.counters {
            *self.counters.entry(name.clone()).or_insert(0.0) += v;
        }
        for (name, h) in &other.hists {
            self.hists
                .entry(name.clone())
                .or_insert_with(|| {
                    hdrhistogram::Histogram::new_with_bounds(
                        HIST_LOW_MICROS,
                        HIST_HIGH_MICROS,
                        HIST_SIGFIGS,
                    )
                    .expect("static histogram bounds are valid")
                })
                .add(h)
                .expect("all run histograms share the same bounds");
        }
        for (name, (total, failed)) in &other.rates {
            let e = self.rates.entry(name.clone()).or_insert((0, 0));
            e.0 += total;
            e.1 += failed;
        }
        for (name, v) in &other.gauges {
            let e = self.gauges.entry(name.clone()).or_insert(f64::MIN);
            *e = e.max(*v);
        }
        self.threshold_results
            .extend(other.threshold_results.iter().cloned());
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
        // Gauges surface as counters in the snapshot — a threshold like
        // `engine_log_channel_depth_max < 400` reads the latest value.
        for (name, v) in &self.gauges {
            out.insert(name.clone(), MetricAgg::counter(*v));
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
        self.summary_lines_inner(
            wall_secs,
            total_iters,
            format!("vus....................: {vus} min=1 max={vus}"),
        )
    }

    /// Summary for staged/arrival-rate runs: the `vus` line reports the
    /// *observed* concurrency (last sample plus min/max over the run) instead
    /// of a fixed configured count. Everything else is identical to
    /// [`Metrics::summary_lines`].
    pub fn summary_lines_observed(
        &self,
        wall_secs: f64,
        total_iters: u64,
        last_vus: u64,
        min_vus: u64,
        max_vus: u64,
    ) -> Vec<String> {
        self.summary_lines_inner(
            wall_secs,
            total_iters,
            format!("vus....................: {last_vus} min={min_vus} max={max_vus}"),
        )
    }

    fn summary_lines_inner(
        &self,
        wall_secs: f64,
        total_iters: u64,
        vus_line: String,
    ) -> Vec<String> {
        // Always emit iteration stats (even with no HTTP requests) so
        // downstream parsers can extract metrics from sleep-only runs.
        let iter_rate = total_iters as f64 / wall_secs.max(0.001);
        let mut lines = vec![
            vus_line,
            format!("iterations..............: {total_iters} {iter_rate:.2}/s"),
        ];

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

        // Gauges (e.g. `engine_log_channel_depth_max`) — a bare value, no
        // rate: a point-in-time reading is not a flow.
        for (name, value) in &self.gauges {
            lines.push(format!("{name}: {value:.0}"));
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

        // Family-owned blocks (RFC 010): the built-in `http` family renders
        // the `http_req_*` triple here — after the generic lines, exactly
        // where the hardcoded block used to sit. Families with the default
        // (empty) rendering add nothing, so unregistered metrics keep the
        // byte-identical generic shapes above.
        for family in super::families() {
            lines.extend(family.summary_lines(self, wall_secs));
        }
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
    /// Families append their own fields via
    /// [`MetricsFamily::stats_extras`](super::MetricsFamily::stats_extras) —
    /// the built-in `llm` family adds `llm_reqs` (completed LLM requests) and
    /// `llm_tok_s` (mean completion throughput) when any LLM throughput
    /// samples were recorded, so LLM load tests still get a live signal even
    /// though `std/llm@v1` never produces `HttpSample`s (`reqs`/`rps` stay 0
    /// on pure-LLM runs).
    ///
    /// ```text
    /// [stats] ts=1720000000000 rps=246.80 err_pct=0.00 p50=1.20 p90=3.40 p95=4.10 p99=8.20 reqs=1234 iters=456
    /// [stats] ts=1720000000000 rps=0.00 reqs=0 iters=12 llm_reqs=9 llm_tok_s=38.7
    /// ```
    pub fn stats_line(&self, ts_ms: u64, window_reqs: u64, window_secs: f64, iters: u64) -> String {
        let rps = window_reqs as f64 / window_secs.max(0.001);
        let mut line = if self.total == 0 {
            format!("[stats] ts={ts_ms} rps={rps:.2} reqs=0 iters={iters}")
        } else {
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
        };
        for family in super::families() {
            line.push_str(&family.stats_extras(self));
        }
        line
    }
}
