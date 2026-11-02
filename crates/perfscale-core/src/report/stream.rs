//! During-run metrics streaming: snapshots out of the engine, batched
//! samples into the collector.
//!
//! # Data flow
//!
//! `run_native` (when its `metrics_tx` is `Some` and the run config's
//! `report.during_run` is set) sends a cumulative [`MetricSnapshot`] every
//! `report.interval_ms`. A streaming reporter ([`super::serve`],
//! [`super::prometheus_rw`], [`super::otel`]) receives them through the
//! shared bounded delivery layer ([`super::delivery`]); the serve reporter
//! converts each to [`Sample`]s via [`snapshot_to_samples`] and POSTs
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
//!   Every snapshot also carries two run-progress gauges,
//!   `perfscale_iterations_completed` and `perfscale_vus`, so a live run
//!   never flattens to an empty batch (LLM runs record no samples until
//!   their first request completes). GPU gauges ship per device as `gpu_utilization_pct`,
//!   `gpu_memory_used_mib`, `gpu_memory_total_mib`, `gpu_temperature_c` and
//!   `gpu_power_w`, each with a `gpu="<index>"` label (plus any
//!   collector-specific extras under their own names).
//! - **Backpressure**: the engine drops snapshots when the channel is full
//!   (never slowing the VU loop); downstream, each reporter's bounded queue
//!   drops the OLDEST snapshot on overflow (counted in
//!   `perfscale_report_dropped_snapshots_total{reporter}`) — cumulative
//!   values recover on the next snapshot. See [`super::delivery`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::step::thresholds::{MetricAgg, MetricKind};

/// Backward-compatible re-exports: the auth types live in [`super::serve`]
/// since issue #12 made the serve shipper a reporter.
pub use super::serve::{Auth, TokenProvider};

/// A point-in-time, cumulative view of every metric family the run collected.
///
/// Cumulative: the underlying HDR histograms and counters never reset during
/// a run, so values grow monotonically and percentiles converge. `ts_ms` is
/// strictly increasing across a run's snapshots.
///
/// `iterations_completed`/`vus` are run-progress gauges: they ride every
/// snapshot so a run whose steps record no request samples yet (e.g. an LLM
/// run waiting on its first 60s completion) still flattens to a non-empty
/// batch — the stream is never silent while the run is alive.
///
/// Issue #12 extended the snapshot additively: `run_id`/`started_ms`/`labels`
/// carry the run identity, and `series` the label-aware per-(name, labels)
/// view with HDR bucket counts for bucketed exporters (Prometheus
/// classic/native, OTLP exponential). The legacy `metrics` aggregate map is
/// unchanged, so existing consumers (the serve shipper) keep working.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSnapshot {
    /// Snapshot time, milliseconds since the Unix epoch.
    pub ts_ms: i64,
    /// Aggregates per metric name, as of `ts_ms`.
    pub metrics: BTreeMap<String, MetricAgg>,
    /// Iterations started since run start (monotonic).
    #[serde(default)]
    pub iterations_completed: u64,
    /// Live VU count at snapshot time (configured count on fixed runs).
    #[serde(default)]
    pub vus: u64,
    /// GPU gauge samples taken since the previous snapshot (empty when the
    /// run has no `gpu:` section). Unlike `metrics` these are NOT
    /// cumulative: each carries its own `ts_ms`, and consumers chart them
    /// directly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu: Vec<crate::gpu::GpuSample>,
    /// Engine-generated run id, stamped by the engine — never user-settable.
    #[serde(default)]
    pub run_id: String,
    /// Run start, milliseconds since the Unix epoch (cumulative temporality
    /// start for the OTLP/native-histogram exporters).
    #[serde(default)]
    pub started_ms: i64,
    /// Run-level labels (`report.labels`), interpolated before the run.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub labels: Map<String, Value>,
    /// Label-aware series: the same data as `metrics`, keyed by
    /// `(name, step labels)` and carrying HDR bucket counts for sample-kind
    /// series. Empty on engines that predate issue #12.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<SeriesSnapshot>,
    /// True on the terminal cumulative snapshot the engine emits right
    /// before the channel closes — the final flush guarantees it lands.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub terminal: bool,
}

/// One label-aware series in a [`MetricSnapshot`] (issue #12): the aggregate
/// numbers plus — for sample-kind series — the HDR bucket counts the
/// bucketed exporters emit. No client-side quantile series: backends compute
/// correct global quantiles from the buckets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesSnapshot {
    /// Metric name, e.g. `http_req_duration`.
    pub name: String,
    /// The step's labels (empty when the metric is not step-attributed).
    /// Reporter and run labels merge at export time (step > reporter > run).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Cumulative aggregates (same values as the `metrics` map entry).
    #[serde(flatten)]
    pub agg: MetricAgg,
    /// True for point-in-time gauges (last write wins), false for monotonic
    /// counters — the OTLP exporter maps these to Gauge vs monotonic Sum.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gauge: bool,
    /// Per-bucket counts of the duration histogram (sample-kind series
    /// only): `(upper bound, count)` pairs, non-cumulative, milliseconds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buckets: Vec<BucketCount>,
}

/// One non-empty HDR histogram bucket: count of samples whose value falls
/// in `(lowest_equivalent(le_ms), le_ms]`, milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BucketCount {
    /// Bucket upper bound, milliseconds.
    pub le_ms: f64,
    /// Samples in this bucket (NOT cumulative).
    pub count: u64,
}

/// Merge the three label levels with priority step > reporter > run, then
/// stamp the reserved engine labels (`run_id`, `worker`) — they always win,
/// user configuration can never override them (rejected at lint/run time).
pub fn merge_labels(
    run: &Map<String, Value>,
    reporter: &Map<String, Value>,
    step: &BTreeMap<String, String>,
    run_id: &str,
    worker: &str,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut overlay = |m: &Map<String, Value>| {
        for (k, v) in m {
            out.insert(k.clone(), label_value_to_string(v));
        }
    };
    overlay(run);
    overlay(reporter);
    out.extend(step.iter().map(|(k, v)| (k.clone(), v.clone())));
    out.insert("run_id".to_string(), run_id.to_string());
    out.insert("worker".to_string(), worker.to_string());
    out
}

/// A label value renders as a string: strings as-is, numbers/booleans in
/// their plain form (interpolation already resolved `${{ … }}` tokens).
pub fn label_value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Resolve `${{ inputs.* }}` and `${{ env.* }}` placeholders in label values
/// — once, before the run starts. A miss resolves to an empty string (the
/// same lenient rule as non-strict step interpolation); everything else
/// stays verbatim.
pub fn interpolate_label_values(labels: &Map<String, Value>, inputs: &Value) -> Map<String, Value> {
    labels
        .iter()
        .map(|(k, v)| {
            let resolved = match v {
                Value::String(s) => Value::String(interpolate_label_str(s, inputs)),
                other => other.clone(),
            };
            (k.clone(), resolved)
        })
        .collect()
}

fn interpolate_label_str(s: &str, inputs: &Value) -> String {
    let Some(mut start) = s.find("${{") else {
        return s.to_string();
    };
    let mut rest = s;
    let mut out = String::with_capacity(s.len() + 16);
    loop {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let expr = after[..end].trim();
        let resolved = if let Some(name) = expr.strip_prefix("env.") {
            std::env::var(name).unwrap_or_default()
        } else if let Some(path) = expr.strip_prefix("inputs.") {
            crate::step::json_path::parse(&format!(".{path}"))
                .ok()
                .and_then(|segments| crate::step::json_path::walk(inputs, &segments))
                .map(|v| label_value_to_string(&v))
                .unwrap_or_default()
        } else {
            // Unknown namespaces stay verbatim — label interpolation covers
            // only inputs/env.
            format!("${{{{ {expr} }}}}")
        };
        out.push_str(&resolved);
        rest = &after[end + 2..];
        match rest.find("${{") {
            Some(next) => start = next,
            None => {
                out.push_str(rest);
                return out;
            }
        }
    }
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
pub(crate) fn epoch_ms_to_rfc3339(ms: i64) -> String {
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
    // Run-progress gauges ride every snapshot: plain names (no `_total` —
    // they are charted as-is, never diffed), so even a snapshot with zero
    // recorded samples proves the run is alive.
    sample(
        "perfscale_iterations_completed".to_string(),
        None,
        snap.iterations_completed as f64,
    );
    sample("perfscale_vus".to_string(), None, snap.vus as f64);
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

/// Label-aware flatten (issue #12): one sample set per `(name, step labels)`
/// series, with the run and reporter labels merged underneath (step >
/// reporter > run). Aggregate-only names (metrics with no labeled series —
/// producers that never went through the labeled fold) are still exported
/// once, unlabeled. A counter series shadowed by a same-key rate series is
/// skipped (same rule as the aggregate snapshot). Run-progress and GPU
/// gauges ride along exactly as in [`snapshot_to_samples`].
///
/// `extra_label` stamps one extra label on every sample (serve's `task_id`).
/// Reserved engine labels (`run_id`, `worker`) are NOT stamped here — the
/// serve ingestion shape predates them; the new exporters add them
/// themselves.
pub fn snapshot_to_samples_labeled(
    snap: &MetricSnapshot,
    reporter_labels: &Map<String, Value>,
    extra_label: Option<(&str, &str)>,
) -> Vec<Sample> {
    let mut out = Vec::new();
    let finish = |out: &mut Vec<Sample>,
                  metric: String,
                  labels: Map<String, Value>,
                  value: f64,
                  ts_ms: i64| {
        let mut labels = labels;
        if let Some((k, v)) = extra_label {
            labels.insert(k.to_string(), Value::String(v.to_string()));
        }
        out.push(Sample {
            metric,
            labels,
            value,
            ts_ms,
        });
    };
    let merged = |step: &BTreeMap<String, String>| -> Map<String, Value> {
        let mut m = snap.labels.clone();
        for (k, v) in reporter_labels {
            m.insert(k.clone(), v.clone());
        }
        for (k, v) in step {
            m.insert(k.clone(), Value::String(v.clone()));
        }
        m
    };
    let rate_keys: std::collections::BTreeSet<(&str, &BTreeMap<String, String>)> = snap
        .series
        .iter()
        .filter(|s| s.agg.kind == MetricKind::Rate)
        .map(|s| (s.name.as_str(), &s.labels))
        .collect();
    let mut seen_names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for s in &snap.series {
        seen_names.insert(s.name.as_str());
        // A counter shadowed by a same-key rate series is skipped (the rate
        // view answers both use cases).
        if s.agg.kind == MetricKind::Counter
            && !s.gauge
            && rate_keys.contains(&(s.name.as_str(), &s.labels))
        {
            continue;
        }
        let labels = merged(&s.labels);
        match s.agg.kind {
            MetricKind::Sample => {
                for (q, v) in [
                    ("0.5", s.agg.p50),
                    ("0.9", s.agg.p90),
                    ("0.95", s.agg.p95),
                    ("0.99", s.agg.p99),
                ] {
                    let mut l = labels.clone();
                    l.insert("quantile".to_string(), Value::String(q.to_string()));
                    finish(&mut out, s.name.clone(), l, v / 1000.0, snap.ts_ms);
                }
                finish(
                    &mut out,
                    format!("{}_count", s.name),
                    labels.clone(),
                    s.agg.count,
                    snap.ts_ms,
                );
                finish(
                    &mut out,
                    format!("{}_sum", s.name),
                    labels,
                    s.agg.avg * s.agg.count / 1000.0,
                    snap.ts_ms,
                );
            }
            MetricKind::Counter => {
                // The legacy flatten ships counters AND gauges (the aggregate
                // snapshot surfaces gauges as counters) with the `_total`
                // suffix — keep that shape.
                finish(
                    &mut out,
                    format!("{}_total", s.name),
                    labels,
                    s.agg.count,
                    snap.ts_ms,
                );
            }
            MetricKind::Rate => {
                finish(
                    &mut out,
                    format!("{}_total", s.name),
                    labels.clone(),
                    s.agg.count,
                    snap.ts_ms,
                );
                finish(
                    &mut out,
                    format!("{}_failed_total", s.name),
                    labels,
                    s.agg.rate * s.agg.count,
                    snap.ts_ms,
                );
            }
        }
    }
    // Aggregate-only metrics (no labeled series): exported once, unlabeled.
    let empty = BTreeMap::new();
    for (name, agg) in &snap.metrics {
        if seen_names.contains(name.as_str()) {
            continue;
        }
        let labels = merged(&empty);
        match agg.kind {
            MetricKind::Sample => {
                for (q, v) in [
                    ("0.5", agg.p50),
                    ("0.9", agg.p90),
                    ("0.95", agg.p95),
                    ("0.99", agg.p99),
                ] {
                    let mut l = labels.clone();
                    l.insert("quantile".to_string(), Value::String(q.to_string()));
                    finish(&mut out, name.clone(), l, v / 1000.0, snap.ts_ms);
                }
                finish(
                    &mut out,
                    format!("{name}_count"),
                    labels.clone(),
                    agg.count,
                    snap.ts_ms,
                );
                finish(
                    &mut out,
                    format!("{name}_sum"),
                    labels,
                    agg.avg * agg.count / 1000.0,
                    snap.ts_ms,
                );
            }
            MetricKind::Counter => finish(
                &mut out,
                format!("{name}_total"),
                labels,
                agg.count,
                snap.ts_ms,
            ),
            MetricKind::Rate => {
                finish(
                    &mut out,
                    format!("{name}_total"),
                    labels.clone(),
                    agg.count,
                    snap.ts_ms,
                );
                finish(
                    &mut out,
                    format!("{name}_failed_total"),
                    labels,
                    agg.rate * agg.count,
                    snap.ts_ms,
                );
            }
        }
    }
    // Progress gauges and GPU gauges: identical to the legacy flatten.
    let base = merged(&empty);
    finish(
        &mut out,
        "perfscale_iterations_completed".to_string(),
        base.clone(),
        snap.iterations_completed as f64,
        snap.ts_ms,
    );
    finish(
        &mut out,
        "perfscale_vus".to_string(),
        base,
        snap.vus as f64,
        snap.ts_ms,
    );
    for g in &snap.gpu {
        let mut gpu_sample = |metric: &str, value: f64| {
            let mut labels = merged(&empty);
            labels.insert("gpu".to_string(), Value::String(g.index.to_string()));
            finish(&mut out, metric.to_string(), labels, value, g.ts_ms as i64);
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

#[cfg(test)]
mod tests {
    use super::*;

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
            iterations_completed: 3,
            vus: 2,
            gpu: Vec::new(),
            run_id: String::new(),
            started_ms: 0,
            labels: Map::new(),
            series: Vec::new(),
            terminal: false,
        };
        let mut labels = Map::new();
        labels.insert("run".to_string(), Value::String("r1".into()));

        let samples = snapshot_to_samples(&snap, &labels);
        // 6 (lat) + 1 (rows_total) + 2 (req_failed_*) + 2 progress gauges = 11.
        assert_eq!(samples.len(), 11);
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

        // Run-progress gauges: plain gauge names (no `_total`), as-is values.
        let iters = find("perfscale_iterations_completed", None);
        assert_eq!(iters.value, 3.0);
        assert_eq!(iters.ts_ms, 42);
        assert_eq!(iters.labels["run"], "r1");
        assert_eq!(find("perfscale_vus", None).value, 2.0);
    }

    /// The gauges are the liveness contract: a snapshot whose steps recorded
    /// nothing (a pure-LLM run before its first completion) still flattens
    /// to a non-empty batch, so the shipper never goes silent mid-run.
    #[test]
    fn snapshot_to_samples_empty_metrics_still_carries_progress_gauges() {
        let snap = MetricSnapshot {
            ts_ms: 42,
            metrics: BTreeMap::new(),
            iterations_completed: 5,
            vus: 3,
            gpu: Vec::new(),
            run_id: String::new(),
            started_ms: 0,
            labels: Map::new(),
            series: Vec::new(),
            terminal: false,
        };
        let samples = snapshot_to_samples(&snap, &Map::new());
        assert_eq!(samples.len(), 2, "{samples:?}");
        assert_eq!(samples[0].metric, "perfscale_iterations_completed");
        assert_eq!(samples[0].value, 5.0);
        assert_eq!(samples[1].metric, "perfscale_vus");
        assert_eq!(samples[1].value, 3.0);
        assert!(samples.iter().all(|s| s.ts_ms == 42));
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
            iterations_completed: 0,
            vus: 1,
            gpu: vec![g0, g1],
            run_id: String::new(),
            started_ms: 0,
            labels: Map::new(),
            series: Vec::new(),
            terminal: false,
        };
        let mut labels = Map::new();
        labels.insert("task_id".to_string(), Value::String("t1".into()));

        let samples = snapshot_to_samples(&snap, &labels);
        // g0: 5 built-ins + 1 extra; g1: 2 built-ins (util + mem used);
        // plus the 2 run-progress gauges.
        assert_eq!(samples.len(), 10, "{samples:?}");
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
}
