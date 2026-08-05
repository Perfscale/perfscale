//! `std/thresholds@v1` — run-level SLO gates over the metrics a run collected.
//!
//! Designed for `after:` blocks: the step evaluates k6-style threshold
//! expressions against aggregates computed over **all** samples emitted
//! during the run, once, after every VU has stopped:
//!
//! ```yaml
//! after:
//!   - name: slo gate
//!     use: std/thresholds@v1
//!     with:
//!       db_query_duration: ["p95<500", "max<2000"]
//!       db_query_failed: ["rate<0.05"]
//!       db_errors: ["count==0"]
//!     severity: fail            # fail (default) | warn | info
//!     message: "checkout SLO"   # optional, interpolated
//! ```
//!
//! Config errors (unknown metric, unparseable expression, empty `with`) are
//! hard failures: the gate is recorded as `fail` in the run summary, so a
//! broken gate can never silently pass CI.
//!
//! # Expressions
//!
//! `<agg><op><number>` — agg ∈ `avg,min,max,p50,p90,p95,p99,count,rate`;
//! op ∈ `<,<=,>,>=,==,!=`; the number is a plain float (int or decimal, no
//! units). Whitespace around the parts is tolerated.
//!
//! # Metric kinds
//!
//! - **sample metrics** (`db_query_duration`, `http_req_duration`,
//!   `ws_msg_rtt`, …): `avg/min/max/p50/p90/p95/p99` come from the same HDR
//!   histograms the text summary prints, so gate numbers match the summary;
//!   `count` is the number of samples.
//! - **counter metrics** (`db_errors`, `db_rows`, …): only `count`, the
//!   counter's final value.
//! - **rate metrics** (`http_req_failed`, `db_query_failed`, …): `rate` is
//!   failed/total invocations in `0.0..=1.0`; `count` is the invocation
//!   count. See [`super::runner`] for how `<family>_failed` samples are
//!   recorded.
//!
//! # Failure sampling
//!
//! There is no per-invocation 0/1 metric in the step outputs — steps emit
//! duration samples and counters. The runner derives failure samples
//! generically: for every histogram (array-valued) metric an invocation
//! emits, it records one 0/1 sample under `<family>_failed` (the metric name
//! with a trailing `_duration`/`_rtt` stripped, plus `_failed`), 1 when the
//! step invocation failed and 0 when it succeeded. `rate` over those samples
//! is therefore exactly failed/total invocations of that step family.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::step::actions::{ActionOutput, LogTag};
use crate::step::context::Context;

/// Maximum length of the step/run-summary message; longer messages are
/// truncated and terminated with `…` so a wall of violations cannot flood
/// the UI.
pub const MAX_MESSAGE_CHARS: usize = 200;

// ---------------------------------------------------------------------------
// Expression parsing
// ---------------------------------------------------------------------------

/// Aggregate an expression compares: `avg`, `min`, `max`, `p50`, `p90`,
/// `p95`, `p99`, `count`, or `rate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Avg,
    Min,
    Max,
    P50,
    P90,
    P95,
    P99,
    Count,
    Rate,
}

impl Agg {
    pub fn as_str(&self) -> &'static str {
        match self {
            Agg::Avg => "avg",
            Agg::Min => "min",
            Agg::Max => "max",
            Agg::P50 => "p50",
            Agg::P90 => "p90",
            Agg::P95 => "p95",
            Agg::P99 => "p99",
            Agg::Count => "count",
            Agg::Rate => "rate",
        }
    }
}

/// Comparison operator: `<`, `<=`, `>`, `>=`, `==`, `!=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl Op {
    pub fn as_str(&self) -> &'static str {
        match self {
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Eq => "==",
            Op::Ne => "!=",
        }
    }

    /// The operator as shown in a violation message — the negation, read as
    /// "the actual value ended up on this side of the threshold"
    /// (`p95<500` violated renders as `p95=612ms ≥ 500ms`).
    fn negated(&self) -> &'static str {
        match self {
            Op::Lt => "≥",
            Op::Le => ">",
            Op::Gt => "≤",
            Op::Ge => "<",
            Op::Eq => "≠",
            Op::Ne => "=",
        }
    }

    fn holds(&self, actual: f64, expected: f64) -> bool {
        match self {
            Op::Lt => actual < expected,
            Op::Le => actual <= expected,
            Op::Gt => actual > expected,
            Op::Ge => actual >= expected,
            Op::Eq => actual == expected,
            Op::Ne => actual != expected,
        }
    }
}

/// One parsed threshold expression, e.g. `p95<500`.
#[derive(Debug, Clone, PartialEq)]
pub struct ThresholdExpr {
    pub agg: Agg,
    pub op: Op,
    pub value: f64,
    /// The expression as written (whitespace trimmed), for violation records.
    pub raw: String,
}

const AGG_NAMES: [&str; 9] = [
    "avg", "min", "max", "p50", "p90", "p95", "p99", "count", "rate",
];
const OP_NAMES: [&str; 6] = ["<=", ">=", "==", "!=", "<", ">"];

/// Parse one `<agg><op><number>` expression. Strict, with errors that name
/// the failing part; whitespace around the parts is tolerated.
pub fn parse_expr(input: &str) -> Result<ThresholdExpr, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty threshold expression — expected e.g. \"p95<500\"".into());
    }

    let agg_name = AGG_NAMES.iter().find(|a| s.starts_with(**a));
    let agg = match agg_name {
        Some(name) => match *name {
            "avg" => Agg::Avg,
            "min" => Agg::Min,
            "max" => Agg::Max,
            "p50" => Agg::P50,
            "p90" => Agg::P90,
            "p95" => Agg::P95,
            "p99" => Agg::P99,
            "count" => Agg::Count,
            _ => Agg::Rate,
        },
        None => {
            let token: String = s
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            return Err(format!(
                "unknown aggregate '{token}' in \"{s}\" — expected one of: {}",
                AGG_NAMES.join(", ")
            ));
        }
    };

    let rest = s[agg_name.unwrap().len()..].trim_start();
    let op_name = OP_NAMES.iter().find(|o| rest.starts_with(**o));
    let op = match op_name {
        Some(name) => match *name {
            "<=" => Op::Le,
            ">=" => Op::Ge,
            "==" => Op::Eq,
            "!=" => Op::Ne,
            "<" => Op::Lt,
            _ => Op::Gt,
        },
        None => {
            return Err(format!(
                "unknown or missing operator in \"{s}\" — expected one of: <, <=, >, >=, ==, !="
            ));
        }
    };

    let value_str = rest[op_name.unwrap().len()..].trim();
    if value_str.is_empty() {
        return Err(format!(
            "missing value in \"{s}\" — expected e.g. \"p95<500\""
        ));
    }
    let value: f64 = value_str.parse().map_err(|_| {
        format!("invalid value '{value_str}' in \"{s}\" — expected a plain number, no units")
    })?;
    if !value.is_finite() {
        return Err(format!(
            "invalid value '{value_str}' in \"{s}\" — expected a finite number"
        ));
    }

    Ok(ThresholdExpr {
        agg,
        op,
        value,
        raw: s.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Metric snapshot
// ---------------------------------------------------------------------------

/// What kind of metric a name resolves to — decides which aggregates apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// Duration/sample histogram (ms): avg/min/max/percentiles + count.
    Sample,
    /// Monotonic counter: only `count` (the final value).
    Counter,
    /// Per-invocation 0/1 failure samples: `rate` + `count`.
    Rate,
}

/// Precomputed aggregates for one metric, taken from the same HDR
/// histograms / counters the text summary prints.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricAgg {
    pub kind: MetricKind,
    pub avg: f64,
    pub min: f64,
    pub max: f64,
    pub p50: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
    pub count: f64,
    pub rate: f64,
}

impl MetricAgg {
    /// Snapshot a duration histogram (microseconds) into millisecond
    /// aggregates, using the same accessors as `Metrics::summary_lines`.
    pub fn sample(h: &hdrhistogram::Histogram<u64>) -> Self {
        let pct = |q: f64| h.value_at_quantile(q) as f64 / 1000.0;
        MetricAgg {
            kind: MetricKind::Sample,
            avg: h.mean() / 1000.0,
            min: h.min() as f64 / 1000.0,
            max: h.max() as f64 / 1000.0,
            p50: pct(0.50),
            p90: pct(0.90),
            p95: pct(0.95),
            p99: pct(0.99),
            count: h.len() as f64,
            rate: 0.0,
        }
    }

    pub fn counter(value: f64) -> Self {
        MetricAgg {
            kind: MetricKind::Counter,
            avg: 0.0,
            min: 0.0,
            max: 0.0,
            p50: 0.0,
            p90: 0.0,
            p95: 0.0,
            p99: 0.0,
            count: value,
            rate: 0.0,
        }
    }

    pub fn rate(total: u64, failed: u64) -> Self {
        MetricAgg {
            kind: MetricKind::Rate,
            avg: 0.0,
            min: 0.0,
            max: 0.0,
            p50: 0.0,
            p90: 0.0,
            p95: 0.0,
            p99: 0.0,
            count: total as f64,
            rate: if total == 0 {
                0.0
            } else {
                failed as f64 / total as f64
            },
        }
    }

    fn value_of(&self, agg: Agg) -> f64 {
        match agg {
            Agg::Avg => self.avg,
            Agg::Min => self.min,
            Agg::Max => self.max,
            Agg::P50 => self.p50,
            Agg::P90 => self.p90,
            Agg::P95 => self.p95,
            Agg::P99 => self.p99,
            Agg::Count => self.count,
            Agg::Rate => self.rate,
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// One violated expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThresholdViolation {
    pub metric: String,
    /// The expression as written, e.g. `"p95<500"`.
    pub expr: String,
    /// The aggregate's actual value (ms for sample metrics, fraction for
    /// `rate`, absolute for `count`).
    pub actual: f64,
}

/// The outcome of a `std/thresholds@v1` step — also the shape the run
/// summary JSON gains under `thresholds`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThresholdsSummary {
    /// `pass`, or the gate's `severity` when violated (`fail`/`warn`/`info`).
    pub status: String,
    /// Violation summary joined with `; ` plus the custom message, truncated
    /// to [`MAX_MESSAGE_CHARS`] with `…`.
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<ThresholdViolation>,
}

/// Severity of a gate: what `status` becomes when any expression is violated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Fail,
    Warn,
    Info,
}

impl Severity {
    fn as_str(&self) -> &'static str {
        match self {
            Severity::Fail => "fail",
            Severity::Warn => "warn",
            Severity::Info => "info",
        }
    }

    /// Rank for combining several gates: worst status wins.
    fn rank(&self) -> u8 {
        match self {
            Severity::Fail => 3,
            Severity::Warn => 2,
            Severity::Info => 1,
        }
    }
}

fn parse_severity(v: Option<&Value>) -> Result<Severity, String> {
    match v.and_then(|v| v.as_str()) {
        None => Ok(Severity::Fail),
        Some("fail") => Ok(Severity::Fail),
        Some("warn") => Ok(Severity::Warn),
        Some("info") => Ok(Severity::Info),
        Some(other) => Err(format!(
            "unknown severity '{other}' — expected fail, warn, or info"
        )),
    }
}

/// Evaluate parsed threshold params against a metric snapshot.
///
/// `params` is the step's interpolated `with` object (with step-level
/// `severity`/`message` merged in by the runner): each key is a metric name,
/// each value an expression string or an array of them.
///
/// Returns the step's [`ThresholdsSummary`]. Config errors (no metric
/// entries, unparseable expression, unknown metric, aggregate not applicable
/// to the metric kind) come back as `Err` with an actionable message.
pub fn evaluate(
    params: &Value,
    metrics: &std::collections::BTreeMap<String, MetricAgg>,
) -> Result<ThresholdsSummary, String> {
    let obj = match params.as_object() {
        Some(o) => o,
        None => return Err("`with` must be an object mapping metric names to expressions".into()),
    };

    let severity = parse_severity(obj.get("severity"))?;
    let custom_message = obj.get("message").and_then(|v| v.as_str());

    let entries: Vec<(&String, &Value)> = obj
        .iter()
        .filter(|(k, _)| k.as_str() != "severity" && k.as_str() != "message")
        .collect();
    if entries.is_empty() {
        return Err(
            "empty `with` — add at least one metric entry, e.g. `db_query_duration: [\"p95<500\"]`"
                .into(),
        );
    }

    let mut violations: Vec<(ThresholdViolation, String)> = Vec::new();

    for (metric, exprs) in entries {
        let expr_strs: Vec<String> = match exprs {
            Value::String(s) => vec![s.clone()],
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str() {
                        Some(s) => out.push(s.to_string()),
                        None => {
                            return Err(format!(
                                "expressions for '{metric}' must be strings, e.g. [\"p95<500\"]"
                            ));
                        }
                    }
                }
                out
            }
            _ => {
                return Err(format!(
                    "expressions for '{metric}' must be a string or an array of strings, e.g. [\"p95<500\"]"
                ));
            }
        };

        let agg = match metrics.get(metric) {
            Some(a) => a,
            None => {
                let mut present: Vec<&str> = metrics.keys().map(String::as_str).collect();
                present.sort_unstable();
                return Err(format!(
                    "unknown metric '{metric}' — metrics present in this run: {}",
                    present.join(", ")
                ));
            }
        };

        for raw in expr_strs {
            let expr = parse_expr(&raw).map_err(|e| format!("metric '{metric}': {e}"))?;
            // Aggregate applicability by metric kind.
            match (agg.kind, expr.agg) {
                (MetricKind::Sample, Agg::Rate) => {
                    return Err(format!(
                        "metric '{metric}' is a duration metric — `rate` only applies to failure metrics like '<family>_failed'"
                    ));
                }
                (MetricKind::Counter, agg_kind) if agg_kind != Agg::Count => {
                    return Err(format!(
                        "metric '{metric}' is a counter — only `count` applies to counters"
                    ));
                }
                (MetricKind::Rate, agg_kind) if agg_kind != Agg::Rate && agg_kind != Agg::Count => {
                    return Err(format!(
                        "metric '{metric}' is a failure-rate metric — only `rate` and `count` apply"
                    ));
                }
                _ => {}
            }

            let actual = agg.value_of(expr.agg);
            if !expr.op.holds(actual, expr.value) {
                let suffix = match (agg.kind, expr.agg) {
                    (MetricKind::Sample, Agg::Count) => "",
                    (MetricKind::Sample, _) => "ms",
                    _ => "",
                };
                let rendered = format!(
                    "{metric} {agg}={actual}{suffix} {neg} {expected}{suffix}",
                    agg = expr.agg.as_str(),
                    actual = fmt_num(actual),
                    neg = expr.op.negated(),
                    expected = fmt_num(expr.value),
                );
                violations.push((
                    ThresholdViolation {
                        metric: metric.clone(),
                        expr: expr.raw,
                        actual,
                    },
                    rendered,
                ));
            }
        }
    }

    let status = if violations.is_empty() {
        "pass".to_string()
    } else {
        severity.as_str().to_string()
    };

    let mut parts: Vec<String> = violations.iter().map(|(_, r)| r.clone()).collect();
    if let Some(msg) = custom_message {
        if !msg.is_empty() && !violations.is_empty() {
            parts.push(msg.to_string());
        }
    }
    let mut message = parts.join("; ");
    if message.is_empty() {
        message = match custom_message {
            Some(msg) if !msg.is_empty() => msg.to_string(),
            _ => "all thresholds met".to_string(),
        };
    }
    let message = truncate_message(&message);

    Ok(ThresholdsSummary {
        status,
        message,
        violations: violations.into_iter().map(|(v, _)| v).collect(),
    })
}

/// Combine the outcomes of several `std/thresholds@v1` steps into the single
/// `thresholds` field of the run summary: worst status wins
/// (fail > warn > info > pass), messages and violations concatenate.
pub fn combine(mut results: Vec<ThresholdsSummary>) -> Option<ThresholdsSummary> {
    if results.is_empty() {
        return None;
    }
    if results.len() == 1 {
        return Some(results.remove(0));
    }
    let rank = |status: &str| match status {
        "fail" => Severity::Fail.rank(),
        "warn" => Severity::Warn.rank(),
        "info" => Severity::Info.rank(),
        _ => 0,
    };
    let status = results
        .iter()
        .map(|r| r.status.clone())
        .max_by_key(|s| rank(s))
        .unwrap_or_else(|| "pass".to_string());
    let message = truncate_message(
        &results
            .iter()
            .map(|r| r.message.as_str())
            .filter(|m| !m.is_empty())
            .collect::<Vec<_>>()
            .join("; "),
    );
    let violations = results.into_iter().flat_map(|r| r.violations).collect();
    Some(ThresholdsSummary {
        status,
        message,
        violations,
    })
}

/// Compact number rendering for violation messages: integers stay integers
/// (`612`, not `612.00`), fractions get two decimals.
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

fn truncate_message(message: &str) -> String {
    if message.chars().count() <= MAX_MESSAGE_CHARS {
        return message.to_string();
    }
    let truncated: String = message.chars().take(MAX_MESSAGE_CHARS).collect();
    format!("{truncated}…")
}

// ---------------------------------------------------------------------------
// The action
// ---------------------------------------------------------------------------

/// `std/thresholds@v1` — evaluate run-level SLO gates against the metrics
/// collected so far (designed for `after:` blocks).
pub(crate) fn thresholds_action(params: &Value, ctx: &Context, step_name: &str) -> ActionOutput {
    let Some(metrics) = &ctx.run_metrics else {
        return err_out(
            step_name,
            "std/thresholds@v1 needs run metrics — use it in the `after:` block of a native run",
        );
    };

    let result = {
        let mut m = metrics.lock().unwrap();
        let snapshot = m.metric_snapshot();
        let result = evaluate(params, &snapshot);
        match &result {
            Ok(summary) => m.record_threshold_result(summary.clone()),
            // A config error is hard: the gate must never silently pass, so
            // it lands in the run summary (and the exit code) as a failure.
            Err(e) => m.record_threshold_result(ThresholdsSummary {
                status: "fail".to_string(),
                message: truncate_message(&format!("config error: {e}")),
                violations: Vec::new(),
            }),
        }
        result
    };

    match result {
        Ok(summary) => {
            let tag = if summary.status == "fail" {
                LogTag::Err
            } else {
                LogTag::Out
            };
            let log = if summary.status == "pass" {
                format!("{step_name}: thresholds PASS — {}", summary.message)
            } else {
                format!(
                    "{step_name}: thresholds {} — {}",
                    summary.status.to_uppercase(),
                    summary.message
                )
            };
            ActionOutput {
                value: serde_json::json!({
                    "status": summary.status,
                    "message": summary.message,
                    "violations": summary.violations,
                }),
                logs: vec![(tag, log)],
                // A violated gate is only a step failure at `severity: fail`;
                // warn/info are advisories and must not fail the teardown step.
                success: summary.status != "fail",
                http_sample: None,
            }
        }
        Err(e) => err_out(step_name, &e),
    }
}

fn err_out(step_name: &str, msg: &str) -> ActionOutput {
    ActionOutput {
        value: serde_json::json!({ "error": msg }),
        logs: vec![(LogTag::Err, format!("{step_name}: {msg}"))],
        success: false,
        http_sample: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    // -----------------------------------------------------------------
    // Expression parser
    // -----------------------------------------------------------------

    #[test]
    fn parses_every_aggregate() {
        for (s, agg) in [
            ("avg<1", Agg::Avg),
            ("min<1", Agg::Min),
            ("max<1", Agg::Max),
            ("p50<1", Agg::P50),
            ("p90<1", Agg::P90),
            ("p95<1", Agg::P95),
            ("p99<1", Agg::P99),
            ("count<1", Agg::Count),
            ("rate<1", Agg::Rate),
        ] {
            let e = parse_expr(s).unwrap();
            assert_eq!(e.agg, agg, "{s}");
            assert_eq!(e.raw, s);
        }
    }

    #[test]
    fn parses_every_operator() {
        for (s, op) in [
            ("avg<1", Op::Lt),
            ("avg<=1", Op::Le),
            ("avg>1", Op::Gt),
            ("avg>=1", Op::Ge),
            ("avg==1", Op::Eq),
            ("avg!=1", Op::Ne),
        ] {
            assert_eq!(parse_expr(s).unwrap().op, op, "{s}");
        }
        // Two-char ops must not be misread as one-char + garbage value.
        let e = parse_expr("p95<=500").unwrap();
        assert_eq!(e.op, Op::Le);
        assert_eq!(e.value, 500.0);
    }

    #[test]
    fn parses_int_decimal_negative_zero_and_large_values() {
        assert_eq!(parse_expr("count==0").unwrap().value, 0.0);
        assert_eq!(parse_expr("rate<0.05").unwrap().value, 0.05);
        assert_eq!(parse_expr("avg>-1.5").unwrap().value, -1.5);
        assert_eq!(parse_expr("max<1000000").unwrap().value, 1_000_000.0);
        assert_eq!(parse_expr("p99<1e6").unwrap().value, 1e6);
    }

    #[test]
    fn tolerates_whitespace_around_parts() {
        let e = parse_expr("  p95  <  500  ").unwrap();
        assert_eq!(e.agg, Agg::P95);
        assert_eq!(e.op, Op::Lt);
        assert_eq!(e.value, 500.0);
        assert_eq!(e.raw, "p95  <  500");
    }

    #[test]
    fn rejects_garbage_with_clear_errors() {
        let e = parse_expr("").unwrap_err();
        assert!(e.contains("empty"), "{e}");

        let e = parse_expr("p95.5<500").unwrap_err();
        assert!(e.contains("operator"), "{e}");

        let e = parse_expr("foo<1").unwrap_err();
        assert!(e.contains("unknown aggregate 'foo'"), "{e}");
        assert!(e.contains("avg"), "{e} lists the valid aggs");

        let e = parse_expr("p95!500").unwrap_err();
        assert!(e.contains("operator"), "{e}");

        let e = parse_expr("p95<").unwrap_err();
        assert!(e.contains("missing value"), "{e}");

        let e = parse_expr("p95<500ms").unwrap_err();
        assert!(e.contains("invalid value '500ms'"), "{e}");
        assert!(e.contains("no units"), "{e}");

        let e = parse_expr("p95<NaN").unwrap_err();
        assert!(e.contains("finite"), "{e}");
    }

    // -----------------------------------------------------------------
    // Aggregates (consistent with the text summary)
    // -----------------------------------------------------------------

    fn sample_hist(values_ms: &[f64]) -> hdrhistogram::Histogram<u64> {
        let mut h = hdrhistogram::Histogram::new_with_bounds(1, 3_600_000_000, 2).unwrap();
        for v in values_ms {
            h.record(((v * 1000.0).round() as u64).clamp(1, 3_600_000_000))
                .unwrap();
        }
        h
    }

    #[test]
    fn sample_aggregates_match_the_summary_histogram_accessors() {
        let values: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        let h = sample_hist(&values);
        let agg = MetricAgg::sample(&h);

        // Same accessors as Metrics::summary_lines — identical numbers.
        assert_eq!(agg.avg, h.mean() / 1000.0);
        assert_eq!(agg.p95, h.value_at_quantile(0.95) as f64 / 1000.0);
        assert_eq!(agg.p99, h.value_at_quantile(0.99) as f64 / 1000.0);
        assert_eq!(agg.min, h.min() as f64 / 1000.0);
        assert_eq!(agg.max, h.max() as f64 / 1000.0);
        assert_eq!(agg.count, 100.0);

        // HDR promises ≤1% quantile error — the aggregates land on the
        // expected values for a known sample set.
        let within = |actual: f64, expected: f64| (actual - expected).abs() <= expected * 0.011;
        assert!(within(agg.p50, 50.0), "p50={}", agg.p50);
        assert!(within(agg.p90, 90.0), "p90={}", agg.p90);
        assert!(within(agg.p95, 95.0), "p95={}", agg.p95);
        assert!(within(agg.avg, 50.5), "avg={}", agg.avg);
    }

    #[test]
    fn rate_is_failed_over_total_invocations() {
        // 1 failure out of 4 invocations.
        let agg = MetricAgg::rate(4, 1);
        assert_eq!(agg.rate, 0.25);
        assert_eq!(agg.count, 4.0);
        assert_eq!(MetricAgg::rate(0, 0).rate, 0.0, "no invocations → 0");
    }

    // -----------------------------------------------------------------
    // Evaluation
    // -----------------------------------------------------------------

    fn snapshot() -> BTreeMap<String, MetricAgg> {
        let mut m = BTreeMap::new();
        m.insert(
            "db_query_duration".to_string(),
            MetricAgg::sample(&sample_hist(&[10.0, 20.0, 30.0, 612.0])),
        );
        m.insert("db_errors".to_string(), MetricAgg::counter(3.0));
        m.insert("db_query_failed".to_string(), MetricAgg::rate(4, 1));
        m
    }

    #[test]
    fn passing_expressions_yield_pass_status() {
        let s = evaluate(
            &json!({
                "db_query_duration": ["p95<5000", "max<2000", "avg>0", "count==4"],
                "db_errors": ["count==3"],
                "db_query_failed": ["rate<0.5", "rate<=0.25", "rate>=0.25"],
            }),
            &snapshot(),
        )
        .unwrap();
        assert_eq!(s.status, "pass");
        assert!(s.violations.is_empty());
        assert_eq!(s.message, "all thresholds met");
    }

    #[test]
    fn violated_expressions_yield_severity_status_and_violations() {
        let s = evaluate(
            &json!({
                "db_query_duration": ["p95<500", "max<2000"],
                "db_errors": ["count==0"],
            }),
            &snapshot(),
        )
        .unwrap();
        assert_eq!(s.status, "fail", "default severity is fail");
        assert_eq!(s.violations.len(), 2);
        // Metric keys iterate in sorted (BTreeMap) order: db_errors first.
        assert_eq!(s.violations[0].metric, "db_errors");
        assert_eq!(s.violations[0].expr, "count==0");
        assert_eq!(s.violations[0].actual, 3.0);
        assert_eq!(s.violations[1].metric, "db_query_duration");
        assert_eq!(s.violations[1].expr, "p95<500");
        // The actual is the HDR histogram's p95 (≤1% quantization, exactly
        // what the text summary prints — 612ms lands on a bucket boundary).
        let h = sample_hist(&[10.0, 20.0, 30.0, 612.0]);
        let expected_p95 = h.value_at_quantile(0.95) as f64 / 1000.0;
        assert_eq!(s.violations[1].actual, expected_p95);
        assert!(expected_p95 >= 612.0);
        // k6-style rendered summary with the negated operator and ms units.
        assert!(
            s.message.contains(&format!(
                "db_query_duration p95={}ms ≥ 500ms",
                super::tests::fmt_num(expected_p95)
            )),
            "{}",
            s.message
        );
        assert!(s.message.contains("db_errors count=3 ≠ 0"), "{}", s.message);
    }

    #[test]
    fn severity_maps_to_status_warn_and_info() {
        for (severity, expected) in [("warn", "warn"), ("info", "info"), ("fail", "fail")] {
            let s = evaluate(
                &json!({ "db_errors": ["count==0"], "severity": severity }),
                &snapshot(),
            )
            .unwrap();
            assert_eq!(s.status, expected);
        }
        let e = evaluate(
            &json!({ "db_errors": ["count==0"], "severity": "page-me" }),
            &snapshot(),
        )
        .unwrap_err();
        assert!(e.contains("unknown severity 'page-me'"), "{e}");
    }

    #[test]
    fn custom_message_is_appended_to_violation_summary() {
        let s = evaluate(
            &json!({ "db_errors": ["count==0"], "message": "checkout SLO" }),
            &snapshot(),
        )
        .unwrap();
        assert_eq!(s.message, "db_errors count=3 ≠ 0; checkout SLO");

        // On pass, the custom message still surfaces.
        let s = evaluate(
            &json!({ "db_errors": ["count==3"], "message": "checkout SLO" }),
            &snapshot(),
        )
        .unwrap();
        assert_eq!(s.status, "pass");
        assert_eq!(s.message, "checkout SLO");
    }

    #[test]
    fn message_truncates_at_200_chars_with_ellipsis() {
        let mut with = serde_json::Map::new();
        // 30 violating expressions → a message far beyond 200 chars.
        let exprs: Vec<String> = (0..30).map(|_| "max<1".to_string()).collect();
        with.insert("db_query_duration".to_string(), json!(exprs));
        let s = evaluate(&Value::Object(with), &snapshot()).unwrap();
        assert!(s.message.ends_with('…'), "{}", s.message);
        assert_eq!(s.message.chars().count(), MAX_MESSAGE_CHARS + 1);

        // Exactly-200 messages are not truncated.
        let short = "x".repeat(MAX_MESSAGE_CHARS);
        assert_eq!(truncate_message(&short), short);
    }

    #[test]
    fn unknown_metric_errors_and_lists_present_metrics() {
        let e = evaluate(&json!({ "typo_metric": ["p95<1"] }), &snapshot()).unwrap_err();
        assert!(e.contains("unknown metric 'typo_metric'"), "{e}");
        assert!(e.contains("db_query_duration"), "{e}");
        assert!(e.contains("db_errors"), "{e}");
        assert!(e.contains("db_query_failed"), "{e}");
    }

    #[test]
    fn empty_with_is_a_config_error() {
        let e = evaluate(&json!({}), &snapshot()).unwrap_err();
        assert!(e.contains("empty `with`"), "{e}");
        // severity/message alone don't count as metric entries either.
        let e = evaluate(&json!({ "severity": "warn" }), &snapshot()).unwrap_err();
        assert!(e.contains("empty `with`"), "{e}");
    }

    #[test]
    fn aggregate_applicability_is_enforced_per_metric_kind() {
        let e = evaluate(&json!({ "db_query_duration": ["rate<0.1"] }), &snapshot()).unwrap_err();
        assert!(e.contains("duration metric"), "{e}");

        let e = evaluate(&json!({ "db_errors": ["avg<1"] }), &snapshot()).unwrap_err();
        assert!(e.contains("counter"), "{e}");

        let e = evaluate(&json!({ "db_query_failed": ["p95<1"] }), &snapshot()).unwrap_err();
        assert!(e.contains("failure-rate"), "{e}");
    }

    #[test]
    fn single_string_expression_is_accepted() {
        let s = evaluate(&json!({ "db_errors": "count==3" }), &snapshot()).unwrap();
        assert_eq!(s.status, "pass");
    }

    #[test]
    fn rate_over_mixed_pass_fail_invocations() {
        // 3 successes, 1 failure → rate 0.25: `<0.05` fails, `<0.5` passes.
        let mut m = BTreeMap::new();
        m.insert("db_query_failed".to_string(), MetricAgg::rate(4, 1));
        let s = evaluate(&json!({ "db_query_failed": ["rate<0.05"] }), &m).unwrap();
        assert_eq!(s.status, "fail");
        assert_eq!(s.violations[0].actual, 0.25);

        let s = evaluate(&json!({ "db_query_failed": ["rate<0.5"] }), &m).unwrap();
        assert_eq!(s.status, "pass");
    }

    #[test]
    fn combine_picks_the_worst_status() {
        let mk = |status: &str, msg: &str| ThresholdsSummary {
            status: status.to_string(),
            message: msg.to_string(),
            violations: vec![],
        };
        assert!(combine(vec![]).is_none());
        assert_eq!(combine(vec![mk("pass", "a")]).unwrap().status, "pass");
        let c = combine(vec![mk("pass", "a"), mk("warn", "b"), mk("fail", "c")]).unwrap();
        assert_eq!(c.status, "fail");
        assert_eq!(c.message, "a; b; c");
        let c = combine(vec![mk("info", "a"), mk("pass", "b")]).unwrap();
        assert_eq!(c.status, "info");
    }

    #[test]
    fn thresholds_summary_serde_round_trip() {
        let s = evaluate(
            &json!({ "db_errors": ["count==0"], "message": "checkout SLO" }),
            &snapshot(),
        )
        .unwrap();
        let json = serde_json::to_string(&s).unwrap();
        let back: ThresholdsSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    // -----------------------------------------------------------------
    // The action (metrics handle through the context)
    // -----------------------------------------------------------------

    use std::sync::{Arc, Mutex};

    fn ctx_with_metrics(
        m: crate::step::runner::Metrics,
    ) -> (Context, Arc<Mutex<crate::step::runner::Metrics>>) {
        let shared = Arc::new(Mutex::new(m));
        let mut ctx = Context::new();
        ctx.run_metrics = Some(Arc::clone(&shared));
        (ctx, shared)
    }

    fn metrics_with_db_samples(samples: &[(f64, bool)]) -> crate::step::runner::Metrics {
        let mut m = crate::step::runner::Metrics::default();
        for (ms, failed) in samples {
            m.add_counters(json!({ "db_query_duration": [ms] }).as_object().unwrap());
            m.record_rate("db_query_failed", *failed);
        }
        m
    }

    #[test]
    fn action_without_run_metrics_is_a_config_error() {
        let ctx = Context::new();
        let out = thresholds_action(&json!({ "db_errors": ["count==0"] }), &ctx, "gate");
        assert!(!out.success);
        assert!(out.logs[0].1.contains("run metrics"), "{:?}", out.logs);
    }

    #[test]
    fn action_records_pass_and_violation_outcomes() {
        let (ctx, shared) = ctx_with_metrics(metrics_with_db_samples(&[
            (10.0, false),
            (20.0, false),
            (30.0, true),
        ]));

        let out = thresholds_action(
            &json!({ "db_query_failed": ["rate<0.5"], "db_query_duration": ["p95<1000"] }),
            &ctx,
            "gate",
        );
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["status"], "pass");

        let out = thresholds_action(
            &json!({ "db_query_failed": ["rate<0.05"], "severity": "warn" }),
            &ctx,
            "gate",
        );
        assert_eq!(out.value["status"], "warn");
        assert!(out.success, "warn is advisory, not a step failure");
        assert_eq!(out.value["violations"][0]["metric"], "db_query_failed");

        // A fail gate is a step failure.
        let out = thresholds_action(&json!({ "db_query_failed": ["rate<0.05"] }), &ctx, "gate");
        assert_eq!(out.value["status"], "fail");
        assert!(!out.success);

        let combined = shared.lock().unwrap().thresholds_summary().unwrap();
        assert_eq!(combined.status, "fail", "worst of pass/warn/fail wins");
        assert_eq!(
            combined.violations.len(),
            2,
            "warn + fail violations concat"
        );
    }

    #[test]
    fn action_config_error_is_recorded_as_a_run_failure() {
        let (ctx, shared) = ctx_with_metrics(metrics_with_db_samples(&[(10.0, false)]));
        let out = thresholds_action(&json!({ "nope": ["p95<1"] }), &ctx, "gate");
        assert!(!out.success);
        assert!(
            out.logs[0].1.contains("unknown metric 'nope'"),
            "{:?}",
            out.logs
        );
        assert!(
            out.logs[0].1.contains("db_query_duration"),
            "{:?}",
            out.logs
        );

        let summary = shared.lock().unwrap().thresholds_summary().unwrap();
        assert_eq!(
            summary.status, "fail",
            "a broken gate must not silently pass"
        );
        assert!(
            summary.message.contains("config error"),
            "{}",
            summary.message
        );
    }
}
