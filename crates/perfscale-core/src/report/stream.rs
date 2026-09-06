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
//!   `_sum` for windowed averages, diff `_total` for rates).
//! - **Units**: `MetricAgg` sample-kind values are milliseconds; the shipped
//!   quantile and `_sum` samples are **seconds**, Prometheus convention.
//! - **Naming** (Prometheus summary/counter conventions): a sample-kind
//!   metric `x` ships as `x{quantile="0.5"|"0.9"|"0.95"|"0.99"}` plus
//!   `x_count` and `x_sum`; a counter `y` ships as `y_total`; a rate metric
//!   `z` ships as `z_total` (invocations) and `z_failed_total` (failures).
//! - **Backpressure**: none, by design — the engine drops snapshots when the
//!   channel is full, and the shipper sheds load (CPU gate, bounded pending)
//!   rather than slow the VU loop.

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
}

/// One ingest sample, matching the controlplane's
/// `{metric, labels, value, ts}` shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub metric: String,
    pub labels: Map<String, Value>,
    pub value: f64,
    /// Sample time, milliseconds since the Unix epoch (`ts` on the wire).
    #[serde(rename = "ts")]
    pub ts_ms: i64,
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
///   ≥ the limit, incoming snapshots are dropped (every 10th logs a warning)
///   and pending batches are held — no POSTs. A `None` reading (non-Linux)
///   leaves the gate inert.
/// - **Bounded pending**: sealed-but-undelivered batches are capped at
///   `max_pending`; beyond that the oldest is dropped with a warning.
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
        let mut seq: u64 = 0;
        let mut gated_drops: u64 = 0;

        loop {
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
                            gated_drops += 1;
                            if gated_drops % 10 == 1 {
                                tracing::warn!(
                                    gated_drops,
                                    cpu_limit = self.cfg.max_cpu_percent,
                                    "cpu gate active — dropping metric snapshot"
                                );
                            }
                            continue;
                        }
                        open.extend(snapshot_to_samples(&snap, &self.labels));
                        if open.len() >= self.cfg.batch_size.max(1) {
                            seq += 1;
                            pending.push_back(PendingBatch::new(seq, std::mem::take(&mut open)));
                            trim_pending(&mut pending, self.cfg.max_pending);
                        }
                    }
                    // Channel closed — final drain below.
                    None => break,
                },
                _ = ticker.tick() => {
                    if !open.is_empty() {
                        seq += 1;
                        pending.push_back(PendingBatch::new(seq, std::mem::take(&mut open)));
                        trim_pending(&mut pending, self.cfg.max_pending);
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

        // Seal whatever is still open and flush everything, best-effort.
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
}

/// Cap the sealed-but-undelivered backlog, dropping the oldest first.
fn trim_pending(pending: &mut VecDeque<PendingBatch>, max_pending: usize) {
    let max = max_pending.max(1);
    while pending.len() > max {
        let dropped = pending.pop_front().expect("len checked above");
        tracing::warn!(
            seq = dropped.seq,
            max_pending = max,
            "pending metrics batches full — dropping oldest"
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
        MetricSnapshot { ts_ms, metrics }
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
        let snap = MetricSnapshot { ts_ms: 42, metrics };
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
            serde_json::json!({"metric": "x", "labels": {"a": 1}, "value": 0.5, "ts": 7})
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
    async fn cpu_gate_drops_snapshots_and_recovers() {
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

        // Gated: the snapshot is dropped, nothing is POSTed.
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

        // Recovered: only the post-recovery snapshot is shipped.
        cpu.store(10, Ordering::Relaxed);
        tx.send(sample_snapshot(2000, 9.0)).await.unwrap();
        let reqs = wait_requests(&server, 1, Duration::from_secs(3)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 1);
        assert!(
            bodies[0]["samples"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["ts"] == 2000),
            "dropped (gated) snapshot must never be shipped: {}",
            bodies[0]["samples"]
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
    // Shipper: shedding / retries / drain
    // -------------------------------------------------------------

    #[tokio::test]
    async fn shipper_drops_oldest_batch_when_pending_is_full() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let cfg = ReportRunConfig {
            batch_size: 1,
            max_pending: 2,
            ..test_cfg()
        };
        let (tx, _handle) = spawn_shipper(&server, cfg, || None);

        // Four snapshots → four sealed batches; the backlog caps at 2, so
        // seq 1 and 2 are shed before their retries come due, and in-order
        // delivery keeps seq 4 stuck behind the retrying seq 3.
        for i in 1..=4 {
            tx.send(sample_snapshot(i * 1000, 4.0)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let _ = wait_requests(&server, 4, Duration::from_secs(4)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let reqs = server.received_requests().await.unwrap_or_default();
        assert!(reqs.len() >= 4, "expected retries, got {}", reqs.len());
        let count_seq = |seq: u64| bodies(&reqs).iter().filter(|b| b["seq"] == seq).count();
        assert_eq!(count_seq(1), 1, "seq 1 attempted once, then dropped");
        assert_eq!(count_seq(2), 1, "seq 2 attempted once, then dropped");
        assert!(count_seq(3) >= 2, "seq 3 kept and retried");
        assert_eq!(count_seq(4), 0, "seq 4 waits behind the retrying head");
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
