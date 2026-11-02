//! `std/report/otel@v1` — OTLP/gRPC metrics streaming reporter (issue #12).
//!
//! Each cumulative [`MetricSnapshot`] encodes to an
//! `ExportMetricsServiceRequest` (cumulative temporality, correct
//! start/time UnixNano) and is sent to the collector at `with.endpoint` over
//! gRPC. Delivery (bounded drop-oldest queue, batching, retries, final
//! flush) is the shared [`super::delivery`] layer; only transient gRPC
//! statuses (Unavailable, ResourceExhausted, DeadlineExceeded) and network
//! errors retry — everything else is poison.
//!
//! # Mapping
//!
//! - counters/rates → monotonic cumulative `Sum` (`<name>_total`,
//!   `<name>_failed_total`), gauges → `Gauge`, durations →
//!   **ExponentialHistogram** (bucket counts + sum + count, attributes from
//!   the merged labels). No client-side quantile series.
//!
//! The protobuf messages are hand-written prost structs (pure Rust, no
//! protoc); field numbers match `opentelemetry/proto/metrics/v1/metrics.proto`
//! and friends. The gRPC client is `tonic::client::Grpc` with a small
//! hand-written codec — no codegen.

use std::collections::BTreeMap;

use futures_util::future::BoxFuture;
use prost::Message;
use serde_json::{Map, Value};

use super::delivery::{self, PostOutcome, Sink};
use super::serve::capped_series_keys;
use super::stream::{merge_labels, MetricSnapshot, SeriesSnapshot};
use crate::step::thresholds::MetricKind;

/// Exponential-histogram scale: base 2^(2^-scale) bucket growth. Scale 3
/// (≈9% relative width) matches the engine's 2-significant-digit HDR
/// histograms without exploding the bucket count.
const EXP_SCALE: i32 = 3;

const METRICS_SERVICE_EXPORT: &str =
    "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export";

// ---------------------------------------------------------------------------
// OTLP protobuf messages (hand-written prost, official field numbers).
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct OtlpAnyValue {
    #[prost(oneof = "otlp_any_value::Value", tags = "1")]
    pub value: Option<otlp_any_value::Value>,
}

pub mod otlp_any_value {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Value {
        #[prost(string, tag = "1")]
        StringValue(String),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpKeyValue {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(message, optional, tag = "2")]
    pub value: Option<OtlpAnyValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpResource {
    #[prost(message, repeated, tag = "1")]
    pub attributes: Vec<OtlpKeyValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpInstrumentationScope {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub version: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpNumberDataPoint {
    #[prost(message, repeated, tag = "7")]
    pub attributes: Vec<OtlpKeyValue>,
    #[prost(fixed64, tag = "2")]
    pub start_time_unix_nano: u64,
    #[prost(fixed64, tag = "3")]
    pub time_unix_nano: u64,
    #[prost(oneof = "otlp_number_data_point::Value", tags = "4, 6")]
    pub value: Option<otlp_number_data_point::Value>,
}

pub mod otlp_number_data_point {
    #[derive(Clone, Copy, PartialEq, prost::Oneof)]
    pub enum Value {
        #[prost(double, tag = "4")]
        AsDouble(f64),
        #[prost(sfixed64, tag = "6")]
        AsInt(i64),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpGauge {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<OtlpNumberDataPoint>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpSum {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<OtlpNumberDataPoint>,
    /// 2 = AGGREGATION_TEMPORALITY_CUMULATIVE.
    #[prost(int32, tag = "2")]
    pub aggregation_temporality: i32,
    #[prost(bool, tag = "3")]
    pub is_monotonic: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpBuckets {
    #[prost(sint32, tag = "1")]
    pub offset: i32,
    #[prost(uint64, repeated, tag = "2")]
    pub bucket_counts: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpExponentialHistogramDataPoint {
    #[prost(message, repeated, tag = "1")]
    pub attributes: Vec<OtlpKeyValue>,
    #[prost(fixed64, tag = "2")]
    pub start_time_unix_nano: u64,
    #[prost(fixed64, tag = "3")]
    pub time_unix_nano: u64,
    #[prost(fixed64, tag = "4")]
    pub count: u64,
    #[prost(double, optional, tag = "5")]
    pub sum: Option<f64>,
    #[prost(sint32, tag = "6")]
    pub scale: i32,
    #[prost(fixed64, tag = "7")]
    pub zero_count: u64,
    #[prost(message, optional, tag = "8")]
    pub positive: Option<OtlpBuckets>,
    #[prost(uint32, tag = "10")]
    pub flags: u32,
    #[prost(double, tag = "14")]
    pub zero_threshold: f64,
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpExponentialHistogram {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<OtlpExponentialHistogramDataPoint>,
    /// 2 = AGGREGATION_TEMPORALITY_CUMULATIVE.
    #[prost(int32, tag = "2")]
    pub aggregation_temporality: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpMetric {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "3")]
    pub unit: String,
    #[prost(oneof = "otlp_metric::Data", tags = "5, 7, 10")]
    pub data: Option<otlp_metric::Data>,
}

pub mod otlp_metric {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Data {
        #[prost(message, tag = "5")]
        Gauge(super::OtlpGauge),
        #[prost(message, tag = "7")]
        Sum(super::OtlpSum),
        #[prost(message, tag = "10")]
        ExponentialHistogram(super::OtlpExponentialHistogram),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpScopeMetrics {
    #[prost(message, optional, tag = "1")]
    pub scope: Option<OtlpInstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    pub metrics: Vec<OtlpMetric>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpResourceMetrics {
    #[prost(message, optional, tag = "1")]
    pub resource: Option<OtlpResource>,
    #[prost(message, repeated, tag = "2")]
    pub scope_metrics: Vec<OtlpScopeMetrics>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OtlpExportMetricsServiceRequest {
    #[prost(message, repeated, tag = "1")]
    pub resource_metrics: Vec<OtlpResourceMetrics>,
}

/// `ExportMetricsServiceResponse` (no fields we read; `partial_success`
/// fields 1+ are ignored).
#[derive(Clone, PartialEq, Message)]
pub struct OtlpExportMetricsServiceResponse {}

// ---------------------------------------------------------------------------
// Hand-written gRPC codec (tonic::client::Grpc, no codegen).
// ---------------------------------------------------------------------------

struct OtlpEncoder;

impl tonic::codec::Encoder for OtlpEncoder {
    type Item = OtlpExportMetricsServiceRequest;
    type Error = tonic::Status;

    fn encode(
        &mut self,
        item: Self::Item,
        dst: &mut tonic::codec::EncodeBuf<'_>,
    ) -> Result<(), Self::Error> {
        item.encode(dst)
            .map_err(|e| tonic::Status::internal(format!("OTLP request encode failed: {e}")))
    }
}

struct OtlpDecoder;

impl tonic::codec::Decoder for OtlpDecoder {
    type Item = OtlpExportMetricsServiceResponse;
    type Error = tonic::Status;

    fn decode(
        &mut self,
        src: &mut tonic::codec::DecodeBuf<'_>,
    ) -> Result<Option<Self::Item>, Self::Error> {
        OtlpExportMetricsServiceResponse::decode(src)
            .map(Some)
            .map_err(|e| tonic::Status::internal(format!("OTLP response decode failed: {e}")))
    }
}

struct OtlpCodec;

impl tonic::codec::Codec for OtlpCodec {
    type Encode = OtlpExportMetricsServiceRequest;
    type Decode = OtlpExportMetricsServiceResponse;
    type Encoder = OtlpEncoder;
    type Decoder = OtlpDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        OtlpEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        OtlpDecoder
    }
}

// ---------------------------------------------------------------------------
// Sink
// ---------------------------------------------------------------------------

/// One encoded data point — batching unit; grouped into metrics at send.
#[derive(Debug)]
pub enum OtlpPoint {
    Number {
        kind: NumberKind,
        point: OtlpNumberDataPoint,
    },
    ExpHistogram(OtlpExponentialHistogramDataPoint),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NumberKind {
    /// Monotonic cumulative Sum (counters, rate totals).
    Sum,
    Gauge,
}

#[derive(Debug)]
pub struct OtlpItem {
    name: String,
    /// Series identity for the max_series guard.
    series_key: (String, BTreeMap<String, String>),
    point: OtlpPoint,
}

/// The OTLP/gRPC sink.
pub struct OtelSink {
    endpoint: String,
    /// Reporter-level labels (already interpolated).
    labels: Map<String, Value>,
    max_series: usize,
    worker: String,
    /// Per-run counter registry for the max_series guard.
    counters: Option<std::sync::Arc<delivery::ReporterCounters>>,
    channel: Option<tonic::transport::Channel>,
}

impl OtelSink {
    pub fn new(
        endpoint: String,
        labels: Map<String, Value>,
        max_series: usize,
        worker: String,
    ) -> Self {
        Self {
            endpoint,
            labels,
            max_series,
            worker,
            counters: None,
            channel: None,
        }
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

fn kv(key: &str, value: &str) -> OtlpKeyValue {
    OtlpKeyValue {
        key: key.to_string(),
        value: Some(OtlpAnyValue {
            value: Some(otlp_any_value::Value::StringValue(value.to_string())),
        }),
    }
}

fn attrs(labels: &BTreeMap<String, String>) -> Vec<OtlpKeyValue> {
    labels.iter().map(|(k, v)| kv(k, v)).collect()
}

fn number_point(
    labels: &BTreeMap<String, String>,
    start_ms: i64,
    ts_ms: i64,
    value: f64,
) -> OtlpNumberDataPoint {
    OtlpNumberDataPoint {
        attributes: attrs(labels),
        start_time_unix_nano: (start_ms.max(0) as u64) * 1_000_000,
        time_unix_nano: (ts_ms.max(0) as u64) * 1_000_000,
        value: Some(otlp_number_data_point::Value::AsDouble(value)),
    }
}

/// HDR buckets (ms, per-bucket counts) → an OTLP exponential-histogram data
/// point (seconds): a dense positive bucket array from the lowest to the
/// highest occupied index.
fn exp_histogram_point(
    s: &SeriesSnapshot,
    labels: &BTreeMap<String, String>,
    start_ms: i64,
    ts_ms: i64,
) -> OtlpExponentialHistogramDataPoint {
    let factor = (1u32 << EXP_SCALE) as f64;
    let mut by_index: BTreeMap<i32, u64> = BTreeMap::new();
    for b in &s.buckets {
        let le_s = b.le_ms / 1000.0;
        if le_s <= 0.0 || b.count == 0 {
            continue;
        }
        let idx = (le_s.log2() * factor).ceil() as i32 - 1;
        *by_index.entry(idx).or_insert(0) += b.count;
    }
    let positive = if let (Some(&first), Some(&last)) =
        (by_index.keys().next(), by_index.keys().next_back())
    {
        let mut counts = vec![0u64; (last - first + 1) as usize];
        for (idx, n) in &by_index {
            counts[(idx - first) as usize] = *n;
        }
        Some(OtlpBuckets {
            offset: first,
            bucket_counts: counts,
        })
    } else {
        None
    };
    OtlpExponentialHistogramDataPoint {
        attributes: attrs(labels),
        start_time_unix_nano: (start_ms.max(0) as u64) * 1_000_000,
        time_unix_nano: (ts_ms.max(0) as u64) * 1_000_000,
        count: s.agg.count as u64,
        // avg (ms) × count → seconds.
        sum: Some(s.agg.avg * s.agg.count / 1000.0),
        scale: EXP_SCALE,
        zero_count: 0,
        positive,
        flags: 0,
        zero_threshold: 0.0,
    }
}

/// Encode one engine series into OTLP data points.
fn expand_series(
    s: &SeriesSnapshot,
    labels: &BTreeMap<String, String>,
    snap: &MetricSnapshot,
    out: &mut Vec<OtlpItem>,
) {
    let key = |name: &str| (name.to_string(), labels.clone());
    match s.agg.kind {
        MetricKind::Sample => out.push(OtlpItem {
            name: s.name.clone(),
            series_key: key(&s.name),
            point: OtlpPoint::ExpHistogram(exp_histogram_point(
                s,
                labels,
                snap.started_ms,
                snap.ts_ms,
            )),
        }),
        MetricKind::Counter => {
            let kind = if s.gauge {
                NumberKind::Gauge
            } else {
                NumberKind::Sum
            };
            out.push(OtlpItem {
                name: if s.gauge {
                    s.name.clone()
                } else {
                    format!("{}_total", s.name)
                },
                series_key: key(&s.name),
                point: OtlpPoint::Number {
                    kind,
                    point: number_point(labels, snap.started_ms, snap.ts_ms, s.agg.count),
                },
            });
        }
        MetricKind::Rate => {
            for (suffix, value) in [
                ("_total", s.agg.count),
                ("_failed_total", s.agg.rate * s.agg.count),
            ] {
                out.push(OtlpItem {
                    name: format!("{}{suffix}", s.name),
                    series_key: key(&format!("{}{suffix}", s.name)),
                    point: OtlpPoint::Number {
                        kind: NumberKind::Sum,
                        point: number_point(labels, snap.started_ms, snap.ts_ms, value),
                    },
                });
            }
        }
    }
}

impl Sink for OtelSink {
    type Item = OtlpItem;

    fn reporter(&self) -> &str {
        "std/report/otel@v1"
    }

    fn encode(&mut self, snap: &MetricSnapshot) -> Vec<OtlpItem> {
        // Unified no-duplicates view: every labeled series, then every
        // aggregate-only metric (a name with no series) as an unlabeled
        // bucketless series, then progress gauges and GPU gauges.
        let mut out: Vec<OtlpItem> = Vec::new();
        let mut series_names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for s in &snap.series {
            series_names.insert(s.name.as_str());
            let labels = merge_labels(
                &snap.labels,
                &self.labels,
                &s.labels,
                &snap.run_id,
                &self.worker,
            );
            expand_series(s, &labels, snap, &mut out);
        }
        // Aggregate-only metrics (no labeled series): counters/rates as
        // sums — sample-kind metrics without buckets carry `_count`/`_sum`.
        let empty = BTreeMap::new();
        for (name, agg) in &snap.metrics {
            if series_names.contains(name.as_str()) {
                continue;
            }
            let labels = merge_labels(
                &snap.labels,
                &self.labels,
                &empty,
                &snap.run_id,
                &self.worker,
            );
            let s = SeriesSnapshot {
                name: name.clone(),
                labels: BTreeMap::new(),
                agg: *agg,
                gauge: false,
                buckets: Vec::new(),
            };
            expand_series(&s, &labels, snap, &mut out);
        }
        // Run-progress gauges ride every snapshot.
        let empty = BTreeMap::new();
        let labels = merge_labels(
            &snap.labels,
            &self.labels,
            &empty,
            &snap.run_id,
            &self.worker,
        );
        for (name, value) in [
            (
                "perfscale_iterations_completed",
                snap.iterations_completed as f64,
            ),
            ("perfscale_vus", snap.vus as f64),
        ] {
            out.push(OtlpItem {
                name: name.to_string(),
                series_key: (name.to_string(), labels.clone()),
                point: OtlpPoint::Number {
                    kind: NumberKind::Gauge,
                    point: number_point(&labels, snap.started_ms, snap.ts_ms, value),
                },
            });
        }
        // GPU gauges: point-in-time, per device, collector timestamps.
        for g in &snap.gpu {
            let mut labels = merge_labels(
                &snap.labels,
                &self.labels,
                &empty,
                &snap.run_id,
                &self.worker,
            );
            labels.insert("gpu".to_string(), g.index.to_string());
            for (name, v) in super::prometheus_rw::gpu_gauges(g) {
                out.push(OtlpItem {
                    name: name.to_string(),
                    series_key: (name.to_string(), labels.clone()),
                    point: OtlpPoint::Number {
                        kind: NumberKind::Gauge,
                        point: number_point(&labels, g.ts_ms as i64, g.ts_ms as i64, v),
                    },
                });
            }
        }
        // max_series guard: deterministic drop; the reporter's own counters
        // are exempt.
        let (exempt, guarded): (Vec<OtlpItem>, Vec<OtlpItem>) = out
            .into_iter()
            .partition(|i| delivery::is_reporter_counter(&i.name));
        let keep = capped_series_keys(
            self.reporter(),
            guarded.iter().map(|i| i.series_key.clone()).collect(),
            self.max_series,
            &self.counters,
        );
        let mut kept: Vec<OtlpItem> = guarded
            .into_iter()
            .filter(|i| keep.contains(&i.series_key))
            .collect();
        kept.extend(exempt);
        kept
    }

    fn send<'a>(&'a mut self, items: &'a [OtlpItem], _seq: u64) -> BoxFuture<'a, PostOutcome> {
        Box::pin(async move {
            if self.channel.is_none() {
                let endpoint = match tonic::transport::Endpoint::new(self.endpoint.clone()) {
                    Ok(e) => e,
                    Err(e) => return PostOutcome::Poison(format!("invalid OTLP endpoint: {e}")),
                };
                match endpoint.connect().await {
                    Ok(ch) => self.channel = Some(ch),
                    Err(e) => {
                        return PostOutcome::Retry {
                            after: None,
                            reason: format!("connect failed: {e}"),
                        }
                    }
                }
            }
            let channel = self.channel.clone().expect("connected above");

            // Group data points into metrics by (name, data kind).
            #[derive(PartialEq, Eq, PartialOrd, Ord)]
            enum KindTag {
                Gauge,
                Sum,
                ExpHistogram,
            }
            let mut metrics: BTreeMap<(String, KindTag), OtlpMetric> = BTreeMap::new();
            for item in items {
                match &item.point {
                    OtlpPoint::Number { kind, point } => {
                        let tag = match kind {
                            NumberKind::Gauge => KindTag::Gauge,
                            NumberKind::Sum => KindTag::Sum,
                        };
                        let entry =
                            metrics
                                .entry((item.name.clone(), tag))
                                .or_insert_with(|| OtlpMetric {
                                    name: item.name.clone(),
                                    unit: String::new(),
                                    data: Some(match kind {
                                        NumberKind::Gauge => otlp_metric::Data::Gauge(OtlpGauge {
                                            data_points: Vec::new(),
                                        }),
                                        NumberKind::Sum => otlp_metric::Data::Sum(OtlpSum {
                                            data_points: Vec::new(),
                                            aggregation_temporality: 2,
                                            is_monotonic: true,
                                        }),
                                    }),
                                });
                        match entry.data.as_mut().expect("set above") {
                            otlp_metric::Data::Gauge(g) => g.data_points.push(point.clone()),
                            otlp_metric::Data::Sum(s) => s.data_points.push(point.clone()),
                            _ => unreachable!("kind tag matches the data variant"),
                        }
                    }
                    OtlpPoint::ExpHistogram(point) => {
                        let entry = metrics
                            .entry((item.name.clone(), KindTag::ExpHistogram))
                            .or_insert_with(|| OtlpMetric {
                                name: item.name.clone(),
                                unit: "s".to_string(),
                                data: Some(otlp_metric::Data::ExponentialHistogram(
                                    OtlpExponentialHistogram {
                                        data_points: Vec::new(),
                                        aggregation_temporality: 2,
                                    },
                                )),
                            });
                        match entry.data.as_mut().expect("set above") {
                            otlp_metric::Data::ExponentialHistogram(h) => {
                                h.data_points.push(point.clone())
                            }
                            _ => unreachable!("kind tag matches the data variant"),
                        }
                    }
                }
            }
            let request = OtlpExportMetricsServiceRequest {
                resource_metrics: vec![OtlpResourceMetrics {
                    resource: Some(OtlpResource {
                        attributes: vec![
                            kv("service.name", "perfscale"),
                            kv("service.version", env!("CARGO_PKG_VERSION")),
                        ],
                    }),
                    scope_metrics: vec![OtlpScopeMetrics {
                        scope: Some(OtlpInstrumentationScope {
                            name: "perfscale".to_string(),
                            version: env!("CARGO_PKG_VERSION").to_string(),
                        }),
                        metrics: metrics.into_values().collect(),
                    }],
                }],
            };

            let mut grpc = tonic::client::Grpc::new(channel);
            let path = http::uri::PathAndQuery::from_static(METRICS_SERVICE_EXPORT);
            // Drive readiness before the call (tonic's generated clients do
            // the same — `unary` itself does not).
            if let Err(e) = grpc.ready().await {
                self.channel = None;
                return PostOutcome::Retry {
                    after: None,
                    reason: format!("channel not ready: {e}"),
                };
            }
            match grpc
                .unary(tonic::Request::new(request), path, OtlpCodec)
                .await
            {
                Ok(_) => PostOutcome::Delivered,
                Err(status) => match status.code() {
                    tonic::Code::Unavailable
                    | tonic::Code::ResourceExhausted
                    | tonic::Code::DeadlineExceeded => PostOutcome::Retry {
                        after: None,
                        reason: format!("gRPC status {}", status.code()),
                    },
                    // A broken pipe poisons the channel but is retryable.
                    tonic::Code::Unknown => {
                        self.channel = None;
                        PostOutcome::Retry {
                            after: None,
                            reason: format!("gRPC status {}", status.code()),
                        }
                    }
                    code => PostOutcome::Poison(format!("gRPC status {code}")),
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::stream::BucketCount;
    use crate::step::thresholds::MetricAgg;

    fn series(buckets: &[(f64, u64)]) -> SeriesSnapshot {
        SeriesSnapshot {
            name: "lat".to_string(),
            labels: BTreeMap::from([("channel".to_string(), "book".to_string())]),
            agg: MetricAgg {
                kind: MetricKind::Sample,
                avg: 10.0,
                min: 1.0,
                max: 100.0,
                p50: 8.0,
                p90: 20.0,
                p95: 30.0,
                p99: 40.0,
                count: buckets.iter().map(|b| b.1).sum::<u64>() as f64,
                rate: 0.0,
            },
            gauge: false,
            buckets: buckets
                .iter()
                .map(|(le_ms, count)| BucketCount {
                    le_ms: *le_ms,
                    count: *count,
                })
                .collect(),
        }
    }

    #[test]
    fn exp_histogram_point_counts_sum_and_attributes() {
        let s = series(&[(1.0, 2), (10.0, 3), (100.0, 1)]);
        let labels = BTreeMap::from([("channel".to_string(), "book".to_string())]);
        let p = exp_histogram_point(&s, &labels, 1_000, 2_000);
        assert_eq!(p.count, 6, "count = total bucket samples");
        assert_eq!(p.sum, Some(10.0 * 6.0 / 1000.0), "sum in seconds");
        assert_eq!(p.start_time_unix_nano, 1_000 * 1_000_000);
        assert_eq!(p.time_unix_nano, 2_000 * 1_000_000);
        assert_eq!(p.scale, EXP_SCALE);
        let buckets = p.positive.expect("positive buckets present");
        let total: u64 = buckets.bucket_counts.iter().sum();
        assert_eq!(total, 6, "every sample lands in exactly one bucket");
        assert!(p.attributes.iter().any(|a| a.key == "channel"
            && matches!(
                &a.value,
                Some(OtlpAnyValue {
                    value: Some(otlp_any_value::Value::StringValue(v))
                }) if v == "book"
            )));
    }

    #[test]
    fn request_encodes_and_decodes_with_cumulative_temporality() {
        let s = series(&[(10.0, 4)]);
        let mut out = Vec::new();
        let snap_labels = Map::new();
        let labels = BTreeMap::from([("run_id".to_string(), "r1".to_string())]);
        let snap = MetricSnapshot {
            ts_ms: 2_000,
            metrics: BTreeMap::new(),
            iterations_completed: 0,
            vus: 0,
            gpu: Vec::new(),
            run_id: "r1".into(),
            started_ms: 1_000,
            labels: snap_labels,
            series: vec![],
            terminal: false,
        };
        expand_series(&s, &labels, &snap, &mut out);
        // Rebuild the same grouping the sender does, minimally.
        let point = match &out[0].point {
            OtlpPoint::ExpHistogram(p) => p.clone(),
            _ => panic!("sample metric → exponential histogram"),
        };
        let req = OtlpExportMetricsServiceRequest {
            resource_metrics: vec![OtlpResourceMetrics {
                resource: Some(OtlpResource {
                    attributes: vec![kv("service.name", "perfscale")],
                }),
                scope_metrics: vec![OtlpScopeMetrics {
                    scope: None,
                    metrics: vec![OtlpMetric {
                        name: "lat".into(),
                        unit: "s".into(),
                        data: Some(otlp_metric::Data::ExponentialHistogram(
                            OtlpExponentialHistogram {
                                data_points: vec![point],
                                aggregation_temporality: 2,
                            },
                        )),
                    }],
                }],
            }],
        };
        let bytes = req.encode_to_vec();
        let back = OtlpExportMetricsServiceRequest::decode(bytes.as_slice()).unwrap();
        let m = &back.resource_metrics[0].scope_metrics[0].metrics[0];
        match m.data.as_ref().unwrap() {
            otlp_metric::Data::ExponentialHistogram(h) => {
                assert_eq!(h.aggregation_temporality, 2, "cumulative");
                assert_eq!(h.data_points[0].count, 4);
                assert_eq!(h.data_points[0].start_time_unix_nano, 1_000_000_000);
            }
            _ => panic!("exponential histogram"),
        }
    }
}

#[cfg(test)]
mod wire_tests {
    use std::collections::BTreeMap;
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::Map;

    use super::*;
    use crate::report::delivery::{self, DeliveryConfig, RetryPolicy, SnapshotQueue};
    use crate::report::stream::BucketCount;
    use crate::step::thresholds::MetricAgg;

    /// A hand-rolled OTLP collector: decodes the gRPC frame + protobuf of
    /// every Export call (trailers-only response). `fail_first` calls answer
    /// with gRPC UNAVAILABLE, then it succeeds — transient retries land.
    #[derive(Clone, Default)]
    struct CollectorSvc {
        requests: Arc<Mutex<Vec<OtlpExportMetricsServiceRequest>>>,
        fail_first: Arc<std::sync::atomic::AtomicU32>,
    }

    impl tonic::server::NamedService for CollectorSvc {
        const NAME: &'static str = "opentelemetry.proto.collector.metrics.v1.MetricsService";
    }

    impl tower::Service<http::Request<tonic::body::Body>> for CollectorSvc {
        type Response = http::Response<tonic::body::Body>;
        type Error = Infallible;
        type Future = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Self::Response, Infallible>> + Send>,
        >;

        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Infallible>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<tonic::body::Body>) -> Self::Future {
            let requests = Arc::clone(&self.requests);
            let fail_first = Arc::clone(&self.fail_first);
            Box::pin(async move {
                use http_body_util::BodyExt as _;
                let payload = req
                    .into_body()
                    .collect()
                    .await
                    .expect("readable body")
                    .to_bytes();
                let mut rest = &payload[..];
                // Skip each 5-byte gRPC frame prefix (1 compressed + 4 len).
                while rest.len() >= 5 {
                    let len = u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
                    let msg = &rest[5..5 + len];
                    let decoded = OtlpExportMetricsServiceRequest::decode(msg)
                        .expect("OTLP protobuf decodes");
                    requests.lock().unwrap().push(decoded);
                    rest = &rest[5 + len..];
                }
                let status = if fail_first
                    .try_update(
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                        |n| n.checked_sub(1),
                    )
                    .is_ok()
                {
                    "14" // UNAVAILABLE — transient, must retry
                } else {
                    "0"
                };
                // Trailers-only response: grpc-status rides the headers.
                let resp = http::Response::builder()
                    .status(200)
                    .header("content-type", "application/grpc")
                    .header("grpc-status", status)
                    .body(tonic::body::Body::default())
                    .unwrap();
                Ok(resp)
            })
        }
    }

    async fn start_collector() -> (String, CollectorSvc, tokio::task::JoinHandle<()>) {
        let svc = CollectorSvc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tonic::transport::Server::builder()
            .add_service(svc.clone())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener));
        let handle = tokio::spawn(async move {
            let _ = server.await;
        });
        (format!("http://{addr}"), svc, handle)
    }

    fn snapshot() -> MetricSnapshot {
        let series = vec![
            SeriesSnapshot {
                name: "http_req_duration".to_string(),
                labels: BTreeMap::from([("channel".to_string(), "book".to_string())]),
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
            },
            SeriesSnapshot {
                name: "db_rows".to_string(),
                labels: BTreeMap::new(),
                agg: MetricAgg::counter(7.0),
                gauge: false,
                buckets: Vec::new(),
            },
            SeriesSnapshot {
                name: "db_query_failed".to_string(),
                labels: BTreeMap::new(),
                agg: MetricAgg::rate(10, 2),
                gauge: false,
                buckets: Vec::new(),
            },
            SeriesSnapshot {
                name: "engine_log_channel_depth_max".to_string(),
                labels: BTreeMap::new(),
                agg: MetricAgg::counter(3.0),
                gauge: true,
                buckets: Vec::new(),
            },
        ];
        MetricSnapshot {
            ts_ms: 1_735_689_600_123,
            metrics: BTreeMap::new(),
            iterations_completed: 9,
            vus: 1,
            gpu: Vec::new(),
            run_id: "run-8".into(),
            started_ms: 1_735_689_000_000,
            labels: Map::new(),
            series,
            terminal: false,
        }
    }

    fn metric<'a>(req: &'a OtlpExportMetricsServiceRequest, name: &str) -> &'a OtlpMetric {
        req.resource_metrics[0].scope_metrics[0]
            .metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("metric {name} present"))
    }

    #[tokio::test]
    async fn otlp_retries_transient_unavailable_statuses() {
        let (endpoint, svc, _server) = start_collector().await;
        svc.fail_first
            .store(2, std::sync::atomic::Ordering::Relaxed);
        let sink = OtelSink::new(endpoint, Map::new(), 10_000, "0".to_string());
        // A tick that never fires during the test: only the final flush
        // sends, deterministically.
        let cfg = DeliveryConfig {
            interval: Duration::from_secs(60),
            batch_size: 1000,
            max_pending: 8,
            retry: RetryPolicy {
                max_attempts: 4,
                backoff_min: Duration::from_millis(50),
                backoff_max: Duration::from_millis(100),
            },
            final_timeout: Duration::from_secs(5),
            cpu_gate: None,
            counters: None,
        };
        let queue = Arc::new(SnapshotQueue::new("test/otelretry".to_string(), 8));
        let worker = delivery::spawn(sink, cfg, Arc::clone(&queue));
        queue.push(snapshot());
        queue.close();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();
        // Two UNAVAILABLE responses, then the terminal batch lands — the
        // collector decoded every attempt's protobuf.
        let n = svc.requests.lock().unwrap().len();
        assert_eq!(n, 3, "2 transient failures + 1 success");
        assert!(svc
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| !r.resource_metrics.is_empty()));
    }

    #[tokio::test]
    async fn otlp_grpc_exports_sums_gauges_and_exponential_histograms() {
        let (endpoint, svc, _server) = start_collector().await;
        let sink = OtelSink::new(endpoint, Map::new(), 10_000, "0".to_string());
        let cfg = DeliveryConfig {
            interval: Duration::from_millis(100),
            batch_size: 1000,
            max_pending: 8,
            retry: RetryPolicy::default(),
            final_timeout: Duration::from_secs(5),
            cpu_gate: None,
            counters: None,
        };
        let queue = Arc::new(SnapshotQueue::new("test/otel".to_string(), 8));
        let worker = delivery::spawn(sink, cfg, Arc::clone(&queue));
        queue.push(snapshot());
        queue.close();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();

        // Closed before the first tick: the coalesced terminal flush is the
        // only export.
        let requests = svc.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let req = requests.last().unwrap();
        // Resource/scope identity.
        let resource = req.resource_metrics[0].resource.as_ref().unwrap();
        assert!(resource.attributes.iter().any(|a| a.key == "service.name"));
        let scope = req.resource_metrics[0].scope_metrics[0]
            .scope
            .as_ref()
            .unwrap();
        assert_eq!(scope.name, "perfscale");

        // Duration → cumulative exponential histogram, seconds, attributes.
        let h = match metric(req, "http_req_duration").data.as_ref().unwrap() {
            otlp_metric::Data::ExponentialHistogram(h) => h,
            other => panic!("http_req_duration is an exponential histogram: {other:?}"),
        };
        assert_eq!(h.aggregation_temporality, 2, "cumulative");
        let p = &h.data_points[0];
        assert_eq!(p.count, 4);
        assert!((p.sum.unwrap() - 10.0 * 4.0 / 1000.0).abs() < 1e-9);
        assert_eq!(p.start_time_unix_nano, 1_735_689_000_000 * 1_000_000);
        assert_eq!(p.time_unix_nano, 1_735_689_600_123 * 1_000_000);
        let buckets = p.positive.as_ref().unwrap();
        assert_eq!(buckets.bucket_counts.iter().sum::<u64>(), 4);
        assert!(p.attributes.iter().any(|a| a.key == "channel"));
        assert!(
            p.attributes
                .iter()
                .any(|a| a.key == "run_id" && matches!(&a.value, Some(OtlpAnyValue { value: Some(otlp_any_value::Value::StringValue(v)) }) if v == "run-8"))
        );
        assert!(p.attributes.iter().any(|a| a.key == "worker"));

        // Counter → monotonic cumulative Sum.
        let sum = match metric(req, "db_rows_total").data.as_ref().unwrap() {
            otlp_metric::Data::Sum(s) => s,
            other => panic!("db_rows_total is a Sum: {other:?}"),
        };
        assert!(sum.is_monotonic);
        assert_eq!(sum.aggregation_temporality, 2);
        assert!(matches!(
            sum.data_points[0].value,
            Some(otlp_number_data_point::Value::AsDouble(v)) if v == 7.0
        ));
        // Rate → `_total` + `_failed_total` sums.
        let failed = metric(req, "db_query_failed_failed_total");
        match failed.data.as_ref().unwrap() {
            otlp_metric::Data::Sum(s) => assert!(matches!(
                s.data_points[0].value,
                Some(otlp_number_data_point::Value::AsDouble(v)) if v == 2.0
            )),
            other => panic!("rate total is a Sum: {other:?}"),
        }
        // Gauge → Gauge; progress gauges ride along.
        let gauge = match metric(req, "engine_log_channel_depth_max")
            .data
            .as_ref()
            .unwrap()
        {
            otlp_metric::Data::Gauge(g) => g,
            other => panic!("gauge metric is a Gauge: {other:?}"),
        };
        assert!(matches!(
            gauge.data_points[0].value,
            Some(otlp_number_data_point::Value::AsDouble(v)) if v == 3.0
        ));
        assert!(matches!(
            metric(req, "perfscale_vus").data.as_ref().unwrap(),
            otlp_metric::Data::Gauge(_)
        ));
    }
}
