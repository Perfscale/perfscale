//! During-run metrics streaming: snapshots out of the engine, batched
//! samples into the collector.
//!
//! # Data flow
//!
//! `run_native` (when its `metrics_tx` is `Some` and the run config's
//! `report.during_run` is set) sends a cumulative [`MetricSnapshot`] every
//! `report.interval_ms`. [`DuringRunShipper`] receives them, converts each to
//! [`Sample`]s via [`snapshot_to_samples`], batches, and POSTs
//! `{samples, taskId?, seq}` to `<target>/api/v1/metrics` — the endpoint the
//! end-of-run summary report already uses.
//!
//! # Semantics
//!
//! - **Cumulative**: engine HDR histograms and counters never reset during a
//!   run, so every snapshot carries since-run-start values. Consumers derive
//!   per-window numbers by diffing successive snapshots (diff `_count` /
//!   `_sum` for windowed averages, diff `_total` for rates). The **GPU
//!   samples riding the same snapshot are the exception**: they are
//!   point-in-time gauges, each carrying its own `ts` (the instant the GPU
//!   collector read the device), and every snapshot ships only the samples
//!   taken since the previous one — consumers chart them as-is, no diffing.
//! - **Units**: `MetricAgg` sample-kind values are milliseconds; the shipped
//!   quantile and `_sum` samples are **seconds**, Prometheus convention.
//! - **Naming** (Prometheus summary/counter conventions): a sample-kind
//!   metric `x` ships as `x{quantile="0.5"|"0.9"|"0.95"|"0.99"}` plus
//!   `x_count` and `x_sum`; a counter `y` ships as `y_total`; a rate metric
//!   `z` ships as `z_total` (invocations) and `z_failed_total` (failures).
//!   GPU gauges ship per device as `gpu_utilization_pct`,
//!   `gpu_memory_used_mib`, `gpu_memory_total_mib`, `gpu_temperature_c` and
//!   `gpu_power_w`, each with a `gpu="<index>"` label (plus any
//!   collector-specific extras under their own names).
//! - **Backpressure**: none, by design — the engine drops snapshots when the
//!   channel is full (the only mid-run shed), while the shipper queues
//!   snapshots through CPU gates and network outages rather than slow the
//!   VU loop.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::report::ReportRunConfig;
use crate::step::thresholds::{MetricAgg, MetricKind};

/// Per-request timeout for one batch POST. Batches are larger than the
/// end-of-run summary's `{"lines": …}` payload, so this is a bit roomier
/// than its 5s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// First retry delay after a failed POST; doubles per failure…
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// …up to this cap.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A point-in-time, cumulative view of every metric family the run collected.
///
/// Cumulative: the underlying HDR histograms and counters never reset during
/// a run, so values grow monotonically and percentiles converge. `ts_ms` is
/// strictly increasing across a run's snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSnapshot {
    /// Snapshot time, milliseconds since the Unix epoch.
    pub ts_ms: i64,
    /// Aggregates per metric name, as of `ts_ms`.
    pub metrics: BTreeMap<String, MetricAgg>,
    /// GPU gauge samples taken since the previous snapshot (empty when the
    /// run has no `gpu:` section). Unlike `metrics` these are NOT
    /// cumulative: each carries its own `ts_ms`, and consumers chart them
    /// directly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu: Vec<crate::gpu::GpuSample>,
}

/// One ingest sample, matching the controlplane's
/// `{metric, labels, value, ts}` shape.
///
/// Wire contract: `ts` is an **RFC3339 string** (the controlplane's
/// `MetricSampleInput.ts` is `Option<String>`; an integer would fail
/// ingestion with a 400). Internally we keep epoch milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub metric: String,
    pub labels: Map<String, Value>,
    pub value: f64,
    /// Sample time (`ts` on the wire, RFC3339).
    #[serde(
        rename = "ts",
        serialize_with = "ts_to_rfc3339",
        deserialize_with = "ts_from_wire"
    )]
    pub ts_ms: i64,
}

/// Serialize epoch milliseconds as RFC3339 (UTC, millisecond precision).
fn ts_to_rfc3339<S: serde::Serializer>(ts_ms: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&epoch_ms_to_rfc3339(*ts_ms))
}

/// Lenient read: RFC3339 string (the wire shape) or bare epoch millis.
fn ts_from_wire<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    if let Some(n) = v.as_i64() {
        return Ok(n);
    }
    let s = v
        .as_str()
        .ok_or_else(|| serde::de::Error::custom("ts must be RFC3339 string or epoch ms"))?;
    rfc3339_to_epoch_ms(s).map_err(serde::de::Error::custom)
}

/// Format epoch milliseconds as `YYYY-MM-DDTHH:MM:SS.sssZ` (UTC) without a
/// date library — Howard Hinnant's civil-from-days conversion.
fn epoch_ms_to_rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let day_secs = secs.rem_euclid(86_400);
    let (hour, min, sec) = (day_secs / 3600, day_secs % 3600 / 60, day_secs % 60);

    // days since epoch → civil date
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z")
}

/// Parse the fixed `…T HH:MM:SS(.sss)?Z` shape we emit (UTC only — that is
/// all the formatter above produces, and all the controlplane needs back).
fn rfc3339_to_epoch_ms(s: &str) -> Result<i64, String> {
    let bad = || format!("invalid ts {s:?}");
    let (date, time) = s.split_once('T').ok_or_else(bad)?;
    let time = time.strip_suffix('Z').ok_or_else(bad)?;
    let mut dp = date.split('-');
    let (y, mo, d): (i64, i64, i64) = (
        dp.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?,
        dp.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?,
        dp.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?,
    );
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let millis: i64 = frac.parse().unwrap_or(0);
    let mut tp = hms.split(':');
    let (h, mi, se): (i64, i64, i64) = (
        tp.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?,
        tp.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?,
        tp.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?,
    );
    // civil date → days since epoch (Hinnant's algorithm, reversed)
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj.rem_euclid(400);
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(days * 86_400_000 + h * 3_600_000 + mi * 60_000 + se * 1000 + millis)
}

/// Flatten a snapshot into ingest samples (see the module docs for naming
/// and unit conventions). `labels` are stamped on every sample.
pub fn snapshot_to_samples(snap: &MetricSnapshot, labels: &Map<String, Value>) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut sample = |metric: String, extra: Option<(&str, &str)>, value: f64| {
        let mut labels = labels.clone();
        if let Some((k, v)) = extra {
            labels.insert(k.to_string(), Value::String(v.to_string()));
        }
        out.push(Sample {
            metric,
            labels,
            value,
            ts_ms: snap.ts_ms,
        });
    };
    for (name, agg) in &snap.metrics {
        match agg.kind {
            // Milliseconds → seconds: MetricAgg sample values are ms, the
            // ingest convention is Prometheus seconds.
            MetricKind::Sample => {
                for (q, v) in [
                    ("0.5", agg.p50),
                    ("0.9", agg.p90),
                    ("0.95", agg.p95),
                    ("0.99", agg.p99),
                ] {
                    sample(name.clone(), Some(("quantile", q)), v / 1000.0);
                }
                sample(format!("{name}_count"), None, agg.count);
                sample(format!("{name}_sum"), None, agg.avg * agg.count / 1000.0);
            }
            MetricKind::Counter => sample(format!("{name}_total"), None, agg.count),
            MetricKind::Rate => {
                sample(format!("{name}_total"), None, agg.count);
                sample(format!("{name}_failed_total"), None, agg.rate * agg.count);
            }
        }
    }
    // GPU gauges: point-in-time, one series per device (`gpu="<index>"`),
    // each sample keeping the collector's own timestamp. `None` fields are
    // the source's "N/A" — skipped, never shipped as zeros. Pro-collector
    // extras ride under their own names, as in the summary timeseries.
    for g in &snap.gpu {
        let mut gpu_sample = |metric: &str, value: f64| {
            let mut labels = labels.clone();
            labels.insert("gpu".to_string(), Value::String(g.index.to_string()));
            out.push(Sample {
                metric: metric.to_string(),
                labels,
                value,
                ts_ms: g.ts_ms as i64,
            });
        };
        if let Some(v) = g.utilization_pct {
            gpu_sample("gpu_utilization_pct", v);
        }
        if let Some(v) = g.memory_used_mib {
            gpu_sample("gpu_memory_used_mib", v);
        }
        if let Some(v) = g.memory_total_mib {
            gpu_sample("gpu_memory_total_mib", v);
        }
        if let Some(v) = g.temperature_c {
            gpu_sample("gpu_temperature_c", v);
        }
        if let Some(v) = g.power_w {
            gpu_sample("gpu_power_w", v);
        }
        for (name, v) in &g.extra {
            gpu_sample(name, *v);
        }
    }
    out
}

/// Async bearer-token provider, called before every POST — e.g. the agent's
/// Keycloak token getter. Returning `None` sends that attempt
/// unauthenticated.
pub type TokenProvider = Arc<dyn Fn() -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// Bearer-token source for the shipper's POSTs.
pub enum Auth {
    /// A static token.
    Bearer(String),
    /// A provider called before every POST (token refresh for free).
    Provider(TokenProvider),
}

/// Batches [`MetricSnapshot`]s and POSTs them to `<target>/api/v1/metrics`.
///
/// Created with [`DuringRunShipper::new`], driven by
/// [`DuringRunShipper::spawn`] over the engine's snapshot channel. Behavior:
///
/// - **Batching**: snapshots accumulate into an open batch; it is sealed and
///   queued once it holds ≥ `batch_size` samples, and whatever is open is
///   sealed on every `interval_ms` tick.
/// - **Ordering / idempotency**: batches POST strictly in order; each sealed
///   batch gets a monotonically increasing `seq` that is reused across
///   retries, so the receiver can deduplicate redeliveries.
/// - **CPU gate**: if `max_cpu_percent > 0` and the reader reports a busy CPU
///   ≥ the limit, incoming snapshots are deferred — queued, not dropped —
///   and pending batches are held (no POSTs). Once the gate opens the
///   deferred snapshots are flushed first, in arrival order. A `None`
///   reading (non-Linux) leaves the gate inert.
/// - **Soft pending cap**: sealed-but-undelivered batches are never dropped
///   while the run is alive; growing past `max_pending` only logs a
///   rate-limited warning. Delivery resumes in order when the target
///   responds again.
/// - **Retries**: network errors, 5xx and 429 retry the same batch with
///   exponential backoff ×2 from 1s, capped at 60s. Any other 4xx marks the
///   batch poison — it is dropped with a warning, never retried.
/// - **Shutdown**: when the snapshot channel closes, everything still open
///   or pending is flushed with short bounded retries (sleeps of 1/2/5/10s),
///   then the task exits. The CPU gate does not apply to this final drain
///   (the VUs have stopped; the CPU is free).
pub struct DuringRunShipper {
    cfg: ReportRunConfig,
    target_url: String,
    /// Static labels plus the `task_id` label, merged once at construction.
    labels: Map<String, Value>,
    auth: Option<Auth>,
    task_id: Option<String>,
    cpu_reader: Arc<dyn Fn() -> Option<f64> + Send + Sync>,
}

/// A sealed batch awaiting delivery.
struct PendingBatch {
    seq: u64,
    samples: Vec<Sample>,
    /// Delay before the next retry; starts at [`INITIAL_BACKOFF`], doubles
    /// per failure up to [`MAX_BACKOFF`].
    backoff: Duration,
    /// Do not attempt delivery before this instant.
    not_before: Instant,
}

impl PendingBatch {
    fn new(seq: u64, samples: Vec<Sample>) -> Self {
        Self {
            seq,
            samples,
            backoff: INITIAL_BACKOFF,
            not_before: Instant::now(),
        }
    }
}

enum PostOutcome {
    Delivered,
    /// Network error, 5xx, or 429 — retry the same batch (same seq).
    Retry,
    /// Any other 4xx — the batch is poison; drop it.
    Poison(reqwest::StatusCode),
}

impl DuringRunShipper {
    /// `static_labels` are stamped on every sample (plus `task_id` as a
    /// `task_id` label when set). `cpu_reader` reports host busy CPU% —
    /// see [`crate::report::cpu::CpuReader`]; `None` readings make the gate
    /// inert.
    pub fn new(
        cfg: ReportRunConfig,
        target_url: String,
        static_labels: Map<String, Value>,
        auth: Option<Auth>,
        task_id: Option<String>,
        cpu_reader: impl Fn() -> Option<f64> + Send + Sync + 'static,
    ) -> Self {
        let mut labels = static_labels;
        if let Some(task_id) = &task_id {
            labels.insert("task_id".to_string(), Value::String(task_id.clone()));
        }
        Self {
            cfg,
            target_url,
            labels,
            auth,
            task_id,
            cpu_reader: Arc::new(cpu_reader),
        }
    }

    /// Run the shipper until the snapshot channel closes and the final drain
    /// completes.
    pub fn spawn(self, rx: mpsc::Receiver<MetricSnapshot>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run(rx))
    }

    async fn run(self, mut rx: mpsc::Receiver<MetricSnapshot>) {
        let client = reqwest::Client::new();
        let mut ticker = tokio::time::interval(self.cfg.interval());
        ticker.tick().await; // consume the immediate first tick

        let mut open: Vec<Sample> = Vec::new();
        let mut pending: VecDeque<PendingBatch> = VecDeque::new();
        // Snapshots that arrived while the CPU gate was closed — queued, not
        // dropped, flushed in arrival order once the gate opens.
        let mut deferred: VecDeque<MetricSnapshot> = VecDeque::new();
        let mut seq: u64 = 0;
        let mut gated_deferred: u64 = 0;
        let mut backlog_overflow: u64 = 0;

        loop {
            // Gate open again? Deferred snapshots go first so series keep
            // arrival order.
            if !self.cpu_gated() {
                while let Some(snap) = deferred.pop_front() {
                    self.absorb(
                        &snap,
                        &mut open,
                        &mut pending,
                        &mut seq,
                        &mut backlog_overflow,
                    );
                }
            }

            // While gated, the delivery arm stays off — recv/tick still wake
            // the loop, so there is no busy spin.
            let due = if self.cpu_gated() {
                None
            } else {
                pending.front().map(|b| b.not_before)
            };

            tokio::select! {
                maybe = rx.recv() => match maybe {
                    Some(snap) => {
                        if self.cpu_gated() {
                            gated_deferred += 1;
                            if gated_deferred % 10 == 1 {
                                tracing::warn!(
                                    gated_deferred,
                                    cpu_limit = self.cfg.max_cpu_percent,
                                    "cpu gate active — queueing metric snapshot"
                                );
                            }
                            deferred.push_back(snap);
                            continue;
                        }
                        // The gate may have opened since the loop-top check:
                        // deferred snapshots go first, keeping arrival order.
                        while let Some(old) = deferred.pop_front() {
                            self.absorb(
                                &old, &mut open, &mut pending, &mut seq, &mut backlog_overflow,
                            );
                        }
                        self.absorb(
                            &snap, &mut open, &mut pending, &mut seq, &mut backlog_overflow,
                        );
                    }
                    // Channel closed — final drain below.
                    None => break,
                },
                _ = ticker.tick() => {
                    if !open.is_empty() {
                        seq += 1;
                        pending.push_back(PendingBatch::new(seq, std::mem::take(&mut open)));
                        note_backlog(pending.len(), self.cfg.max_pending, &mut backlog_overflow);
                    }
                }
                // Delivery wakeup: fires when the head batch's retry comes
                // due. Disabled arms would still evaluate their expression
                // (select! semantics), so gate with a never-ready future.
                _ = async {
                    match due {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending().await,
                    }
                } => {}
            }

            // Deliver the oldest due batch. In-order only: seq is meaningful
            // strictly in sequence, so a retrying head blocks newer batches.
            if !self.cpu_gated()
                && pending
                    .front()
                    .is_some_and(|b| b.not_before <= Instant::now())
            {
                let mut batch = pending.pop_front().expect("front checked above");
                match self.post(&client, &batch).await {
                    PostOutcome::Delivered => {}
                    PostOutcome::Retry => {
                        batch.not_before = Instant::now() + batch.backoff;
                        batch.backoff = (batch.backoff * 2).min(MAX_BACKOFF);
                        pending.push_front(batch);
                    }
                    PostOutcome::Poison(status) => {
                        tracing::warn!(
                            seq = batch.seq,
                            %status,
                            "dropping undeliverable metrics batch (4xx)"
                        );
                    }
                }
            }
        }

        // Seal whatever is still open — after flushing the snapshots the CPU
        // gate deferred — and drain everything, best-effort.
        while let Some(snap) = deferred.pop_front() {
            self.absorb(
                &snap,
                &mut open,
                &mut pending,
                &mut seq,
                &mut backlog_overflow,
            );
        }
        if !open.is_empty() {
            seq += 1;
            pending.push_back(PendingBatch::new(seq, open));
        }
        self.final_drain(&client, pending).await;
    }

    /// Bounded final flush: each batch gets an initial attempt plus retries
    /// after 1/2/5/10s. A poisoned batch is dropped and the drain continues;
    /// a batch that exhausts its retries means the target is down, so the
    /// rest are dropped with one warning instead of sleeping through every
    /// remaining batch.
    async fn final_drain(&self, client: &reqwest::Client, pending: VecDeque<PendingBatch>) {
        const DRAIN_DELAYS: [Duration; 4] = [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(5),
            Duration::from_secs(10),
        ];
        let total = pending.len();
        for (i, batch) in pending.into_iter().enumerate() {
            let mut outcome = self.post(client, &batch).await;
            for delay in DRAIN_DELAYS {
                if !matches!(outcome, PostOutcome::Retry) {
                    break;
                }
                tokio::time::sleep(delay).await;
                outcome = self.post(client, &batch).await;
            }
            match outcome {
                PostOutcome::Delivered => {}
                PostOutcome::Poison(status) => {
                    tracing::warn!(
                        seq = batch.seq,
                        %status,
                        "dropping undeliverable metrics batch (4xx) during final drain"
                    );
                }
                PostOutcome::Retry => {
                    tracing::warn!(
                        seq = batch.seq,
                        remaining = total - i - 1,
                        "metrics target unreachable during final drain — dropping remaining batches"
                    );
                    return;
                }
            }
        }
    }

    async fn post(&self, client: &reqwest::Client, batch: &PendingBatch) -> PostOutcome {
        let endpoint = format!("{}/api/v1/metrics", self.target_url.trim_end_matches('/'));
        let mut body = serde_json::json!({
            "samples": batch.samples,
            "seq": batch.seq,
        });
        if let Some(task_id) = &self.task_id {
            body["taskId"] = Value::String(task_id.clone());
        }
        let mut req = client.post(&endpoint).json(&body).timeout(REQUEST_TIMEOUT);
        match &self.auth {
            Some(Auth::Bearer(token)) => req = req.bearer_auth(token),
            Some(Auth::Provider(provider)) => {
                if let Some(token) = provider().await {
                    req = req.bearer_auth(token);
                }
            }
            None => {}
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => PostOutcome::Delivered,
            Ok(resp) => {
                let status = resp.status();
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    PostOutcome::Retry
                } else {
                    PostOutcome::Poison(status)
                }
            }
            Err(_) => PostOutcome::Retry,
        }
    }

    /// True when the gate is configured and the reader reports a busy CPU at
    /// or above the limit. `None` readings (non-Linux) are inert.
    fn cpu_gated(&self) -> bool {
        if self.cfg.max_cpu_percent <= 0.0 {
            return false;
        }
        match (self.cpu_reader)() {
            Some(cpu) => cpu >= self.cfg.max_cpu_percent,
            None => false,
        }
    }

    /// Convert one snapshot into samples on the open batch, sealing and
    /// queueing a batch once it reaches `batch_size`.
    fn absorb(
        &self,
        snap: &MetricSnapshot,
        open: &mut Vec<Sample>,
        pending: &mut VecDeque<PendingBatch>,
        seq: &mut u64,
        backlog_overflow: &mut u64,
    ) {
        open.extend(snapshot_to_samples(snap, &self.labels));
        if open.len() >= self.cfg.batch_size.max(1) {
            *seq += 1;
            pending.push_back(PendingBatch::new(*seq, std::mem::take(open)));
            note_backlog(pending.len(), self.cfg.max_pending, backlog_overflow);
        }
    }
}

/// Warn when the sealed-but-undelivered backlog grows past the
/// `max_pending` soft cap — first crossing, then every 10th batch beyond.
/// Batches are never dropped while the run is alive; the cap only paces the
/// warning.
fn note_backlog(len: usize, max_pending: usize, overflow: &mut u64) {
    if len <= max_pending.max(1) {
        return;
    }
    *overflow += 1;
    if *overflow % 10 == 1 {
        tracing::warn!(
            backlog = len,
            max_pending,
            "metrics backlog past the soft cap — batches kept for delivery"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;

    fn test_cfg() -> ReportRunConfig {
        ReportRunConfig {
            during_run: true,
            interval_ms: 1000,
            batch_size: 500,
            // Gate off unless the test opts in.
            max_cpu_percent: 0.0,
            max_pending: 24,
            ..ReportRunConfig::default()
        }
    }

    /// Wire contract regression: the controlplane's MetricSampleInput takes
    /// `ts` as an RFC3339 STRING — an integer fails ingestion with a 400 and
    /// the batch is poisoned. Keep the format pinned.
    #[test]
    fn sample_ts_serializes_as_rfc3339_string() {
        let s = Sample {
            metric: "m".into(),
            labels: Map::new(),
            value: 1.0,
            ts_ms: 1_735_689_600_123,
        };
        let v = serde_json::to_value(&s).unwrap();
        let ts = v["ts"].as_str().expect("ts must serialize as a string");
        assert_eq!(ts, "2025-01-01T00:00:00.123Z", "{ts}");
        // Round trip, and a lenient integer read for robustness.
        let back: Sample = serde_json::from_value(v).unwrap();
        assert_eq!(back.ts_ms, s.ts_ms);
        let as_int: Sample = serde_json::from_value(serde_json::json!(
            {"metric": "m", "labels": {}, "value": 1.0, "ts": 42}
        ))
        .unwrap();
        assert_eq!(as_int.ts_ms, 42);
        // A couple of calendar anchors (leap-year boundary included).
        assert_eq!(epoch_ms_to_rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            epoch_ms_to_rfc3339(1_767_225_600_000),
            "2026-01-01T00:00:00.000Z"
        );
        assert_eq!(
            epoch_ms_to_rfc3339(1_740_693_600_000),
            "2025-02-27T22:00:00.000Z"
        );
        assert_eq!(
            rfc3339_to_epoch_ms("2025-02-27T22:00:00.000Z").unwrap(),
            1_740_693_600_000
        );
    }

    /// One snapshot with a single sample-kind metric → 6 samples
    /// (4 quantiles + `_count` + `_sum`).
    fn sample_snapshot(ts_ms: i64, count: f64) -> MetricSnapshot {
        let mut metrics = BTreeMap::new();
        metrics.insert(
            "http_req_duration".to_string(),
            MetricAgg {
                kind: MetricKind::Sample,
                avg: 10.0,
                min: 1.0,
                max: 50.0,
                p50: 8.0,
                p90: 20.0,
                p95: 30.0,
                p99: 40.0,
                count,
                rate: 0.0,
            },
        );
        MetricSnapshot {
            ts_ms,
            metrics,
            gpu: Vec::new(),
        }
    }

    fn spawn_shipper(
        server: &MockServer,
        cfg: ReportRunConfig,
        cpu: impl Fn() -> Option<f64> + Send + Sync + 'static,
    ) -> (mpsc::Sender<MetricSnapshot>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(512);
        let shipper = DuringRunShipper::new(cfg, server.uri(), Map::new(), None, None, cpu);
        (tx, shipper.spawn(rx))
    }

    /// Poll the server's recorded requests until `n` arrive or time out.
    async fn wait_requests(server: &MockServer, n: usize, timeout: Duration) -> Vec<Request> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let reqs = server.received_requests().await.unwrap_or_default();
            if reqs.len() >= n || std::time::Instant::now() >= deadline {
                return reqs;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn bodies(reqs: &[Request]) -> Vec<Value> {
        reqs.iter()
            .map(|r| serde_json::from_slice(&r.body).expect("batch body is JSON"))
            .collect()
    }

    // -------------------------------------------------------------
    // snapshot_to_samples
    // -------------------------------------------------------------

    #[test]
    fn snapshot_to_samples_maps_all_three_kinds() {
        let mut metrics = BTreeMap::new();
        metrics.insert(
            "lat".to_string(),
            MetricAgg {
                kind: MetricKind::Sample,
                avg: 10.0,
                min: 1.0,
                max: 50.0,
                p50: 8.0,
                p90: 20.0,
                p95: 30.0,
                p99: 40.0,
                count: 4.0,
                rate: 0.0,
            },
        );
        metrics.insert("rows".to_string(), MetricAgg::counter(7.0));
        metrics.insert("req_failed".to_string(), MetricAgg::rate(10, 2));
        let snap = MetricSnapshot {
            ts_ms: 42,
            metrics,
            gpu: Vec::new(),
        };
        let mut labels = Map::new();
        labels.insert("run".to_string(), Value::String("r1".into()));

        let samples = snapshot_to_samples(&snap, &labels);
        // 6 (lat) + 1 (rows_total) + 2 (req_failed_*) = 9.
        assert_eq!(samples.len(), 9);
        let find = |metric: &str, quantile: Option<&str>| {
            samples
                .iter()
                .find(|s| {
                    s.metric == metric
                        && s.labels.get("quantile").and_then(Value::as_str) == quantile
                })
                .unwrap_or_else(|| panic!("sample {metric}/{quantile:?} present"))
        };

        // Quantile series in SECONDS (ms → s conversion), static labels on.
        assert_eq!(find("lat", Some("0.5")).value, 0.008);
        assert_eq!(find("lat", Some("0.9")).value, 0.020);
        assert_eq!(find("lat", Some("0.95")).value, 0.030);
        assert_eq!(find("lat", Some("0.99")).value, 0.040);
        let p50 = find("lat", Some("0.5"));
        assert_eq!(p50.ts_ms, 42);
        assert_eq!(p50.labels["run"], "r1");
        // count as-is, sum = avg * count in seconds.
        assert_eq!(find("lat_count", None).value, 4.0);
        assert_eq!(find("lat_sum", None).value, 0.040);

        assert_eq!(find("rows_total", None).value, 7.0);
        assert_eq!(find("req_failed_total", None).value, 10.0);
        assert_eq!(find("req_failed_failed_total", None).value, 2.0);
    }

    /// GPU gauges ride the same snapshot as per-device series with the
    /// collector's own timestamps — point-in-time, never cumulative, and
    /// `N/A` fields are skipped rather than shipped as zeros.
    #[test]
    fn snapshot_to_samples_streams_gpu_gauges_per_device() {
        let mut g0 = crate::gpu::GpuSample {
            ts_ms: 7_000,
            index: 0,
            utilization_pct: Some(73.0),
            memory_used_mib: Some(8192.0),
            memory_total_mib: Some(24576.0),
            temperature_c: Some(58.0),
            power_w: Some(210.5),
            extra: std::collections::BTreeMap::new(),
        };
        g0.extra.insert("pro_gpu_clocks_sm_mhz".to_string(), 1980.0);
        // Device 1 reports N/A for temperature and power (virtualized GPUs
        // do) — those series must not appear for it at all.
        let g1 = crate::gpu::GpuSample {
            ts_ms: 7_005,
            index: 1,
            utilization_pct: Some(3.0),
            memory_used_mib: Some(512.0),
            memory_total_mib: None,
            temperature_c: None,
            power_w: None,
            extra: std::collections::BTreeMap::new(),
        };
        let snap = MetricSnapshot {
            ts_ms: 8_000,
            metrics: BTreeMap::new(),
            gpu: vec![g0, g1],
        };
        let mut labels = Map::new();
        labels.insert("task_id".to_string(), Value::String("t1".into()));

        let samples = snapshot_to_samples(&snap, &labels);
        // g0: 5 built-ins + 1 extra; g1: 2 built-ins (util + mem used).
        assert_eq!(samples.len(), 8, "{samples:?}");
        let find = |metric: &str, gpu: &str| {
            samples
                .iter()
                .find(|s| {
                    s.metric == metric && s.labels.get("gpu").and_then(Value::as_str) == Some(gpu)
                })
                .unwrap_or_else(|| panic!("sample {metric}/gpu{gpu} present: {samples:?}"))
        };

        let u0 = find("gpu_utilization_pct", "0");
        assert_eq!(u0.value, 73.0);
        assert_eq!(u0.ts_ms, 7_000, "GPU samples keep the collector's ts");
        assert_eq!(u0.labels["task_id"], "t1", "static labels still stamped");
        assert_eq!(find("gpu_memory_used_mib", "0").value, 8192.0);
        assert_eq!(find("gpu_memory_total_mib", "0").value, 24576.0);
        assert_eq!(find("gpu_temperature_c", "0").value, 58.0);
        assert_eq!(find("gpu_power_w", "0").value, 210.5);
        assert_eq!(find("pro_gpu_clocks_sm_mhz", "0").value, 1980.0);

        let u1 = find("gpu_utilization_pct", "1");
        assert_eq!(u1.value, 3.0);
        assert_eq!(u1.ts_ms, 7_005);
        assert!(
            samples.iter().all(
                |s| !(s.labels.get("gpu").and_then(Value::as_str) == Some("1")
                    && (s.metric == "gpu_temperature_c"
                        || s.metric == "gpu_power_w"
                        || s.metric == "gpu_memory_total_mib"))
            ),
            "N/A fields are skipped, not shipped: {samples:?}"
        );
    }

    #[test]
    fn sample_serializes_to_the_ingest_shape() {
        let mut labels = Map::new();
        labels.insert("a".to_string(), Value::from(1));
        let s = Sample {
            metric: "x".into(),
            labels,
            value: 0.5,
            ts_ms: 7,
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"metric": "x", "labels": {"a": 1}, "value": 0.5, "ts": "1970-01-01T00:00:00.007Z"})
        );
        let back: Sample = serde_json::from_value(json).unwrap();
        assert_eq!(back, s);
    }

    // -------------------------------------------------------------
    // Shipper: batching / flush
    // -------------------------------------------------------------

    #[tokio::test]
    async fn shipper_seals_a_batch_at_batch_size_samples() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let cfg = ReportRunConfig {
            batch_size: 6, // one snapshot fills a batch exactly
            ..test_cfg()
        };
        let (tx, _handle) = spawn_shipper(&server, cfg, || None);
        tx.send(sample_snapshot(1000, 4.0)).await.unwrap();
        tx.send(sample_snapshot(2000, 9.0)).await.unwrap();

        let reqs = wait_requests(&server, 2, Duration::from_secs(3)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 2, "one POST per full batch");
        assert_eq!(bodies[0]["seq"], 1);
        assert_eq!(bodies[1]["seq"], 2);
        assert_eq!(bodies[0]["samples"].as_array().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn shipper_flushes_open_batch_on_interval_tick() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // batch_size far above the snapshot's 6 samples → only the 1s tick
        // can flush it.
        let (tx, _handle) = spawn_shipper(&server, test_cfg(), || None);
        tx.send(sample_snapshot(1000, 4.0)).await.unwrap();

        let reqs = wait_requests(&server, 1, Duration::from_secs(3)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 1, "tick flushed the open batch");
        assert_eq!(bodies[0]["seq"], 1);
        assert_eq!(bodies[0]["samples"].as_array().unwrap().len(), 6);
        // No taskId field when the shipper has no task id.
        assert!(bodies[0].get("taskId").is_none());
    }

    #[tokio::test]
    async fn shipper_stamps_task_id_in_ledger_field_and_label() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let (tx, rx) = mpsc::channel(512);
        let shipper = DuringRunShipper::new(
            ReportRunConfig {
                batch_size: 1,
                ..test_cfg()
            },
            server.uri(),
            Map::new(),
            None,
            Some("task-9".to_string()),
            || None,
        );
        let _handle = shipper.spawn(rx);
        tx.send(sample_snapshot(1000, 4.0)).await.unwrap();

        let reqs = wait_requests(&server, 1, Duration::from_secs(3)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies[0]["taskId"], "task-9");
        assert_eq!(
            bodies[0]["samples"][0]["labels"]["task_id"],
            Value::String("task-9".into())
        );
    }

    // -------------------------------------------------------------
    // Shipper: CPU gate
    // -------------------------------------------------------------

    #[tokio::test]
    async fn cpu_gate_defers_snapshots_and_ships_them_on_recovery() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let cpu = Arc::new(AtomicU64::new(95));
        let reader = {
            let cpu = Arc::clone(&cpu);
            move || Some(cpu.load(Ordering::Relaxed) as f64)
        };
        let cfg = ReportRunConfig {
            batch_size: 1,
            max_cpu_percent: 90.0,
            ..test_cfg()
        };
        let (tx, _handle) = spawn_shipper(&server, cfg, reader);

        // Gated: the snapshot is queued, nothing is POSTed.
        tx.send(sample_snapshot(1000, 4.0)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "gated shipper must not POST"
        );

        // Recovered: the deferred snapshot ships first, then the fresh one —
        // nothing is dropped and arrival order is preserved.
        cpu.store(10, Ordering::Relaxed);
        tx.send(sample_snapshot(2000, 9.0)).await.unwrap();
        let reqs = wait_requests(&server, 2, Duration::from_secs(3)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 2, "deferred + fresh snapshot both ship");
        assert_eq!(bodies[0]["seq"], 1);
        assert!(
            bodies[0]["samples"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["ts"] == "1970-01-01T00:00:01.000Z"),
            "the snapshot gated at ts=1s must ship after recovery: {}",
            bodies[0]["samples"]
        );
        assert_eq!(bodies[1]["seq"], 2);
        assert!(
            bodies[1]["samples"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["ts"] == "1970-01-01T00:00:02.000Z"),
            "the post-recovery snapshot ships second: {}",
            bodies[1]["samples"]
        );
    }

    #[tokio::test]
    async fn cpu_gate_holds_pending_batches_until_recovery() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let cpu = Arc::new(AtomicU64::new(10));
        let reader = {
            let cpu = Arc::clone(&cpu);
            move || Some(cpu.load(Ordering::Relaxed) as f64)
        };
        let cfg = ReportRunConfig {
            batch_size: 1,
            max_cpu_percent: 90.0,
            ..test_cfg()
        };
        let (tx, _handle) = spawn_shipper(&server, cfg, reader);

        // First attempt fails (500) → batch pending with a 1s backoff.
        tx.send(sample_snapshot(1000, 4.0)).await.unwrap();
        let reqs = wait_requests(&server, 1, Duration::from_secs(3)).await;
        assert_eq!(reqs.len(), 1);

        // Gate on: the backoff expires while gated — the retry must be held.
        cpu.store(95, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            1,
            "gated shipper must hold pending batches"
        );

        // Gate off: the held batch is retried (same seq).
        cpu.store(10, Ordering::Relaxed);
        let reqs = wait_requests(&server, 2, Duration::from_secs(4)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["seq"], 1);
        assert_eq!(bodies[1]["seq"], 1, "retry reuses the batch's seq");
    }

    // -------------------------------------------------------------
    // Shipper: backlog / retries / drain
    // -------------------------------------------------------------

    #[tokio::test]
    async fn shipper_keeps_all_batches_past_the_pending_soft_cap() {
        let server = MockServer::start().await;
        // The first POST fails; everything after it succeeds.
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(500))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let cfg = ReportRunConfig {
            batch_size: 1,
            max_pending: 2, // soft cap — exceeded, yet nothing is dropped
            ..test_cfg()
        };
        let (tx, _handle) = spawn_shipper(&server, cfg, || None);

        // Four snapshots → four sealed batches. The first POST 500s, so
        // seqs 2–4 pile up behind the retrying head — past the soft cap.
        for i in 1..=4 {
            tx.send(sample_snapshot(i * 1000, 4.0)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // 1 failed attempt + 4 deliveries: every batch is kept and lands in
        // order once the target responds (the retry reuses seq 1).
        let reqs = wait_requests(&server, 5, Duration::from_secs(5)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 5, "retry + all four batches: {bodies:?}");
        let seqs: Vec<u64> = bodies.iter().map(|b| b["seq"].as_u64().unwrap()).collect();
        assert_eq!(
            seqs,
            vec![1, 1, 2, 3, 4],
            "no drops past the soft cap, in-order delivery"
        );
    }

    #[tokio::test]
    async fn final_drain_flushes_snapshots_deferred_by_the_cpu_gate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // The gate never opens during the run…
        let cfg = ReportRunConfig {
            max_cpu_percent: 90.0,
            ..test_cfg()
        };
        let (tx, handle) = spawn_shipper(&server, cfg, || Some(95.0));
        tx.send(sample_snapshot(1000, 4.0)).await.unwrap();
        tx.send(sample_snapshot(2000, 9.0)).await.unwrap();
        drop(tx);

        // …but the final drain ignores the gate (the VUs have stopped), so
        // the deferred snapshots still ship.
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("shipper exits after the final drain")
            .unwrap();
        let reqs = server.received_requests().await.unwrap_or_default();
        let bodies = bodies(&reqs);
        assert_eq!(
            bodies.len(),
            1,
            "deferred snapshots sealed into one batch: {bodies:?}"
        );
        assert_eq!(bodies[0]["samples"].as_array().unwrap().len(), 12);
    }

    #[tokio::test]
    async fn shipper_retries_5xx_with_the_same_seq() {
        let server = MockServer::start().await;
        // wiremock matches lowest priority number first, stable within a
        // priority: one 500 (spent after one match), then 200s.
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(500))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let cfg = ReportRunConfig {
            batch_size: 1,
            ..test_cfg()
        };
        let (tx, _handle) = spawn_shipper(&server, cfg, || None);
        tx.send(sample_snapshot(1000, 4.0)).await.unwrap();

        let reqs = wait_requests(&server, 2, Duration::from_secs(4)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 2, "500 → backoff → retry");
        assert_eq!(bodies[0]["seq"], 1);
        assert_eq!(bodies[1]["seq"], 1, "same seq across retries");
    }

    #[tokio::test]
    async fn shipper_drops_4xx_poison_batch_without_retrying() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;

        let cfg = ReportRunConfig {
            batch_size: 1,
            ..test_cfg()
        };
        let (tx, _handle) = spawn_shipper(&server, cfg, || None);
        tx.send(sample_snapshot(1000, 4.0)).await.unwrap();

        // Past the first backoff step: a poison batch must not reappear.
        let reqs = wait_requests(&server, 1, Duration::from_secs(3)).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            1,
            "4xx is poison — dropped, never retried"
        );
    }

    #[tokio::test]
    async fn shipper_drains_everything_when_the_channel_closes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // batch_size above the total: only the channel close can flush.
        let (tx, handle) = spawn_shipper(&server, test_cfg(), || None);
        tx.send(sample_snapshot(1000, 4.0)).await.unwrap();
        tx.send(sample_snapshot(2000, 9.0)).await.unwrap();
        drop(tx);

        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("shipper exits after the final drain")
            .unwrap();
        let reqs = server.received_requests().await.unwrap_or_default();
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 1, "open snapshots sealed into one batch");
        assert_eq!(bodies[0]["seq"], 1);
        assert_eq!(bodies[0]["samples"].as_array().unwrap().len(), 12);
    }
}
