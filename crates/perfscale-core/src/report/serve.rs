//! The `std/report/serve@v1` streaming reporter: batches snapshots into
//! [`Sample`]s and POSTs them to `<url>/api/v1/metrics` — the endpoint the
//! end-of-run summary report already uses, and the historical
//! `report.url + during_run` streaming path (issue #12 made it a reporter).
//!
//! Delivery (batching, bounded drop-oldest queues, retries, final flush)
//! lives in [`super::delivery`]; this file is only the encode/POST logic.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use super::delivery::{self, PostOutcome, Sink};
use super::stream::{snapshot_to_samples, MetricSnapshot, Sample};
use super::ReportRunConfig;

/// Per-request timeout for one batch POST. Batches are larger than the
/// end-of-run summary's `{"lines": …}` payload, so this is a bit roomier
/// than its 5s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Async bearer-token provider, called before every POST — e.g. the agent's
/// Keycloak token getter. Returning `None` sends that attempt
/// unauthenticated.
pub type TokenProvider = Arc<dyn Fn() -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// Bearer-token source for the reporter's POSTs.
pub enum Auth {
    /// A static token.
    Bearer(String),
    /// A provider called before every POST (token refresh for free).
    Provider(TokenProvider),
}

/// The serve sink: JSON batches to `<url>/api/v1/metrics`.
pub struct ServeSink {
    url: String,
    /// Reporter-level labels (reporter `labels:` plus the legacy
    /// constructor's static labels); run labels from the snapshot merge
    /// underneath, `task_id` stamps on top.
    labels: Map<String, Value>,
    auth: Option<Auth>,
    task_id: Option<String>,
    max_series: usize,
    /// Counter/log identity — `std/report/serve@v1` in production; tests
    /// override it so parallel runs never share a counter key.
    reporter_id: String,
    /// Per-run counter registry for the max_series guard.
    counters: Option<Arc<delivery::ReporterCounters>>,
    client: reqwest::Client,
}

impl ServeSink {
    pub fn new(
        url: String,
        labels: Map<String, Value>,
        auth: Option<Auth>,
        task_id: Option<String>,
        max_series: usize,
    ) -> Self {
        Self {
            url,
            labels,
            auth,
            task_id,
            max_series,
            reporter_id: "std/report/serve@v1".to_string(),
            counters: None,
            client: reqwest::Client::new(),
        }
    }

    /// Attach the per-run counter registry.
    pub fn with_counters_registry(mut self, counters: Arc<delivery::ReporterCounters>) -> Self {
        self.counters = Some(counters);
        self
    }

    /// Override the counter/log identity (tests isolate their counters).
    #[cfg(test)]
    pub(crate) fn with_reporter_id(mut self, id: &str) -> Self {
        self.reporter_id = id.to_string();
        self
    }

    /// The labels stamped on every sample: run labels from the snapshot,
    /// overlaid by this reporter's labels, plus `task_id` when set.
    fn labels_for(&self, snap: &MetricSnapshot) -> Map<String, Value> {
        let mut labels = snap.labels.clone();
        for (k, v) in &self.labels {
            labels.insert(k.clone(), v.clone());
        }
        if let Some(task_id) = &self.task_id {
            labels.insert("task_id".to_string(), Value::String(task_id.clone()));
        }
        labels
    }
}

/// Drop output series past the `max_series` guard — deterministic: the
/// distinct `(metric, labels)` keys are sorted and the first `max_series`
/// survive; the rest are counted in
/// `perfscale_report_dropped_series_total{reporter}`, idempotently per run
/// (the terminal flush re-encoding the same snapshot does not count again).
pub(crate) fn cap_sample_series(
    reporter: &str,
    samples: Vec<Sample>,
    max_series: usize,
    counters: &Option<Arc<delivery::ReporterCounters>>,
) -> Vec<Sample> {
    // The reporter's own counters are exempt from the guard.
    let (exempt, samples): (Vec<Sample>, Vec<Sample>) = samples
        .into_iter()
        .partition(|s| delivery::is_reporter_counter(&s.metric));
    let mut keys: Vec<(String, String)> = samples
        .iter()
        .map(|s| {
            (
                s.metric.clone(),
                Value::Object(s.labels.clone()).to_string(),
            )
        })
        .collect();
    keys.sort();
    keys.dedup();
    if keys.len() <= max_series {
        return [exempt, samples].concat();
    }
    let keep: std::collections::BTreeSet<_> = keys.into_iter().take(max_series).collect();
    let dropped: std::collections::BTreeSet<String> = samples
        .iter()
        .map(|s| {
            (
                s.metric.clone(),
                Value::Object(s.labels.clone()).to_string(),
            )
        })
        .filter(|k| !keep.contains(k))
        .map(|(m, l)| format!("{m}|{l}"))
        .collect();
    delivery::note_dropped_series_with(counters, reporter, dropped.into_iter());
    let kept: Vec<Sample> = samples
        .into_iter()
        .filter(|s| {
            keep.contains(&(
                s.metric.clone(),
                Value::Object(s.labels.clone()).to_string(),
            ))
        })
        .collect();
    [exempt, kept].concat()
}

impl Sink for ServeSink {
    type Item = Sample;

    fn reporter(&self) -> &str {
        &self.reporter_id
    }

    fn encode(&mut self, snap: &MetricSnapshot) -> Vec<Sample> {
        // Label-aware flatten (issue #12): with labeled series present (the
        // native engine always fills them), each `(name, step labels)` series
        // ships with the merged step > reporter > run labels; `task_id` stamps
        // on top. Snapshots without series (foreign engines) keep the legacy
        // aggregate flatten, byte-identical to before.
        let samples = if snap.series.is_empty() {
            snapshot_to_samples(snap, &self.labels_for(snap))
        } else {
            super::stream::snapshot_to_samples_labeled(
                snap,
                &self.labels,
                self.task_id.as_deref().map(|t| ("task_id", t)),
            )
        };
        let reporter = self.reporter_id.clone();
        cap_sample_series(&reporter, samples, self.max_series, &self.counters)
    }

    fn send<'a>(&'a mut self, items: &'a [Sample], seq: u64) -> BoxFuture<'a, PostOutcome> {
        Box::pin(async move {
            let endpoint = format!("{}/api/v1/metrics", self.url.trim_end_matches('/'));
            let mut body = serde_json::json!({
                "samples": items,
                "seq": seq,
            });
            if let Some(task_id) = &self.task_id {
                body["taskId"] = Value::String(task_id.clone());
            }
            let mut req = self
                .client
                .post(&endpoint)
                .json(&body)
                .timeout(REQUEST_TIMEOUT);
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
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
                    {
                        PostOutcome::Retry {
                            after: retry_after(resp.headers()),
                            reason: format!("status {status}"),
                        }
                    } else {
                        PostOutcome::Poison(format!("status {status}"))
                    }
                }
                Err(e) => PostOutcome::Retry {
                    after: None,
                    reason: format!("network error: {e}"),
                },
            }
        })
    }
}

/// Parse a `Retry-After` response header (seconds or HTTP-date; the date
/// form is rare for APIs — seconds covers 429/503 from the targets we
/// support).
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Backward-compatible facade over the serve reporter +
/// [`delivery::spawn`]: same constructor and `spawn(rx)` shape as before
/// issue #12, so existing consumers (the agent) keep working. The delivery
/// semantics are the shared bounded ones now: the snapshot queue and the
/// sealed-batch backlog drop the OLDEST entry past `max_pending` (counted in
/// `perfscale_report_dropped_snapshots_total`), replacing the previous soft
/// warn-only cap.
pub struct DuringRunShipper {
    sink: ServeSink,
    cfg: delivery::DeliveryConfig,
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
        let sink = ServeSink::new(
            target_url,
            static_labels,
            auth,
            task_id,
            super::reporters::DEFAULT_MAX_SERIES,
        );
        let retry = delivery::RetryPolicy {
            // The historical shipper retried without an attempt cap during
            // the run; keep that for the legacy facade (the final flush is
            // bounded by final_timeout).
            max_attempts: u32::MAX,
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
        };
        Self {
            sink,
            cfg: delivery::DeliveryConfig {
                interval: cfg.interval(),
                batch_size: cfg.batch_size,
                max_pending: cfg.max_pending,
                retry,
                final_timeout: Duration::from_secs(30),
                cpu_gate: (cfg.max_cpu_percent > 0.0)
                    .then(|| (cfg.max_cpu_percent, Arc::new(cpu_reader) as _)),
                counters: None,
            },
        }
    }

    /// Run the shipper until the snapshot channel closes and the final
    /// drain completes.
    pub fn spawn(
        self,
        mut rx: tokio::sync::mpsc::Receiver<MetricSnapshot>,
    ) -> tokio::task::JoinHandle<()> {
        let queue = Arc::new(delivery::SnapshotQueue::new(
            "std/report/serve@v1".to_string(),
            self.cfg.max_pending,
        ));
        let forward = {
            let queue = Arc::clone(&queue);
            tokio::spawn(async move {
                while let Some(snap) = rx.recv().await {
                    queue.push(snap);
                }
                queue.close();
            })
        };
        let worker = delivery::spawn(self.sink, self.cfg, queue);
        tokio::spawn(async move {
            let _ = forward.await;
            let _ = worker.await;
        })
    }
}

/// Legacy alias kept for callers that name the auth type through the old
/// path.
pub type ServeAuth = Auth;

/// Enforce `max_series` on a set of label-keyed series built by a sink —
/// deterministic drop: keys sort, the first `max_series` survive.
/// Returns the surviving keys (in sorted order) and the drop count.
pub(crate) fn capped_series_keys<K: Ord + Clone + std::fmt::Debug>(
    reporter: &str,
    mut keys: Vec<K>,
    max_series: usize,
    counters: &Option<Arc<delivery::ReporterCounters>>,
) -> std::collections::BTreeSet<K> {
    keys.sort();
    keys.dedup();
    if keys.len() > max_series {
        // Idempotent per run: only keys never dropped before count.
        let dropped = keys
            .drain(max_series..)
            .map(|k| format!("{k:?}"))
            .collect::<Vec<_>>();
        delivery::note_dropped_series_with(counters, reporter, dropped.into_iter());
    }
    keys.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn cap_sample_series_drops_deterministically_and_counts() {
        let mk = |metric: &str, tag: &str| {
            let mut labels = Map::new();
            labels.insert("tag".into(), Value::String(tag.into()));
            Sample {
                metric: metric.into(),
                labels,
                value: 1.0,
                ts_ms: 7,
            }
        };
        let reporter = "test/cap-series";
        let before = delivery::counter_value(delivery::DROPPED_SERIES_METRIC, reporter);
        let samples = vec![mk("b", "2"), mk("a", "1"), mk("c", "3"), mk("b", "2")];
        let kept = cap_sample_series(reporter, samples, 2, &None);
        // Two series survive: ("a",…1) and ("b",…2) — sorted order, `c` drops.
        assert_eq!(kept.len(), 3);
        assert!(kept.iter().all(|s| s.metric != "c"));
        let after = delivery::counter_value(delivery::DROPPED_SERIES_METRIC, reporter);
        assert_eq!(after - before, 1);
    }

    #[test]
    fn retry_after_parses_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(7)));
        assert_eq!(retry_after(&reqwest::header::HeaderMap::new()), None);
    }

    #[test]
    fn labeled_encode_merges_step_labels_and_matches_legacy_for_unlabeled() {
        use crate::report::stream::{
            snapshot_to_samples, snapshot_to_samples_labeled, SeriesSnapshot,
        };
        use crate::step::thresholds::{MetricAgg, MetricKind};

        let agg = MetricAgg {
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
        };
        let series = |labels: &[(&str, &str)]| SeriesSnapshot {
            name: "lat".to_string(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            agg,
            gauge: false,
            buckets: Vec::new(),
        };
        let mut run_labels = Map::new();
        run_labels.insert("testid".into(), Value::String("t1".into()));
        let base = MetricSnapshot {
            ts_ms: 42,
            metrics: BTreeMap::from([("lat".to_string(), agg)]),
            iterations_completed: 3,
            vus: 1,
            gpu: Vec::new(),
            run_id: "r1".into(),
            started_ms: 0,
            labels: run_labels,
            series: Vec::new(),
            terminal: false,
        };

        // Step labels merge under task_id, over run/reporter labels.
        let reporter_labels =
            Map::from_iter([("env".to_string(), Value::String("staging".into()))]);
        let mut labeled_snap = base.clone();
        labeled_snap.series = vec![series(&[("channel", "book"), ("env", "step-wins")])];
        let samples = snapshot_to_samples_labeled(
            &labeled_snap,
            &reporter_labels,
            Some(("task_id", "task-9")),
        );
        let p50 = samples
            .iter()
            .find(|s| s.metric == "lat" && s.labels["quantile"] == "0.5")
            .expect("quantile series");
        assert_eq!(p50.labels["channel"], "book");
        assert_eq!(p50.labels["env"], "step-wins", "step > reporter");
        assert_eq!(p50.labels["testid"], "t1", "run labels underneath");
        assert_eq!(p50.labels["task_id"], "task-9");
        assert_eq!(p50.value, 0.008);
        // Reserved engine labels are NOT stamped on the serve shape.
        assert!(!p50.labels.contains_key("run_id"));
        assert!(!p50.labels.contains_key("worker"));

        // Unlabeled run: the labeled flatten equals the legacy flatten plus
        // run labels (the legacy path never saw them).
        let mut unlabeled = base.clone();
        unlabeled.series = vec![series(&[])];
        let new =
            snapshot_to_samples_labeled(&unlabeled, &reporter_labels, Some(("task_id", "task-9")));
        let legacy = snapshot_to_samples(&unlabeled, &Map::new());
        let strip = |s: &[Sample]| -> Vec<(String, f64, i64)> {
            s.iter()
                .map(|s| (s.metric.clone(), s.value, s.ts_ms))
                .collect()
        };
        assert_eq!(
            strip(&new),
            strip(&legacy),
            "same metrics, values and timestamps as the legacy flatten"
        );
        // …and the legacy path stays byte-identical (labels from the
        // constructor only, as before).
        let mut static_labels = Map::new();
        static_labels.insert("task_id".into(), Value::String("task-9".into()));
        let legacy_with_labels = snapshot_to_samples(&unlabeled, &static_labels);
        assert!(
            legacy_with_labels
                .iter()
                .all(|s| s.labels.keys().all(|k| k == "task_id" || k == "quantile")),
            "legacy shape: constructor labels + quantile only"
        );
    }
}
