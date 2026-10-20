//! The canonical `report@v1` data model and the built-in `json` / `junit`
//! report renderers (issue #9).
//!
//! # Contract
//!
//! `report@v1` is a public, stable contract: evolution is additive-only
//! within the major version. Two fields are reserved from day one so
//! streaming snapshots can land without a break: [`ReportV1::seq`] (always
//! `null` today) and [`ReportV1::samples`] (always empty today).
//!
//! # Semantics (mirrors the during-run channel, ADR-0007)
//!
//! - **Cumulative** since run start: quantiles and counts cover the whole
//!   run; windowing is left to consumers.
//! - **Units**: milliseconds inside the engine, **seconds on the wire**
//!   (Prometheus convention) — every duration/quantile/sample value here is
//!   seconds.
//! - **Self-describing metrics**: every family entry carries the registry
//!   metadata (`name`, `kind`, `unit`, `description`). Kinds follow the
//!   report vocabulary: a registry `Histogram` is a `sample` family here.
//!   Families declared but never recorded carry `recorded: false` and no
//!   series. Series keep their labels; gauges are point-in-time (last
//!   observed value plus its `ts`).
//! - **Raw samples** are opt-in per reporter (`max_samples`, default
//!   [`crate::metrics::MAX_STORED_STEP_SAMPLES`]); a step that recorded more
//!   samples than the reporter included is flagged `truncated: true`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::gpu::GpuSummary;
use crate::metrics::{catalog, MetricKind, Metrics};
use crate::step::thresholds::ThresholdsSummary;

/// Schema tag pinned at the top of every report document.
pub const SCHEMA_VERSION: &str = "report@v1";

/// The canonical run report. All duration values are **seconds**.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportV1 {
    /// Always `"report@v1"`.
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    pub run: RunIdentity,
    /// Per-step aggregates, in the test's declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepReport>,
    /// Threshold/SLO verdicts: every evaluated expression, pass or fail.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verdicts: Vec<Verdict>,
    /// Every registered metric family (built-in + custom), recorded or not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<MetricFamilyReport>,
    /// Reserved for future streaming snapshots (issue #12); always `null`
    /// in `report@v1` end-of-run documents.
    pub seq: Option<u64>,
    /// Reserved for future streaming snapshots (issue #12); always empty in
    /// `report@v1` end-of-run documents.
    pub samples: Vec<Value>,
}

/// Run identity: engine-generated `run_id` plus user labels (config/CLI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunIdentity {
    pub run_id: String,
    /// Engine that ran the test: `"native"`, `"k6"`, `"locust"`, `"jmeter"`.
    pub engine: String,
    /// perfscale version that produced the report.
    pub perfscale_version: String,
    /// Virtual users; `null` where the load shape lives outside the CLI's
    /// knowledge (k6 scripts, staged/arrival runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vus: Option<u32>,
    /// Run start, UTC ISO 8601.
    pub started_at: String,
    /// Wall-clock run length, seconds.
    pub duration_s: f64,
    /// User labels (config `report.labels` / CLI `--label`), interpolated.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub labels: Map<String, Value>,
}

/// Per-step aggregates. Duration values are seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepReport {
    /// Step `name:`, or the action id when the step has no name.
    pub name: String,
    /// Action id, e.g. `std/http@v1`.
    pub action: String,
    /// Invocations over the whole run.
    pub count: u64,
    /// Failed invocations.
    pub failed: u64,
    /// Invocations per second of wall-clock run time.
    pub rate: f64,
    pub duration: DurationAgg,
    /// Raw duration samples (seconds), present when the reporter opted in
    /// via `max_samples`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples: Option<RawSamples>,
}

/// Quantile block for a step (or a sample-kind series). Seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DurationAgg {
    pub avg: f64,
    pub min: f64,
    pub p50: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

/// Raw samples block: the values the reporter included, plus the truncation
/// marker when the step recorded more than fit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawSamples {
    /// Included duration samples, seconds, in record order.
    pub values: Vec<f64>,
    /// True when the step recorded more samples than `values` holds.
    pub truncated: bool,
    /// Total samples recorded (including any dropped past the cap).
    pub recorded: u64,
}

/// One threshold verdict: an expression, its outcome, the observed value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// The metric the expression evaluated over, e.g. `http_req_duration`.
    pub threshold: String,
    /// The expression as written, e.g. `p95<500`.
    pub expression: String,
    pub pass: bool,
    /// The aggregate's actual value (ms for duration metrics — thresholds
    /// speak milliseconds; fraction for `rate`, absolute for `count`).
    pub observed: f64,
}

/// One metric family: registry metadata plus its recorded series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricFamilyReport {
    pub name: String,
    /// Owning family (`http`, `llm`, …), `custom` for unregistered names.
    pub family: String,
    /// `sample` | `counter` | `rate` | `gauge`.
    pub kind: String,
    pub unit: String,
    pub description: String,
    /// False when the family was declared but nothing was recorded — the
    /// entry then carries no series.
    pub recorded: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<MetricSeries>,
}

/// One series of a family: a `(labels, value)` pair. Which value fields are
/// set depends on the family kind:
///
/// - `sample`: `quantiles` (seconds), `count`, `sum` (seconds);
/// - `counter`: `value`;
/// - `rate`: `total` (invocations), `failed`;
/// - `gauge`: `value` plus `ts` (point-in-time, RFC3339).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricSeries {
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub labels: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantiles: Option<BTreeMap<String, f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sum: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
}

/// Everything the builder needs beyond the metrics store.
pub struct ReportContext {
    pub run_id: String,
    pub engine: String,
    pub perfscale_version: String,
    pub vus: Option<u32>,
    /// Run start, UTC ISO 8601.
    pub started_at: String,
    pub wall_secs: f64,
    /// Interpolated user labels (config `report.labels` / CLI `--label`).
    pub labels: Map<String, Value>,
    /// Declared test steps as `(name, action)`, in order — report steps
    /// follow this order; aggregates under other names append after.
    pub step_order: Vec<(String, String)>,
    /// Iterations started (the `perfscale_iterations_completed` gauge).
    pub iterations: u64,
    /// Live/configured VU count at run end (the `perfscale_vus` gauge).
    pub vus_gauge: u64,
    /// Raw-sample cap applied per step; `None` omits the samples blocks.
    pub max_samples: Option<usize>,
}

/// Build the canonical report from the merged metrics store.
pub fn build_report_v1(
    m: &Metrics,
    thresholds: Option<&ThresholdsSummary>,
    gpu: Option<&GpuSummary>,
    ctx: &ReportContext,
) -> ReportV1 {
    ReportV1 {
        schema_version: SCHEMA_VERSION.to_string(),
        run: RunIdentity {
            run_id: ctx.run_id.clone(),
            engine: ctx.engine.clone(),
            perfscale_version: ctx.perfscale_version.clone(),
            vus: ctx.vus,
            started_at: ctx.started_at.clone(),
            duration_s: ctx.wall_secs,
            labels: ctx.labels.clone(),
        },
        steps: build_steps(m, ctx),
        verdicts: build_verdicts(thresholds),
        metrics: build_metrics(m, gpu, ctx),
        seq: None,
        samples: Vec::new(),
    }
}

/// HDR histograms store microseconds; the wire unit is seconds.
fn duration_agg(h: &hdrhistogram::Histogram<u64>) -> DurationAgg {
    let s = |v: u64| v as f64 / 1e6;
    DurationAgg {
        avg: h.mean() / 1e6,
        min: s(h.min()),
        p50: s(h.value_at_quantile(0.50)),
        p90: s(h.value_at_quantile(0.90)),
        p95: s(h.value_at_quantile(0.95)),
        p99: s(h.value_at_quantile(0.99)),
        max: s(h.max()),
    }
}

fn build_steps(m: &Metrics, ctx: &ReportContext) -> Vec<StepReport> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (name, action) in &ctx.step_order {
        push_step(m, ctx, &mut seen, &mut out, name, action);
    }
    // Aggregates under names not in the declaration (shouldn't happen,
    // but never drop data) append in key order.
    for name in m.step_aggs().keys() {
        if !seen.contains(name) {
            push_step(m, ctx, &mut seen, &mut out, name, "");
        }
    }
    out
}

fn push_step(
    m: &Metrics,
    ctx: &ReportContext,
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<StepReport>,
    name: &str,
    action: &str,
) {
    let Some(agg) = m.step_aggs().get(name) else {
        return;
    };
    seen.insert(name.to_string());
    let samples = ctx.max_samples.map(|cap| {
        let take = agg.samples.len().min(cap);
        RawSamples {
            values: agg.samples[..take].iter().map(|ms| ms / 1000.0).collect(),
            truncated: agg.samples_recorded as usize > take,
            recorded: agg.samples_recorded,
        }
    });
    out.push(StepReport {
        name: name.to_string(),
        action: action.to_string(),
        count: agg.count,
        failed: agg.failed,
        rate: agg.count as f64 / ctx.wall_secs.max(0.001),
        duration: duration_agg(&agg.durations_micros),
        samples,
    });
}

fn build_verdicts(thresholds: Option<&ThresholdsSummary>) -> Vec<Verdict> {
    thresholds
        .map(|t| {
            t.checks
                .iter()
                .map(|c| Verdict {
                    threshold: c.metric.clone(),
                    expression: c.expr.clone(),
                    pass: c.pass,
                    observed: c.actual,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn sample_series(h: &hdrhistogram::Histogram<u64>) -> MetricSeries {
    let d = duration_agg(h);
    let quantiles = BTreeMap::from([
        ("0.5".to_string(), d.p50),
        ("0.9".to_string(), d.p90),
        ("0.95".to_string(), d.p95),
        ("0.99".to_string(), d.p99),
    ]);
    MetricSeries {
        quantiles: Some(quantiles),
        count: Some(h.len() as f64),
        sum: Some(h.mean() * h.len() as f64 / 1e6),
        ..MetricSeries::default()
    }
}

fn kind_str(kind: MetricKind) -> &'static str {
    match kind {
        MetricKind::Histogram => "sample",
        MetricKind::Counter => "counter",
        MetricKind::Rate => "rate",
        MetricKind::Gauge => "gauge",
    }
}

fn build_metrics(
    m: &Metrics,
    gpu: Option<&GpuSummary>,
    ctx: &ReportContext,
) -> Vec<MetricFamilyReport> {
    let mut out = Vec::new();
    // Registered families first, in registration order.
    for (family, def) in catalog() {
        let series = lookup_series(m, def.name, ctx);
        out.push(MetricFamilyReport {
            name: def.name.to_string(),
            family: family.to_string(),
            kind: kind_str(def.kind).to_string(),
            unit: def.unit.to_string(),
            description: def.description.to_string(),
            recorded: series.is_some(),
            series: series.into_iter().collect(),
        });
    }
    // Recorded but unregistered names (implicit custom metrics) follow,
    // kind guessed from where they were recorded — the `custom` family.
    let registered: std::collections::HashSet<&str> =
        catalog().iter().map(|(_, d)| d.name).collect();
    let mut custom = |name: &str, kind: MetricKind, series: MetricSeries| {
        if registered.contains(name) {
            return;
        }
        out.push(MetricFamilyReport {
            name: name.to_string(),
            family: "custom".to_string(),
            kind: kind_str(kind).to_string(),
            unit: String::new(),
            description: String::new(),
            recorded: true,
            series: vec![series],
        });
    };
    for (name, h) in &m.hists {
        custom(name, MetricKind::Histogram, sample_series(h));
    }
    for (name, v) in &m.counters {
        if !m.rates.contains_key(name) {
            custom(
                name,
                MetricKind::Counter,
                MetricSeries {
                    value: Some(*v),
                    ..MetricSeries::default()
                },
            );
        }
    }
    for (name, v) in &m.gauges {
        custom(
            name,
            MetricKind::Gauge,
            MetricSeries {
                value: Some(*v),
                ..MetricSeries::default()
            },
        );
    }
    for (name, (total, failed)) in &m.rates {
        custom(
            name,
            MetricKind::Rate,
            MetricSeries {
                total: Some(*total as f64),
                failed: Some(*failed as f64),
                ..MetricSeries::default()
            },
        );
    }
    // GPU gauges: point-in-time series per device (`gpu="<index>"` label),
    // each with the collector's own timestamp of the last sample.
    if let Some(g) = gpu {
        let mut gauge =
            |name: &str, unit: &str, pick: fn(&crate::gpu::GpuSample) -> Option<f64>| {
                let series: Vec<MetricSeries> = g
                    .devices
                    .iter()
                    .filter_map(|d| {
                        let last = d
                            .samples
                            .iter()
                            .rev()
                            .find_map(|s| pick(s).map(|v| (v, s.ts_ms)))?;
                        Some(MetricSeries {
                            labels: Map::from_iter([(
                                "gpu".to_string(),
                                Value::String(d.index.to_string()),
                            )]),
                            value: Some(last.0),
                            ts: Some(crate::report::stream::epoch_ms_to_rfc3339(last.1 as i64)),
                            ..MetricSeries::default()
                        })
                    })
                    .collect();
                if series.is_empty() {
                    return;
                }
                out.push(MetricFamilyReport {
                    name: name.to_string(),
                    family: "gpu".to_string(),
                    kind: "gauge".to_string(),
                    unit: unit.to_string(),
                    description: format!("GPU gauge reported by {}", g.source),
                    recorded: true,
                    series,
                });
            };
        gauge("gpu_utilization_pct", "%", |s| s.utilization_pct);
        gauge("gpu_memory_used_mib", "MiB", |s| s.memory_used_mib);
        gauge("gpu_memory_total_mib", "MiB", |s| s.memory_total_mib);
        gauge("gpu_temperature_c", "°C", |s| s.temperature_c);
        gauge("gpu_power_w", "W", |s| s.power_w);
    }
    out
}

/// The recorded series of a registered family, or `None` when nothing was
/// recorded for it this run.
fn lookup_series(m: &Metrics, name: &str, ctx: &ReportContext) -> Option<MetricSeries> {
    match name {
        "http_req_duration" if m.total > 0 => Some(sample_series(&m.durations_micros)),
        "http_req_failed" if m.total > 0 => Some(MetricSeries {
            total: Some(m.total as f64),
            failed: Some(m.failures as f64),
            ..MetricSeries::default()
        }),
        "http_reqs" if m.total > 0 => Some(MetricSeries {
            value: Some(m.total as f64),
            ..MetricSeries::default()
        }),
        "perfscale_iterations_completed" => Some(MetricSeries {
            value: Some(ctx.iterations as f64),
            ..MetricSeries::default()
        }),
        "perfscale_vus" => Some(MetricSeries {
            value: Some(ctx.vus_gauge as f64),
            ..MetricSeries::default()
        }),
        _ => {
            if let Some(h) = m.hists.get(name) {
                return Some(sample_series(h));
            }
            if let Some((total, failed)) = m.rates.get(name) {
                return Some(MetricSeries {
                    total: Some(*total as f64),
                    failed: Some(*failed as f64),
                    ..MetricSeries::default()
                });
            }
            if let Some(v) = m.counters.get(name) {
                return Some(MetricSeries {
                    value: Some(*v),
                    ..MetricSeries::default()
                });
            }
            m.gauges.get(name).map(|v| MetricSeries {
                value: Some(*v),
                ..MetricSeries::default()
            })
        }
    }
}

impl ReportV1 {
    /// Canonical JSON dump — the `std/report/json@v1` renderer.
    pub fn to_json(&self) -> String {
        let mut out = serde_json::to_string_pretty(self).expect("report is always serializable");
        out.push('\n');
        out
    }

    /// Apply one reporter's raw-sample cap in place: `0` drops the samples
    /// blocks entirely, otherwise each step keeps its first `max_samples`
    /// values and re-flags `truncated` accordingly.
    pub fn truncate_samples(&mut self, max_samples: usize) {
        for step in &mut self.steps {
            step.samples = step.samples.take().and_then(|mut raw| {
                if max_samples == 0 {
                    return None;
                }
                raw.values.truncate(max_samples);
                raw.truncated = raw.recorded as usize > raw.values.len();
                Some(raw)
            });
        }
    }

    /// JUnit XML — the `std/report/junit@v1` renderer: the testsuite is the
    /// run, each step is a testcase, and every failed threshold verdict adds
    /// a failing `threshold: …` testcase, so a CI gate reads the file alone.
    pub fn to_junit_xml(&self) -> String {
        let failed_verdicts = self.verdicts.iter().filter(|v| !v.pass).count();
        let tests = self.steps.len() + self.verdicts.len();
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        xml.push_str(&format!(
            "<testsuite name=\"perfscale run {}\" tests=\"{}\" failures=\"{}\" time=\"{:.3}\">\n",
            esc(&self.run.run_id),
            tests,
            failed_verdicts,
            self.run.duration_s,
        ));
        for step in &self.steps {
            xml.push_str(&format!(
                "  <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\"/>\n",
                esc(&step.action),
                esc(&step.name),
                step.duration.avg,
            ));
        }
        for verdict in &self.verdicts {
            let name = format!("{} {}", verdict.threshold, verdict.expression);
            if verdict.pass {
                xml.push_str(&format!(
                    "  <testcase classname=\"threshold\" name=\"{}\"/>\n",
                    esc(&name),
                ));
            } else {
                xml.push_str(&format!(
                    "  <testcase classname=\"threshold\" name=\"{}\">\n    <failure message=\"observed {}\">{} {} → observed {}</failure>\n  </testcase>\n",
                    esc(&name),
                    esc(&verdict.observed.to_string()),
                    esc(&verdict.threshold),
                    esc(&verdict.expression),
                    esc(&verdict.observed.to_string()),
                ));
            }
        }
        xml.push_str("</testsuite>\n");
        xml
    }
}

/// XML-escape the five predefined entities.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::actions::HttpSample;

    fn ctx() -> ReportContext {
        ReportContext {
            run_id: "run-1".into(),
            engine: "native".into(),
            perfscale_version: "0.0.0-test".into(),
            vus: Some(2),
            started_at: "2026-10-20T00:00:00Z".into(),
            wall_secs: 10.0,
            labels: Map::new(),
            step_order: vec![("hit".into(), "std/http@v1".into())],
            iterations: 40,
            vus_gauge: 2,
            max_samples: Some(3),
        }
    }

    fn store_with_traffic() -> Metrics {
        let mut m = Metrics::default();
        for i in 0..10u64 {
            m.record(&HttpSample {
                duration_ms: 1.0 + i as f64,
                status: 200,
                failed: i % 5 == 4,
            });
            m.record_step("hit", 1.0 + i as f64, i % 5 == 4);
        }
        m
    }

    #[test]
    fn report_carries_identity_steps_and_metrics() {
        let m = store_with_traffic();
        let r = build_report_v1(&m, None, None, &ctx());
        assert_eq!(r.schema_version, "report@v1");
        assert_eq!(r.run.run_id, "run-1");
        assert_eq!(r.steps.len(), 1);
        let s = &r.steps[0];
        assert_eq!(s.name, "hit");
        assert_eq!(s.action, "std/http@v1");
        assert_eq!(s.count, 10);
        assert_eq!(s.failed, 2);
        assert!((s.rate - 1.0).abs() < 1e-9);
        // Raw samples capped at max_samples=3 and flagged truncated.
        let raw = s.samples.as_ref().expect("samples included");
        assert_eq!(raw.values.len(), 3);
        assert!(raw.truncated);
        assert_eq!(raw.recorded, 10);
        // Seconds on the wire.
        assert!(raw.values.iter().all(|v| *v < 0.1), "{:?}", raw.values);

        // Registered families: the recorded http triple plus unrecorded
        // families flagged `recorded: false`.
        let by_name: BTreeMap<&str, &MetricFamilyReport> =
            r.metrics.iter().map(|f| (f.name.as_str(), f)).collect();
        assert_eq!(by_name["http_req_duration"].kind, "sample");
        assert!(by_name["http_req_duration"].recorded);
        let q = by_name["http_req_duration"].series[0]
            .quantiles
            .as_ref()
            .unwrap();
        assert!(*q.get("0.95").unwrap() < 0.1, "seconds on the wire: {q:?}");
        assert_eq!(by_name["http_reqs"].series[0].value, Some(10.0));
        assert_eq!(by_name["http_req_failed"].series[0].failed, Some(2.0));
        assert!(!by_name["llm_ttft_ms"].recorded);
        assert!(by_name["llm_ttft_ms"].series.is_empty());
        assert_eq!(by_name["perfscale_vus"].series[0].value, Some(2.0));
    }

    #[test]
    fn unregistered_recorded_names_land_in_the_custom_family() {
        let mut m = Metrics::default();
        m.add_counters(
            serde_json::json!({ "my_counter": 3.0, "my_hist": [10.0] })
                .as_object()
                .unwrap(),
        );
        let r = build_report_v1(&m, None, None, &ctx());
        let by_name: BTreeMap<&str, &MetricFamilyReport> =
            r.metrics.iter().map(|f| (f.name.as_str(), f)).collect();
        assert_eq!(by_name["my_counter"].family, "custom");
        assert_eq!(by_name["my_counter"].kind, "counter");
        assert_eq!(by_name["my_hist"].kind, "sample");
        assert!(by_name["my_hist"].recorded);
    }

    #[test]
    fn verdicts_cover_pass_and_fail() {
        let t = ThresholdsSummary {
            status: "fail".into(),
            message: "x".into(),
            violations: vec![crate::step::thresholds::ThresholdViolation {
                metric: "http_req_duration".into(),
                expr: "p95<0".into(),
                actual: 10.0,
            }],
            checks: vec![
                crate::step::thresholds::ThresholdCheck {
                    metric: "http_req_duration".into(),
                    expr: "p95<0".into(),
                    actual: 10.0,
                    pass: false,
                },
                crate::step::thresholds::ThresholdCheck {
                    metric: "http_req_failed".into(),
                    expr: "rate<0.05".into(),
                    actual: 0.0,
                    pass: true,
                },
            ],
        };
        let r = build_report_v1(&Metrics::default(), Some(&t), None, &ctx());
        assert_eq!(r.verdicts.len(), 2);
        assert!(!r.verdicts[0].pass);
        assert!(r.verdicts[1].pass);

        let xml = r.to_junit_xml();
        assert!(xml.contains("failures=\"1\""), "{xml}");
        assert!(xml.contains("<failure"), "{xml}");
        assert!(xml.contains("classname=\"threshold\""), "{xml}");
    }

    #[test]
    fn json_round_trips_and_pins_reserved_fields() {
        let m = store_with_traffic();
        let r = build_report_v1(&m, None, None, &ctx());
        let json = r.to_json();
        assert!(json.contains("\"schemaVersion\": \"report@v1\""), "{json}");
        assert!(json.contains("\"seq\": null"), "{json}");
        assert!(json.contains("\"samples\": []"), "{json}");
        let back: ReportV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn junit_escapes_xml_and_marks_steps() {
        let m = store_with_traffic();
        let mut c = ctx();
        c.step_order = vec![("a<b>".into(), "std/http@v1".into())];
        let mut m2 = Metrics::default();
        m2.record_step("a<b>", 1.0, false);
        let _ = m;
        let r = build_report_v1(&m2, None, None, &c);
        let xml = r.to_junit_xml();
        assert!(xml.contains("name=\"a&lt;b&gt;\""), "{xml}");
        assert!(xml.contains("tests=\"1\""), "{xml}");
    }

    #[test]
    fn gauges_carry_last_value_and_ts() {
        let g = GpuSummary {
            source: "nvidia-smi".into(),
            interval_ms: 1000,
            devices: vec![crate::gpu::GpuDeviceSummary {
                index: 0,
                samples: vec![
                    crate::gpu::GpuSample {
                        ts_ms: 1000,
                        index: 0,
                        utilization_pct: Some(10.0),
                        memory_used_mib: None,
                        memory_total_mib: None,
                        temperature_c: None,
                        power_w: None,
                        extra: BTreeMap::new(),
                    },
                    crate::gpu::GpuSample {
                        ts_ms: 2000,
                        index: 0,
                        utilization_pct: Some(42.0),
                        memory_used_mib: None,
                        memory_total_mib: None,
                        temperature_c: None,
                        power_w: None,
                        extra: BTreeMap::new(),
                    },
                ],
                avg_utilization_pct: Some(26.0),
                max_utilization_pct: Some(42.0),
                max_memory_used_mib: None,
                memory_total_mib: None,
                max_temperature_c: None,
                max_power_w: None,
            }],
        };
        let r = build_report_v1(&Metrics::default(), None, Some(&g), &ctx());
        let util = r
            .metrics
            .iter()
            .find(|f| f.name == "gpu_utilization_pct")
            .expect("gpu gauge present");
        assert_eq!(util.kind, "gauge");
        let s = &util.series[0];
        assert_eq!(s.labels["gpu"], "0");
        assert_eq!(s.value, Some(42.0), "last value wins");
        assert_eq!(s.ts.as_deref(), Some("1970-01-01T00:00:02.000Z"));
    }
}
