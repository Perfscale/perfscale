//! `std/report/prometheus-rw@v1` — Prometheus remote write streaming
//! reporter (issue #12).
//!
//! Each cumulative [`MetricSnapshot`] encodes to protobuf `TimeSeries`
//! (`WriteRequest`), Snappy-block-compressed and POSTed to `with.url` with
//! the remote-write headers. Delivery (bounded drop-oldest queue, batching,
//! retries, final flush) is the shared [`super::delivery`] layer.
//!
//! # Mapping
//!
//! - counters/rates/gauges → plain samples (`<name>_total`,
//!   `<name>_failed_total`, gauges as-is; durations in **seconds**);
//! - durations → bucketed histograms, never client-side quantiles:
//!   `with.histogram: classic` (default) emits `_bucket{le}` + `_sum` +
//!   `_count`; `native` emits a Prometheus native histogram (opt-in — the
//!   backend must have the native-histogram feature enabled).
//!
//! Labels are the merged step > reporter > run set plus the reserved
//! `run_id` and `worker`. `max_series` caps the output series count
//! (classic buckets included), deterministically.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::future::BoxFuture;
use prost::Message;
use serde_json::{Map, Value};

use super::delivery::{self, PostOutcome, Sink};
use super::serve::capped_series_keys;
use super::stream::{merge_labels, MetricSnapshot, SeriesSnapshot};
use crate::step::thresholds::MetricKind;

/// Per-request timeout for one remote-write POST.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Native-histogram schema: base 2^(2^-schema) bucket growth. Schema 3
/// (≈9% relative width per bucket) is a reasonable match for the engine's
/// 2-significant-digit HDR histograms without exploding the span count.
const NATIVE_SCHEMA: i32 = 3;

// ---------------------------------------------------------------------------
// protobuf messages (prometheus/prompb `types.proto` + `remote.proto`),
// hand-written prost structs — pure Rust, no protoc. Field numbers match the
// official definitions.
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct PromLabel {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct PromSample {
    #[prost(double, tag = "1")]
    pub value: f64,
    /// Milliseconds since the Unix epoch.
    #[prost(int64, tag = "2")]
    pub timestamp: i64,
}

/// A native-histogram bucket span (`BucketSpan`).
#[derive(Clone, PartialEq, Message)]
pub struct PromBucketSpan {
    #[prost(sint32, tag = "1")]
    pub offset: i32,
    #[prost(uint32, tag = "2")]
    pub length: u32,
}

/// A Prometheus native histogram (`Histogram`, integer-count flavor).
#[derive(Clone, PartialEq, Message)]
pub struct PromHistogram {
    #[prost(uint64, tag = "1")]
    pub count_int: u64,
    #[prost(double, tag = "3")]
    pub sum: f64,
    #[prost(sint32, tag = "4")]
    pub schema: i32,
    #[prost(double, tag = "5")]
    pub zero_threshold: f64,
    #[prost(uint64, tag = "6")]
    pub zero_count_int: u64,
    #[prost(message, repeated, tag = "8")]
    pub negative_spans: Vec<PromBucketSpan>,
    #[prost(sint64, repeated, tag = "9")]
    pub negative_deltas: Vec<i64>,
    #[prost(message, repeated, tag = "11")]
    pub positive_spans: Vec<PromBucketSpan>,
    #[prost(sint64, repeated, tag = "12")]
    pub positive_deltas: Vec<i64>,
    /// `ResetHint::NO` — cumulative snapshots within one run never reset.
    #[prost(int32, tag = "14")]
    pub reset_hint: i32,
    /// Milliseconds since the Unix epoch.
    #[prost(int64, tag = "15")]
    pub timestamp: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct PromTimeSeries {
    #[prost(message, repeated, tag = "1")]
    pub labels: Vec<PromLabel>,
    #[prost(message, repeated, tag = "2")]
    pub samples: Vec<PromSample>,
    #[prost(message, repeated, tag = "4")]
    pub histograms: Vec<PromHistogram>,
}

/// `WriteRequest` (`remote.proto`): `repeated TimeSeries timeseries = 1`.
#[derive(Clone, PartialEq, Message)]
pub struct PromWriteRequest {
    #[prost(message, repeated, tag = "1")]
    pub timeseries: Vec<PromTimeSeries>,
}

/// `with.histogram` mode: `classic` (default) or `native`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistogramMode {
    Classic,
    Native,
}

/// The Prometheus remote-write sink.
pub struct PromRwSink {
    url: String,
    headers: Vec<(String, String)>,
    /// Reporter-level labels (already interpolated).
    labels: Map<String, Value>,
    max_series: usize,
    mode: HistogramMode,
    /// The `worker` label value (shard index; `"0"` for a standalone run).
    worker: String,
    /// Per-run counter registry for the max_series guard.
    counters: Option<std::sync::Arc<delivery::ReporterCounters>>,
    client: reqwest::Client,
}

impl PromRwSink {
    /// `headers` are resolved (`${VAR}` interpolation happens here — the
    /// values are secrets, never logged).
    pub fn new(
        url: String,
        headers: &BTreeMap<String, String>,
        labels: Map<String, Value>,
        max_series: usize,
        histogram: Option<&str>,
        worker: String,
    ) -> Result<Self, String> {
        let headers = headers
            .iter()
            .map(|(k, v)| Ok((k.clone(), super::reporters::resolve_env_refs(v)?)))
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            url,
            headers,
            labels,
            max_series,
            mode: match histogram {
                Some("native") => HistogramMode::Native,
                _ => HistogramMode::Classic,
            },
            worker,
            counters: None,
            client: reqwest::Client::new(),
        })
    }

    /// Attach the per-run counter registry.
    pub fn with_counters_registry(
        mut self,
        counters: std::sync::Arc<delivery::ReporterCounters>,
    ) -> Self {
        self.counters = Some(counters);
        self
    }
}

/// One labeled output series before protobuf encoding: a metric name (with
/// the `_bucket`/`_sum`/`_count`/`_total` suffix applied), its full label
/// set, and the value or native histogram.
#[derive(Debug)]
struct OutSeries {
    name: String,
    labels: BTreeMap<String, String>,
    /// Sample time override (GPU gauges keep the collector's timestamp);
    /// `None` uses the snapshot's `ts_ms`.
    ts_ms: Option<i64>,
    value: OutValue,
}

#[derive(Debug)]
enum OutValue {
    Sample(f64),
    Histogram {
        sum: f64,
        count: u64,
        buckets: Vec<(f64, u64)>,
    },
}

/// Expand one engine series into output series (seconds, Prometheus naming).
fn expand_series(
    s: &SeriesSnapshot,
    labels: &BTreeMap<String, String>,
    mode: HistogramMode,
    out: &mut Vec<OutSeries>,
) {
    let push =
        |name: String, extra: Option<(&str, String)>, value: f64, out: &mut Vec<OutSeries>| {
            let mut labels = labels.clone();
            if let Some((k, v)) = extra {
                labels.insert(k.to_string(), v);
            }
            out.push(OutSeries {
                name,
                labels,
                ts_ms: None,
                value: OutValue::Sample(value),
            });
        };
    match s.agg.kind {
        MetricKind::Sample => match mode {
            HistogramMode::Native => {
                out.push(OutSeries {
                    name: s.name.clone(),
                    labels: labels.clone(),
                    ts_ms: None,
                    value: OutValue::Histogram {
                        // avg (ms) × count → seconds.
                        sum: s.agg.avg * s.agg.count / 1000.0,
                        count: s.agg.count as u64,
                        buckets: s
                            .buckets
                            .iter()
                            .map(|b| (b.le_ms / 1000.0, b.count))
                            .collect(),
                    },
                });
            }
            HistogramMode::Classic => {
                // `_bucket{le}` series carry CUMULATIVE counts (Prometheus
                // convention); the HDR buckets are per-bucket counts.
                let mut cumulative = 0u64;
                for b in &s.buckets {
                    cumulative += b.count;
                    push(
                        format!("{}_bucket", s.name),
                        Some(("le", format_le(b.le_ms / 1000.0))),
                        cumulative as f64,
                        out,
                    );
                }
                push(
                    format!("{}_bucket", s.name),
                    Some(("le", "+Inf".to_string())),
                    s.agg.count,
                    out,
                );
                push(
                    format!("{}_sum", s.name),
                    None,
                    s.agg.avg * s.agg.count / 1000.0,
                    out,
                );
                push(format!("{}_count", s.name), None, s.agg.count, out);
            }
        },
        MetricKind::Counter => push(format!("{}_total", s.name), None, s.agg.count, out),
        MetricKind::Rate => {
            push(format!("{}_total", s.name), None, s.agg.count, out);
            push(
                format!("{}_failed_total", s.name),
                None,
                s.agg.rate * s.agg.count,
                out,
            );
        }
    }
}

/// Format an `le` bound: integers stay short, fractions keep enough digits
/// to be exact for millisecond/microsecond boundaries.
fn format_le(secs: f64) -> String {
    if secs == secs.trunc() && secs.abs() < 1e15 {
        format!("{}", secs as i64)
    } else {
        format!("{secs}")
    }
}

/// Encode a snapshot into output series — the unified no-duplicates view:
///
/// - every label-aware series in `snap.series` (the native engine fills it);
/// - every metric in the aggregate `snap.metrics` map whose name has NO
///   series (aggregate-only producers), as an unlabeled bucketless series;
/// - the run-progress gauges, so a live run never flattens to an empty
///   request;
/// - the GPU gauge samples (point-in-time, `gpu="<index>"` label, each with
///   the collector's own timestamp).
fn snapshot_to_out_series(
    snap: &MetricSnapshot,
    reporter_labels: &Map<String, Value>,
    worker: &str,
    mode: HistogramMode,
) -> Vec<OutSeries> {
    let mut out = Vec::new();
    let mut series_names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for s in &snap.series {
        series_names.insert(s.name.as_str());
        let labels = merge_labels(
            &snap.labels,
            reporter_labels,
            &s.labels,
            &snap.run_id,
            worker,
        );
        expand_series(s, &labels, mode, &mut out);
    }
    // Aggregate-only metrics (no labeled series): quantile-less series —
    // count/sum for sample metrics, totals for counters and rates.
    let empty = BTreeMap::new();
    for (name, agg) in &snap.metrics {
        if series_names.contains(name.as_str()) {
            continue;
        }
        let labels = merge_labels(&snap.labels, reporter_labels, &empty, &snap.run_id, worker);
        let s = SeriesSnapshot {
            name: name.clone(),
            labels: BTreeMap::new(),
            agg: *agg,
            gauge: false,
            buckets: Vec::new(),
        };
        expand_series(&s, &labels, mode, &mut out);
    }
    // Run-progress gauges ride every snapshot.
    let labels = merge_labels(&snap.labels, reporter_labels, &empty, &snap.run_id, worker);
    for (name, value) in [
        (
            "perfscale_iterations_completed",
            snap.iterations_completed as f64,
        ),
        ("perfscale_vus", snap.vus as f64),
    ] {
        out.push(OutSeries {
            name: name.to_string(),
            labels: labels.clone(),
            ts_ms: None,
            value: OutValue::Sample(value),
        });
    }
    // GPU gauges: point-in-time, per device, collector timestamps.
    for g in &snap.gpu {
        let mut labels = labels.clone();
        labels.insert("gpu".to_string(), g.index.to_string());
        for (name, v) in gpu_gauges(g) {
            out.push(OutSeries {
                name: name.to_string(),
                labels: labels.clone(),
                ts_ms: Some(g.ts_ms as i64),
                value: OutValue::Sample(v),
            });
        }
    }
    out
}

/// A GPU sample's populated fields as `(name, value)` gauge pairs — `N/A`
/// fields are skipped, pro-collector extras ride under their own names.
pub(crate) fn gpu_gauges(g: &crate::gpu::GpuSample) -> Vec<(&str, f64)> {
    let mut out: Vec<(&str, f64)> = Vec::new();
    for (name, v) in [
        ("gpu_utilization_pct", g.utilization_pct),
        ("gpu_memory_used_mib", g.memory_used_mib),
        ("gpu_memory_total_mib", g.memory_total_mib),
        ("gpu_temperature_c", g.temperature_c),
        ("gpu_power_w", g.power_w),
    ] {
        if let Some(v) = v {
            out.push((name, v));
        }
    }
    for (name, v) in &g.extra {
        out.push((name.as_str(), *v));
    }
    out
}

/// Aggregate per-bucket counts (upper bound seconds → count) into a
/// Prometheus native histogram: power-of-2^(2^-schema) buckets as
/// spans + deltas.
fn buckets_to_native(sum: f64, count: u64, buckets: &[(f64, u64)], ts_ms: i64) -> PromHistogram {
    let base_exponent = (1u32 << NATIVE_SCHEMA) as f64; // indices per power of two
    let mut by_index: BTreeMap<i32, u64> = BTreeMap::new();
    for &(le, n) in buckets {
        if le <= 0.0 || n == 0 {
            continue;
        }
        // Smallest bucket index whose upper bound base^(i+1) ≥ le.
        let idx = (le.log2() * base_exponent).ceil() as i32 - 1;
        *by_index.entry(idx).or_insert(0) += n;
    }
    let mut spans: Vec<PromBucketSpan> = Vec::new();
    let mut deltas: Vec<i64> = Vec::new();
    let mut prev_end: Option<i32> = None;
    let mut prev_count = 0i64;
    for (idx, n) in &by_index {
        match prev_end {
            // Same contiguous run: extend the span.
            Some(end) if *idx == end => {
                spans.last_mut().expect("a span is open").length += 1;
            }
            _ => {
                let offset = match prev_end {
                    Some(end) => idx - end,
                    None => *idx,
                };
                spans.push(PromBucketSpan { offset, length: 1 });
            }
        }
        let delta = *n as i64 - prev_count;
        deltas.push(delta);
        prev_count = *n as i64;
        prev_end = Some(idx + 1);
    }
    PromHistogram {
        count_int: count,
        sum,
        schema: NATIVE_SCHEMA,
        zero_threshold: 0.0,
        zero_count_int: 0,
        negative_spans: Vec::new(),
        negative_deltas: Vec::new(),
        positive_spans: spans,
        positive_deltas: deltas,
        reset_hint: 2, // ResetHint::NO — cumulative within a run
        timestamp: ts_ms,
    }
}

impl Sink for PromRwSink {
    type Item = PromTimeSeries;

    fn reporter(&self) -> &str {
        "std/report/prometheus-rw@v1"
    }

    fn encode(&mut self, snap: &MetricSnapshot) -> Vec<PromTimeSeries> {
        let out = snapshot_to_out_series(snap, &self.labels, &self.worker, self.mode);
        // max_series guard: deterministic drop — series sort by
        // (name, labels) and the first `max_series` survive.
        // The reporter's own counters are exempt from the guard.
        let (exempt, guarded): (Vec<OutSeries>, Vec<OutSeries>) = out
            .into_iter()
            .partition(|s| delivery::is_reporter_counter(&s.name));
        let keys: Vec<(String, BTreeMap<String, String>)> = guarded
            .iter()
            .map(|s| (s.name.clone(), s.labels.clone()))
            .collect();
        let keep = capped_series_keys(self.reporter(), keys, self.max_series, &self.counters);
        guarded
            .into_iter()
            .filter(|s| keep.contains(&(s.name.clone(), s.labels.clone())))
            .chain(exempt)
            .map(|s| {
                let mut labels: Vec<PromLabel> = s
                    .labels
                    .iter()
                    .map(|(k, v)| PromLabel {
                        name: k.clone(),
                        value: v.clone(),
                    })
                    .collect();
                // `__name__` is the metric-name label, sorted first.
                labels.insert(
                    0,
                    PromLabel {
                        name: "__name__".to_string(),
                        value: s.name,
                    },
                );
                PromTimeSeries {
                    labels,
                    samples: match &s.value {
                        OutValue::Sample(v) => vec![PromSample {
                            value: *v,
                            timestamp: s.ts_ms.unwrap_or(snap.ts_ms),
                        }],
                        OutValue::Histogram { .. } => Vec::new(),
                    },
                    histograms: match &s.value {
                        OutValue::Histogram {
                            sum,
                            count,
                            buckets,
                        } => vec![buckets_to_native(
                            *sum,
                            *count,
                            buckets,
                            s.ts_ms.unwrap_or(snap.ts_ms),
                        )],
                        OutValue::Sample(_) => Vec::new(),
                    },
                }
            })
            .collect()
    }

    fn send<'a>(
        &'a mut self,
        items: &'a [PromTimeSeries],
        _seq: u64,
    ) -> BoxFuture<'a, PostOutcome> {
        Box::pin(async move {
            let body = PromWriteRequest {
                timeseries: items.to_vec(),
            }
            .encode_to_vec();
            let compressed = match snap::raw::Encoder::new().compress_vec(&body) {
                Ok(c) => c,
                Err(e) => return PostOutcome::Poison(format!("snappy encode failed: {e}")),
            };
            let mut req = self
                .client
                .post(&self.url)
                .header("Content-Type", "application/x-protobuf")
                .header("Content-Encoding", "snappy")
                .header("X-Prometheus-Remote-Write-Version", "0.1.0")
                .body(compressed)
                .timeout(REQUEST_TIMEOUT);
            for (k, v) in &self.headers {
                req = req.header(k, v);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => PostOutcome::Delivered,
                Ok(resp) => {
                    let status = resp.status();
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
                    {
                        PostOutcome::Retry {
                            after: super::serve::retry_after(resp.headers()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::delivery;
    use crate::report::stream::BucketCount;
    use crate::step::thresholds::MetricAgg;

    fn sample_series(
        name: &str,
        labels: &[(&str, &str)],
        buckets: &[(f64, u64)],
    ) -> SeriesSnapshot {
        SeriesSnapshot {
            name: name.to_string(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            gauge: false,
            agg: MetricAgg {
                kind: MetricKind::Sample,
                avg: 10.0,
                min: 1.0,
                max: 50.0,
                p50: 8.0,
                p90: 20.0,
                p95: 30.0,
                p99: 40.0,
                count: buckets.iter().map(|b| b.1).sum::<u64>() as f64,
                rate: 0.0,
            },
            buckets: buckets
                .iter()
                .map(|(le_ms, count)| BucketCount {
                    le_ms: *le_ms,
                    count: *count,
                })
                .collect(),
        }
    }

    fn snap(series: Vec<SeriesSnapshot>) -> MetricSnapshot {
        let mut run_labels = Map::new();
        run_labels.insert("run_label".into(), Value::String("r".into()));
        MetricSnapshot {
            ts_ms: 1_735_689_600_123,
            metrics: BTreeMap::new(),
            iterations_completed: 5,
            vus: 2,
            gpu: Vec::new(),
            run_id: "run-1".into(),
            started_ms: 1_735_689_000_000,
            labels: run_labels,
            series,
            terminal: false,
        }
    }

    #[test]
    fn classic_histogram_emits_cumulative_buckets_sum_count() {
        let s = sample_series("lat", &[("channel", "book")], &[(10.0, 3), (20.0, 1)]);
        let out = snapshot_to_out_series(&snap(vec![s]), &Map::new(), "0", HistogramMode::Classic);
        let find = |name: &str, le: Option<&str>| {
            out.iter()
                .find(|s| {
                    s.name == name
                        && le
                            .map(|le| s.labels.get("le").map(String::as_str) == Some(le))
                            .unwrap_or(true)
                        && !s.labels.contains_key("le") == le.is_none()
                })
                .unwrap_or_else(|| panic!("series {name}/{le:?} present: {out:?}"))
        };
        // Cumulative bucket counts: 3 below 10ms, 4 below 20ms, 4 at +Inf.
        let b = |le: &str| match &find("lat_bucket", Some(le)).value {
            OutValue::Sample(v) => *v,
            _ => panic!("bucket is a sample"),
        };
        assert_eq!(b("0.01"), 3.0);
        assert_eq!(b("0.02"), 4.0);
        assert_eq!(b("+Inf"), 4.0);
        // sum = avg × count in seconds; count as-is.
        match &find("lat_sum", None).value {
            OutValue::Sample(v) => assert_eq!(*v, 10.0 * 4.0 / 1000.0),
            _ => panic!(),
        }
        match &find("lat_count", None).value {
            OutValue::Sample(v) => assert_eq!(*v, 4.0),
            _ => panic!(),
        }
        // Labels: step + run + reserved engine labels; NO quantile series.
        let bucket = find("lat_bucket", Some("+Inf"));
        assert_eq!(
            bucket.labels.get("channel").map(String::as_str),
            Some("book")
        );
        assert_eq!(
            bucket.labels.get("run_label").map(String::as_str),
            Some("r")
        );
        assert_eq!(
            bucket.labels.get("run_id").map(String::as_str),
            Some("run-1")
        );
        assert_eq!(bucket.labels.get("worker").map(String::as_str), Some("0"));
        assert!(out.iter().all(|s| !s.labels.contains_key("quantile")));
    }

    #[test]
    fn native_histogram_counts_and_sum_match_the_hdr_buckets() {
        let buckets = [(1.0, 2u64), (10.0, 3), (100.0, 1)];
        let s = sample_series("lat", &[], &buckets);
        let out = snapshot_to_out_series(&snap(vec![s]), &Map::new(), "0", HistogramMode::Native);
        let hist = out
            .iter()
            .find_map(|s| match &s.value {
                OutValue::Histogram {
                    sum,
                    count,
                    buckets,
                } => Some((*sum, *count, buckets.clone())),
                _ => None,
            })
            .expect("one native histogram series");
        assert_eq!(hist.1, 6, "count = total samples");
        assert_eq!(hist.0, 10.0 * 6.0 / 1000.0, "sum in seconds");

        let h = buckets_to_native(hist.0, hist.1, &hist.2, 42);
        // Every sample lands in exactly one positive bucket (deltas are
        // relative — expand them first).
        let mut acc = 0i64;
        let mut counts = Vec::new();
        for d in &h.positive_deltas {
            acc += d;
            counts.push(acc);
        }
        assert_eq!(
            counts.iter().sum::<i64>(),
            6,
            "{counts:?} (spans: {:?}, deltas: {:?})",
            h.positive_spans,
            h.positive_deltas
        );
        assert_eq!(h.count_int, 6);
        assert_eq!(h.timestamp, 42);
        assert_eq!(h.schema, NATIVE_SCHEMA);
        // Spans cover all delta entries.
        let span_len: u32 = h.positive_spans.iter().map(|s| s.length).sum();
        assert_eq!(span_len as usize, h.positive_deltas.len());
    }

    #[test]
    fn write_request_roundtrips_through_snappy_and_protobuf() {
        let mut labels = BTreeMap::new();
        labels.insert("run_id".to_string(), "r1".to_string());
        let ts = PromTimeSeries {
            labels: vec![
                PromLabel {
                    name: "__name__".into(),
                    value: "lat_count".into(),
                },
                PromLabel {
                    name: "run_id".into(),
                    value: "r1".into(),
                },
            ],
            samples: vec![PromSample {
                value: 4.0,
                timestamp: 1_735_689_600_123,
            }],
            histograms: Vec::new(),
        };
        let body = PromWriteRequest {
            timeseries: vec![ts],
        }
        .encode_to_vec();
        let compressed = snap::raw::Encoder::new().compress_vec(&body).unwrap();
        let decoded = snap::raw::Decoder::new()
            .decompress_vec(&compressed)
            .unwrap();
        let req = PromWriteRequest::decode(decoded.as_slice()).unwrap();
        assert_eq!(req.timeseries.len(), 1);
        let s = &req.timeseries[0];
        assert_eq!(s.labels[0].name, "__name__");
        assert_eq!(s.labels[0].value, "lat_count");
        assert_eq!(s.samples[0].value, 4.0);
        assert_eq!(s.samples[0].timestamp, 1_735_689_600_123);
        assert!(labels.contains_key("run_id"));
    }

    #[test]
    fn max_series_drops_deterministically_and_counts() {
        let series: Vec<SeriesSnapshot> = (0..5)
            .map(|i| {
                let mut s = sample_series(&format!("m{i}"), &[], &[(1.0, 1)]);
                s.labels = BTreeMap::new();
                s
            })
            .collect();
        let reporter = "std/report/prometheus-rw@v1";
        let before = delivery::counter_value(delivery::DROPPED_SERIES_METRIC, reporter);
        let out = snapshot_to_out_series(&snap(series), &Map::new(), "0", HistogramMode::Classic);
        let keys: Vec<_> = out
            .iter()
            .map(|s| (s.name.clone(), s.labels.clone()))
            .collect();
        // 5 metrics × 4 series (bucket 1ms, bucket +Inf, sum, count) + 2 gauges = 22.
        assert_eq!(keys.len(), 22);
        let keep = capped_series_keys(reporter, keys, 10, &None);
        assert_eq!(keep.len(), 10);
        let after = delivery::counter_value(delivery::DROPPED_SERIES_METRIC, reporter);
        assert_eq!(after - before, 12);
    }
}

#[cfg(test)]
mod wire_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Map, Value};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::report::delivery::{self, DeliveryConfig, RetryPolicy, SnapshotQueue};
    use crate::report::stream::BucketCount;
    use crate::step::thresholds::MetricAgg;

    fn snapshot() -> MetricSnapshot {
        let mut labels = BTreeMap::new();
        labels.insert("channel".to_string(), "book".to_string());
        let series = vec![SeriesSnapshot {
            name: "http_req_duration".to_string(),
            labels,
            agg: MetricAgg {
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
            gauge: false,
            buckets: vec![
                BucketCount {
                    le_ms: 10.0,
                    count: 3,
                },
                BucketCount {
                    le_ms: 50.0,
                    count: 1,
                },
            ],
        }];
        MetricSnapshot {
            ts_ms: 1_735_689_600_123,
            metrics: BTreeMap::new(),
            iterations_completed: 9,
            vus: 1,
            gpu: Vec::new(),
            run_id: "run-7".into(),
            started_ms: 1_735_689_000_000,
            labels: Map::new(),
            series,
            terminal: false,
        }
    }

    fn decode_body(body: &[u8]) -> PromWriteRequest {
        let raw = snap::raw::Decoder::new()
            .decompress_vec(body)
            .expect("snappy block decode");
        PromWriteRequest::decode(raw.as_slice()).expect("protobuf WriteRequest")
    }

    #[tokio::test]
    async fn remote_write_posts_snappy_protobuf_with_buckets_and_labels() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/write"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let mut reporter_labels = Map::new();
        reporter_labels.insert("env".into(), Value::String("staging".into()));
        let sink = PromRwSink::new(
            format!("{}/api/v1/write", server.uri()),
            &BTreeMap::new(),
            reporter_labels,
            10_000,
            None, // classic
            "0".to_string(),
        )
        .unwrap();
        let cfg = DeliveryConfig {
            interval: Duration::from_millis(100),
            batch_size: 1000,
            max_pending: 8,
            retry: RetryPolicy::default(),
            final_timeout: Duration::from_secs(5),
            cpu_gate: None,
            counters: None,
        };
        let queue = Arc::new(SnapshotQueue::new("test/promrw".to_string(), 8));
        let worker = delivery::spawn(sink, cfg, Arc::clone(&queue));
        queue.push(snapshot());
        queue.close();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();

        // Closed before the first tick: the coalesced terminal flush is the
        // only request.
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "{:?}", reqs.len());
        let req = reqs.last().unwrap();
        assert_eq!(
            req.headers.get("Content-Encoding").unwrap(),
            "snappy",
            "block Snappy framing"
        );
        assert_eq!(
            req.headers
                .get("X-Prometheus-Remote-Write-Version")
                .unwrap(),
            "0.1.0"
        );
        let decoded = decode_body(&req.body);
        // 2 bucket series + _sum + _count + 2 progress gauges.
        let names: Vec<&str> = decoded
            .timeseries
            .iter()
            .map(|t| {
                t.labels
                    .iter()
                    .find(|l| l.name == "__name__")
                    .unwrap()
                    .value
                    .as_str()
            })
            .collect();
        for expected in [
            "http_req_duration_bucket",
            "http_req_duration_sum",
            "http_req_duration_count",
            "perfscale_iterations_completed",
            "perfscale_vus",
        ] {
            assert!(names.contains(&expected), "{expected} in {names:?}");
        }
        assert!(
            decoded
                .timeseries
                .iter()
                .all(|t| t.labels.iter().all(|l| l.name != "quantile")),
            "no client-side quantile series"
        );
        // Cumulative bucket counts, seconds bounds, merged labels.
        let buckets: Vec<&PromTimeSeries> = decoded
            .timeseries
            .iter()
            .filter(|t| {
                t.labels
                    .iter()
                    .any(|l| l.name == "__name__" && l.value == "http_req_duration_bucket")
            })
            .collect();
        assert_eq!(buckets.len(), 3, "le=0.01, le=0.05, +Inf");
        let at = |le: &str| {
            buckets
                .iter()
                .find(|t| t.labels.iter().any(|l| l.name == "le" && l.value == le))
                .unwrap_or_else(|| panic!("bucket le={le}"))
        };
        assert_eq!(at("0.01").samples[0].value, 3.0);
        assert_eq!(at("0.05").samples[0].value, 4.0);
        assert_eq!(at("+Inf").samples[0].value, 4.0);
        let ts = at("+Inf");
        assert_eq!(ts.samples[0].timestamp, 1_735_689_600_123);
        for (k, v) in [
            ("channel", "book"),
            ("env", "staging"),
            ("run_id", "run-7"),
            ("worker", "0"),
        ] {
            assert!(
                ts.labels.iter().any(|l| l.name == k && l.value == v),
                "label {k}={v} present: {:?}",
                ts.labels
            );
        }
    }

    #[tokio::test]
    async fn native_mode_sends_histograms_not_bucket_series() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let sink = PromRwSink::new(
            server.uri(),
            &BTreeMap::new(),
            Map::new(),
            10_000,
            Some("native"),
            "0".to_string(),
        )
        .unwrap();
        let cfg = DeliveryConfig {
            interval: Duration::from_millis(100),
            batch_size: 1000,
            max_pending: 8,
            retry: RetryPolicy::default(),
            final_timeout: Duration::from_secs(5),
            cpu_gate: None,
            counters: None,
        };
        let queue = Arc::new(SnapshotQueue::new("test/promnative".to_string(), 8));
        let worker = delivery::spawn(sink, cfg, Arc::clone(&queue));
        queue.push(snapshot());
        queue.close();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();

        let reqs = server.received_requests().await.unwrap();
        let decoded = decode_body(&reqs.last().unwrap().body);
        let hist = decoded
            .timeseries
            .iter()
            .find(|t| {
                t.labels
                    .iter()
                    .any(|l| l.name == "__name__" && l.value == "http_req_duration")
            })
            .expect("the native histogram series carries the plain name");
        assert_eq!(hist.histograms.len(), 1);
        let h = &hist.histograms[0];
        assert_eq!(h.count_int, 4);
        assert!((h.sum - 10.0 * 4.0 / 1000.0).abs() < 1e-9);
        assert_eq!(h.timestamp, 1_735_689_600_123);
        assert_eq!(h.reset_hint, 2, "no counter reset within a run");
        // The bucket counts recover the HDR distribution exactly.
        let mut acc = 0i64;
        let counts: Vec<i64> = h
            .positive_deltas
            .iter()
            .map(|d| {
                acc += d;
                acc
            })
            .collect();
        assert_eq!(counts.iter().sum::<i64>(), 4);
        assert!(
            decoded.timeseries.iter().all(|t| !t
                .labels
                .iter()
                .any(|l| l.name == "__name__" && l.value.ends_with("_bucket"))),
            "native mode emits no classic bucket series"
        );
    }

    #[tokio::test]
    async fn remote_write_retries_429_with_retry_after_and_drops_400() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let mk = || {
            PromRwSink::new(
                server.uri(),
                &BTreeMap::new(),
                Map::new(),
                10_000,
                None,
                "0".into(),
            )
            .unwrap()
        };
        let cfg = DeliveryConfig {
            interval: Duration::from_millis(100),
            batch_size: 1000,
            max_pending: 8,
            retry: RetryPolicy {
                max_attempts: 3,
                backoff_min: Duration::from_millis(50),
                backoff_max: Duration::from_millis(200),
            },
            final_timeout: Duration::from_secs(5),
            cpu_gate: None,
            counters: None,
        };
        let queue = Arc::new(SnapshotQueue::new("test/promretry".to_string(), 8));
        let worker = delivery::spawn(mk(), cfg, Arc::clone(&queue));
        queue.push(snapshot());
        queue.close();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "429 with Retry-After: 0 retries promptly"
        );
    }

    #[tokio::test]
    async fn unified_view_exports_labeled_aggregate_only_and_gpu_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // Mixed snapshot: one labeled series, one aggregate-only counter,
        // and GPU gauges.
        let mut snap = snapshot();
        snap.metrics.insert(
            "engine_log_channel_depth_max".to_string(),
            MetricAgg::counter(5.0),
        );
        snap.gpu = vec![crate::gpu::GpuSample {
            ts_ms: 7_000,
            index: 1,
            utilization_pct: Some(73.0),
            memory_used_mib: None,
            memory_total_mib: None,
            temperature_c: None,
            power_w: None,
            extra: BTreeMap::new(),
        }];
        let mut reporter_labels = Map::new();
        reporter_labels.insert("env".into(), Value::String("staging".into()));

        let sink = PromRwSink::new(
            server.uri(),
            &BTreeMap::new(),
            reporter_labels,
            10_000,
            None,
            "0".into(),
        )
        .unwrap();
        let cfg = DeliveryConfig {
            interval: Duration::from_secs(60),
            batch_size: 1000,
            max_pending: 8,
            retry: RetryPolicy::default(),
            final_timeout: Duration::from_secs(5),
            cpu_gate: None,
            counters: None,
        };
        let queue = Arc::new(SnapshotQueue::new("test/unified".to_string(), 8));
        let worker = delivery::spawn(sink, cfg, Arc::clone(&queue));
        queue.push(snap);
        queue.close();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let decoded = decode_body(&reqs[0].body);
        fn name(t: &PromTimeSeries) -> &str {
            t.labels
                .iter()
                .find(|l| l.name == "__name__")
                .unwrap()
                .value
                .as_str()
        }
        let names: Vec<&str> = decoded.timeseries.iter().map(name).collect();
        // Every metric exactly once: the labeled duration series (4 classic
        // series), the aggregate-only counter, the GPU gauge, the 2 progress
        // gauges. No duplicate from the metrics map (http_req_duration has a
        // series → not re-exported from the aggregate map).
        assert_eq!(
            names
                .iter()
                .filter(|n| n.starts_with("http_req_duration"))
                .count(),
            5,
            "2 buckets + +Inf + sum + count, once each: {names:?}"
        );
        let counter = decoded
            .timeseries
            .iter()
            .find(|t| name(t) == "engine_log_channel_depth_max_total")
            .expect("aggregate-only metric exported");
        assert_eq!(counter.samples[0].value, 5.0);
        assert!(
            counter
                .labels
                .iter()
                .any(|l| l.name == "env" && l.value == "staging"),
            "reporter labels on aggregate-only series"
        );
        assert!(counter
            .labels
            .iter()
            .any(|l| l.name == "run_id" && l.value == "run-7"));
        assert!(counter
            .labels
            .iter()
            .any(|l| l.name == "worker" && l.value == "0"));

        let gpu = decoded
            .timeseries
            .iter()
            .find(|t| name(t) == "gpu_utilization_pct")
            .expect("GPU gauge exported");
        assert_eq!(gpu.samples[0].value, 73.0);
        assert_eq!(gpu.samples[0].timestamp, 7_000, "collector's own ts");
        assert!(gpu.labels.iter().any(|l| l.name == "gpu" && l.value == "1"));
        assert!(gpu.labels.iter().any(|l| l.name == "run_id"));
        // N/A fields are skipped.
        assert!(!names.contains(&"gpu_memory_used_mib"));
    }

    #[tokio::test]
    async fn backend_receives_the_dropped_series_counter() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let counters = Arc::new(delivery::ReporterCounters::default());
        // max_series 2: of the snapshot's classic series (5 + 2 gauges), 5
        // drop — counted once per distinct series, even though the snapshot
        // is encoded twice (the absorb + the terminal flush re-encode).
        let sink = PromRwSink::new(
            server.uri(),
            &BTreeMap::new(),
            Map::new(),
            2,
            None,
            "0".into(),
        )
        .unwrap()
        .with_counters_registry(Arc::clone(&counters));
        let cfg = DeliveryConfig {
            interval: Duration::from_secs(60),
            batch_size: 1000,
            max_pending: 8,
            retry: RetryPolicy::default(),
            final_timeout: Duration::from_secs(5),
            cpu_gate: None,
            counters: Some(Arc::clone(&counters)),
        };
        let queue = Arc::new(SnapshotQueue::new("test/dropcounter".to_string(), 8));
        let worker = delivery::spawn(sink, cfg, Arc::clone(&queue));
        queue.push(snapshot());
        queue.close();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            counters.get(
                delivery::DROPPED_SERIES_METRIC,
                "std/report/prometheus-rw@v1"
            ),
            5,
            "idempotent: re-encoding the same snapshot does not double-count"
        );
        // The terminal flush refreshes the injected counters: the final
        // request already carries the current value — no manual re-inject.
        let reqs = server.received_requests().await.unwrap();
        let decoded = decode_body(&reqs.last().unwrap().body);
        let counter = decoded
            .timeseries
            .iter()
            .find(|t| {
                t.labels.iter().any(|l| {
                    l.name == "__name__" && l.value == "perfscale_report_dropped_series_total"
                })
            })
            .expect("the drop counter reached the backend");
        assert_eq!(counter.samples[0].value, 5.0);
        assert!(counter
            .labels
            .iter()
            .any(|l| l.name == "reporter" && l.value == "std/report/prometheus-rw@v1"));
    }
}
