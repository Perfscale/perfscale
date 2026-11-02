//! Reporter configuration (`report.reporters`, issues #9 and #12).
//!
//! Reporters use the same `use:`/`with:` shape as steps: `use` names a
//! built-in — the final-only renderers (`std/report/json@v1`,
//! `std/report/junit@v1`, `std/report/html@v1`) or the streaming exporters
//! (`std/report/serve@v1`, `std/report/prometheus-rw@v1`,
//! `std/report/otel@v1`) — or a WASM reporter component
//! (`wasm:./reporter.wasm`, see `crate::report::wasm` and
//! `wit/reporter.wit`).
//!
//! Engine-owned keys sit next to `use:` (issue #12): `during_run`,
//! `interval_ms`, `labels` and `max_series`, plus `max_samples` (moved out of
//! `with:`; the legacy `with.max_samples` is still read for compatibility).
//! `with:` stays reporter-owned: destination (`path`/`url`/`endpoint`),
//! delivery knobs (`retry`, `max_pending`, `final_timeout`) and the
//! Prometheus `histogram` mode.
//!
//! Every reporter declares its capabilities: `stream` (during-run snapshot
//! export) and/or `final` (end-of-run document). `during_run: true` on a
//! final-only reporter is a configuration error, not a silent no-op.
//!
//! Configuration errors (unknown reporter, missing destination, a `${VAR}`
//! reference to an unset variable, an unloadable WASM module, a reserved
//! `run_id`/`worker` label override) are fail-fast: lint flags them and the
//! run refuses to start ([`validate_reporters`]). Runtime delivery errors are
//! best-effort — logged and counted, never run-killing.

use std::collections::BTreeMap;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::metrics::MAX_STORED_STEP_SAMPLES;

/// Default `max_series` cardinality guard per reporter (issue #12).
pub const DEFAULT_MAX_SERIES: usize = 10_000;

/// Label names the engine always sets itself; user configuration may never
/// override them (rejected at lint/run time).
pub const RESERVED_LABELS: [&str; 2] = ["run_id", "worker"];

/// One configured reporter (`report.reporters[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReporterConfig {
    /// Reporter id: `std/report/json@v1`, `std/report/junit@v1`,
    /// `std/report/html@v1`, `std/report/serve@v1`,
    /// `std/report/prometheus-rw@v1`, `std/report/otel@v1`, or
    /// `wasm:./path/to/reporter.wasm`.
    #[serde(rename = "use")]
    pub use_: String,

    /// Stream metric snapshots during the run (streaming reporters only).
    /// Setting this on a final-only reporter is a configuration error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub during_run: bool,

    /// Snapshot/flush interval for this reporter in milliseconds (default
    /// the run-level `report.interval_ms`, clamped to ≥
    /// [`crate::report::MIN_INTERVAL_MS`] at use).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,

    /// Reporter-level labels: stamped on every series this reporter exports.
    /// Merge priority is step > reporter > run. `run_id` and `worker` are
    /// reserved — the engine always sets them. Values support
    /// `${{ inputs.* }}` / `${{ env.* }}` interpolation, resolved once before
    /// the run starts.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub labels: Map<String, Value>,

    /// Cardinality guard: maximum distinct output series this reporter
    /// exports (default [`DEFAULT_MAX_SERIES`]). Series beyond the limit are
    /// dropped deterministically (sorted order) and counted in
    /// `perfscale_report_dropped_series_total{reporter}`; the run continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_series: Option<usize>,

    /// Raw per-step samples to include (final reporters; default
    /// [`MAX_STORED_STEP_SAMPLES`], `0` disables the samples block). Moved
    /// out of `with:` in issue #12; the legacy `with.max_samples` key is
    /// still honored when this is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_samples: Option<usize>,

    /// Reporter parameters (destination + knobs).
    #[serde(default)]
    pub with: ReporterWith,
}

impl ReporterConfig {
    /// The raw-sample cap for this reporter: engine-level `max_samples`
    /// wins, then the legacy `with.max_samples`, then the default.
    pub fn max_samples(&self) -> usize {
        self.max_samples
            .or(self.with.max_samples)
            .unwrap_or(MAX_STORED_STEP_SAMPLES)
    }

    /// The effective cardinality guard.
    pub fn max_series(&self) -> usize {
        self.max_series.unwrap_or(DEFAULT_MAX_SERIES)
    }

    /// This reporter's snapshot interval, falling back to the run-level
    /// default, clamped to [`crate::report::MIN_INTERVAL_MS`].
    pub fn interval(&self, run_interval: Duration) -> Duration {
        self.interval_ms
            .map(|ms| Duration::from_millis(ms.max(crate::report::MIN_INTERVAL_MS)))
            .unwrap_or(run_interval)
    }
}

/// Reporter parameters (`with:`). Destination plus reporter-owned knobs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReporterWith {
    /// Write the report to this local file (final reporters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Target URL: HTTP push for final reporters, the base URL of a
    /// `perfscale serve` for `std/report/serve@v1`, the remote-write endpoint
    /// for `std/report/prometheus-rw@v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// OTLP/gRPC collector endpoint for `std/report/otel@v1`, e.g.
    /// `http://otel-collector:4317`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Extra headers for HTTP delivery. Values may embed `${VAR}`
    /// environment references, e.g. `Authorization: "Bearer ${TOKEN}"` —
    /// interpolated host-side at delivery, never logged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    /// Prometheus histogram mode (`std/report/prometheus-rw@v1`):
    /// `classic` (default, `_bucket{le}` + `_sum` + `_count`) or `native`
    /// (Prometheus native histograms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub histogram: Option<String>,

    /// Retry policy for transient delivery failures (network errors, 429,
    /// 5xx, transient gRPC statuses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryConfig>,

    /// Batch size for streaming delivery: seal and send a batch once it
    /// holds this many items (default the run-level `report.batch_size`).
    /// Streaming reporters only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<usize>,

    /// Bounded queue depth: snapshots/batches awaiting delivery. Overflow
    /// drops the OLDEST entry and increments
    /// `perfscale_report_dropped_snapshots_total{reporter}` (default 24).
    /// Streaming reporters only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pending: Option<usize>,

    /// Bound on the synchronous final flush at run end, e.g. `"30s"`
    /// (default `"30s"`). A timed-out flush warns but never fails the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_timeout: Option<String>,

    /// Legacy spelling of the engine-level `max_samples` key (kept for
    /// backward compatibility; prefer `max_samples` next to `use:`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_samples: Option<usize>,

    /// WASM reporters only: output directory for the guest's `emit-file`
    /// writes. Default `./perfscale-report-<module-stem>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,

    /// WASM reporters only: free-form config handed to the guest's
    /// `on-report` verbatim (the `config-json` argument).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// Retry policy for transient delivery failures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    /// Maximum delivery attempts per batch before it is dropped and counted
    /// (default 5; `1` disables retries).
    #[serde(default = "default_retry_max_attempts")]
    pub max_attempts: u32,

    /// Backoff range, `"<min>..<max>"` (default `"1s..30s"`). The first retry
    /// waits `min`, doubling per attempt up to `max`; a server's
    /// `Retry-After` hint wins when present.
    #[serde(default = "default_retry_backoff")]
    pub backoff: String,
}

fn default_retry_max_attempts() -> u32 {
    5
}

fn default_retry_backoff() -> String {
    "1s..30s".to_string()
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_retry_max_attempts(),
            backoff: default_retry_backoff(),
        }
    }
}

impl RetryConfig {
    /// Parse the `backoff` range into `(min, max)`.
    pub fn backoff_range(&self) -> Result<(Duration, Duration), String> {
        parse_backoff_range(&self.backoff)
    }
}

/// Parse a `"<min>..<max>"` backoff range (durations like `500ms`, `1s`).
pub fn parse_backoff_range(s: &str) -> Result<(Duration, Duration), String> {
    let (min, max) = s
        .split_once("..")
        .ok_or_else(|| format!("invalid backoff range '{s}' — expected \"1s..30s\""))?;
    let min = crate::step::parse_duration_ms_strict(min.trim())
        .map_err(|e| format!("invalid backoff range '{s}': {e}"))?;
    let max = crate::step::parse_duration_ms_strict(max.trim())
        .map_err(|e| format!("invalid backoff range '{s}': {e}"))?;
    if max < min {
        return Err(format!("invalid backoff range '{s}': max is below min"));
    }
    Ok((Duration::from_millis(min), Duration::from_millis(max)))
}

/// What a reporter can do: `stream` (during-run snapshot export) and/or
/// `final` (end-of-run report document).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReporterCapabilities {
    pub stream: bool,
    pub final_: bool,
}

/// The built-in reporters shipped in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReporterKind {
    Json,
    Junit,
    Html,
    /// The `perfscale serve` sink (`POST <url>/api/v1/metrics`) — the
    /// historical `report.url` + `during_run` streaming path as a reporter.
    Serve,
    /// Prometheus remote write (protobuf + Snappy over HTTP).
    PrometheusRw,
    /// OTLP/gRPC metrics exporter.
    Otel,
}

impl ReporterKind {
    /// Stream/final capabilities of this kind. Final-only reporters reject
    /// `during_run: true` at lint/run time; streaming reporters never
    /// receive the end-of-run document.
    pub fn capabilities(&self) -> ReporterCapabilities {
        match self {
            ReporterKind::Json | ReporterKind::Junit | ReporterKind::Html => ReporterCapabilities {
                stream: false,
                final_: true,
            },
            ReporterKind::Serve | ReporterKind::PrometheusRw | ReporterKind::Otel => {
                ReporterCapabilities {
                    stream: true,
                    final_: false,
                }
            }
        }
    }
}

/// Map a reporter id to a built-in. Accepts the full id
/// (`std/report/json@v1`) and the CLI shorthand (`json`).
pub fn reporter_kind(use_: &str) -> Option<ReporterKind> {
    match use_ {
        "std/report/json@v1" | "json" => Some(ReporterKind::Json),
        "std/report/junit@v1" | "junit" => Some(ReporterKind::Junit),
        "std/report/html@v1" | "html" => Some(ReporterKind::Html),
        "std/report/serve@v1" | "serve" => Some(ReporterKind::Serve),
        "std/report/prometheus-rw@v1" | "prometheus-rw" => Some(ReporterKind::PrometheusRw),
        "std/report/otel@v1" | "otel" => Some(ReporterKind::Otel),
        _ => None,
    }
}

/// The module path of a WASM reporter id (`wasm:./path/to/reporter.wasm`),
/// or `None` when `use_` does not name a WASM reporter.
pub fn wasm_reporter_module(use_: &str) -> Option<&str> {
    use_.strip_prefix("wasm:").filter(|p| !p.is_empty())
}

/// The known built-in reporter ids, for error messages and docs.
pub const BUILTIN_REPORTER_IDS: &str = "std/report/json@v1, std/report/junit@v1, std/report/html@v1, std/report/serve@v1, std/report/prometheus-rw@v1, std/report/otel@v1";

/// All `${VAR}` references in `s`, in order of appearance.
fn env_refs(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        rest = &rest[start + 2..];
        match rest.find('}') {
            Some(end)
                if end > 0
                    && rest[..end]
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                out.push(&rest[..end]);
                rest = &rest[end + 1..];
            }
            _ => break,
        }
    }
    out
}

/// Interpolate `${VAR}` references in a header value against the process
/// environment. The interpolated value is a secret: callers must never log
/// it.
pub fn resolve_env_refs(s: &str) -> Result<String, String> {
    let mut out = s.to_string();
    for var in env_refs(s) {
        let value = std::env::var(var)
            .map_err(|_| format!("environment variable ${{{var}}} is not set"))?;
        out = out.replace(&format!("${{{var}}}"), &value);
    }
    Ok(out)
}

/// Check a labels map (run-, reporter- or step-level): values must be
/// scalars and the reserved engine labels (`run_id`, `worker`) must not be
/// overridden. `where_` names the location for the error message.
pub fn validate_labels(labels: &Map<String, Value>, where_: &str) -> Result<(), String> {
    for (k, v) in labels {
        if RESERVED_LABELS.contains(&k.as_str()) {
            return Err(format!(
                "{where_}: label '{k}' is reserved — the engine always sets `run_id` and `worker`"
            ));
        }
        if !v.is_string() && !v.is_number() && !v.is_boolean() {
            return Err(format!(
                "{where_}: label '{k}' must be a scalar (string, number or boolean)"
            ));
        }
    }
    Ok(())
}

/// Fail-fast validation of a reporters list — run at lint time and before
/// the run starts. Errors are plain messages; the callers wrap them with
/// location hints.
pub fn validate_reporters(reporters: &[ReporterConfig]) -> Result<(), String> {
    for r in reporters {
        validate_labels(&r.labels, &format!("reporter '{}'", r.use_))?;
        if let Some(module) = wasm_reporter_module(&r.use_) {
            validate_wasm_reporter(r, module)?;
        } else {
            let kind = reporter_kind(&r.use_).ok_or_else(|| {
                format!(
                    "unknown reporter '{}' — built-ins: {BUILTIN_REPORTER_IDS}; or a WASM reporter: wasm:./path/to/reporter.wasm",
                    r.use_
                )
            })?;
            validate_builtin_reporter(r, kind)?;
        }
        validate_delivery_knobs(r)?;
        for (k, v) in &r.with.headers {
            for var in env_refs(v) {
                if std::env::var_os(var).is_none() {
                    return Err(format!(
                        "reporter '{r}' header '{k}' references ${{{var}}}, which is not set",
                        r = r.use_
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Capability and destination checks for one built-in reporter.
fn validate_builtin_reporter(r: &ReporterConfig, kind: ReporterKind) -> Result<(), String> {
    let caps = kind.capabilities();
    if r.during_run && !caps.stream {
        return Err(format!(
            "reporter '{}' is final-only and does not support `during_run` — streaming reporters: std/report/serve@v1, std/report/prometheus-rw@v1, std/report/otel@v1",
            r.use_
        ));
    }
    match kind {
        ReporterKind::Json | ReporterKind::Junit | ReporterKind::Html => {
            for (key, set) in [
                ("retry", r.with.retry.is_some()),
                ("max_pending", r.with.max_pending.is_some()),
                ("batch_size", r.with.batch_size.is_some()),
                ("final_timeout", r.with.final_timeout.is_some()),
                ("histogram", r.with.histogram.is_some()),
                ("endpoint", r.with.endpoint.is_some()),
            ] {
                if set {
                    return Err(format!(
                        "reporter '{}': `with.{key}` only applies to streaming reporters (std/report/serve@v1, std/report/prometheus-rw@v1, std/report/otel@v1)",
                        r.use_
                    ));
                }
            }
            if r.interval_ms.is_some() {
                return Err(format!(
                    "reporter '{}': `interval_ms` only applies to streaming reporters",
                    r.use_
                ));
            }
            match (&r.with.path, &r.with.url) {
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "reporter '{}': `with.path` and `with.url` are mutually exclusive",
                        r.use_
                    ))
                }
                (None, None) => {
                    return Err(format!(
                    "reporter '{}' needs a destination: `with.path` (file) or `with.url` (HTTP push)",
                    r.use_
                ))
                }
                _ => {}
            }
        }
        ReporterKind::Serve | ReporterKind::PrometheusRw => {
            if r.with.url.as_deref().is_none_or(|u| u.is_empty()) {
                return Err(format!(
                    "reporter '{}' needs `with.url` ({}), and does not write files (`with.path`)",
                    r.use_,
                    match kind {
                        ReporterKind::Serve => "the perfscale serve base URL",
                        _ => "the Prometheus remote-write endpoint",
                    }
                ));
            }
            if r.with.path.is_some() {
                return Err(format!(
                    "reporter '{}': `with.path` does not apply — it streams to `with.url`",
                    r.use_
                ));
            }
            if kind == ReporterKind::PrometheusRw {
                match r.with.histogram.as_deref() {
                    None | Some("classic") | Some("native") => {}
                    Some(other) => {
                        return Err(format!(
                            "reporter '{}': unknown `with.histogram` mode '{other}' — expected `classic` or `native`",
                            r.use_
                        ))
                    }
                }
            } else if r.with.histogram.is_some() {
                return Err(format!(
                    "reporter '{}': `with.histogram` only applies to std/report/prometheus-rw@v1",
                    r.use_
                ));
            }
        }
        ReporterKind::Otel => {
            if r.with.endpoint.as_deref().is_none_or(|u| u.is_empty()) {
                return Err(format!(
                    "reporter '{}' needs `with.endpoint` (the OTLP/gRPC collector, e.g. http://localhost:4317)",
                    r.use_
                ));
            }
            if r.with.path.is_some() || r.with.url.is_some() {
                return Err(format!(
                    "reporter '{}': destination is `with.endpoint`; `with.path`/`with.url` do not apply",
                    r.use_
                ));
            }
        }
    }
    if kind != ReporterKind::Otel && r.with.endpoint.is_some() {
        return Err(format!(
            "reporter '{}': `with.endpoint` only applies to std/report/otel@v1",
            r.use_
        ));
    }
    Ok(())
}

/// Delivery knob validation shared by every reporter kind: the retry
/// backoff range must parse, `final_timeout` must be a duration, and
/// `max_pending`/`batch` sizes must be non-zero when set.
fn validate_delivery_knobs(r: &ReporterConfig) -> Result<(), String> {
    if let Some(retry) = &r.with.retry {
        retry
            .backoff_range()
            .map_err(|e| format!("reporter '{}': {e}", r.use_))?;
        if retry.max_attempts == 0 {
            return Err(format!(
                "reporter '{}': `with.retry.max_attempts` must be ≥ 1",
                r.use_
            ));
        }
    }
    if let Some(ft) = &r.with.final_timeout {
        crate::step::parse_duration_ms_strict(ft)
            .map_err(|e| format!("reporter '{}': invalid `with.final_timeout`: {e}", r.use_))?;
    }
    if r.with.max_pending == Some(0) {
        return Err(format!(
            "reporter '{}': `with.max_pending` must be ≥ 1",
            r.use_
        ));
    }
    if r.with.batch_size == Some(0) {
        return Err(format!(
            "reporter '{}': `with.batch_size` must be ≥ 1",
            r.use_
        ));
    }
    if r.max_series == Some(0) {
        return Err(format!("reporter '{}': `max_series` must be ≥ 1", r.use_));
    }
    Ok(())
}

/// Fail-fast checks for a `wasm:` reporter: `path`/`url` do not apply (the
/// guest writes via `emit-file`), `during_run` does not apply (WASM
/// reporters are final-only), and the module must exist, compile and export
/// the `perfscale:reporter` world before the run starts.
fn validate_wasm_reporter(r: &ReporterConfig, module: &str) -> Result<(), String> {
    if r.with.path.is_some() || r.with.url.is_some() {
        return Err(format!(
            "reporter '{}': `with.path`/`with.url` do not apply to WASM reporters — the guest writes through emit-file into `with.output`",
            r.use_
        ));
    }
    if r.during_run {
        return Err(format!(
            "reporter '{}': WASM reporters are final-only — `during_run` does not apply",
            r.use_
        ));
    }
    wasm_module_check(&r.use_, module)
}

/// The module exists, compiles and exports the `perfscale:reporter` world.
/// Without the `wasm-libs` feature a `wasm:` reporter is unsupported.
#[cfg(feature = "wasm-libs")]
fn wasm_module_check(use_: &str, module: &str) -> Result<(), String> {
    crate::report::wasm::WasmReporter::check(std::path::Path::new(module))
        .map_err(|e| format!("reporter '{use_}': {e}"))
}

/// The module exists, compiles and exports the `perfscale:reporter` world.
/// Without the `wasm-libs` feature a `wasm:` reporter is unsupported.
#[cfg(not(feature = "wasm-libs"))]
fn wasm_module_check(use_: &str, _module: &str) -> Result<(), String> {
    Err(format!(
        "reporter '{use_}': WASM reporters need the `wasm-libs` feature, which this build lacks"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin(use_: &str) -> ReporterConfig {
        ReporterConfig {
            use_: use_.into(),
            during_run: false,
            interval_ms: None,
            labels: Map::new(),
            max_series: None,
            max_samples: None,
            with: ReporterWith::default(),
        }
    }

    #[test]
    fn validates_known_reporters_and_destinations() {
        let ok = vec![
            ReporterConfig {
                with: ReporterWith {
                    path: Some("out.json".into()),
                    ..ReporterWith::default()
                },
                ..builtin("std/report/json@v1")
            },
            ReporterConfig {
                with: ReporterWith {
                    url: Some("http://localhost:9000/reports".into()),
                    ..ReporterWith::default()
                },
                ..builtin("junit")
            },
            ReporterConfig {
                with: ReporterWith {
                    path: Some("out.html".into()),
                    ..ReporterWith::default()
                },
                ..builtin("std/report/html@v1")
            },
        ];
        assert!(validate_reporters(&ok).is_ok());

        let bad_use = vec![ReporterConfig {
            with: ReporterWith {
                path: Some("out.xml".into()),
                ..ReporterWith::default()
            },
            ..builtin("xml")
        }];
        let err = validate_reporters(&bad_use).unwrap_err();
        assert!(err.contains("unknown reporter 'xml'"), "{err}");

        let no_dest = vec![builtin("std/report/json@v1")];
        let err = validate_reporters(&no_dest).unwrap_err();
        assert!(err.contains("needs a destination"), "{err}");

        let both = vec![ReporterConfig {
            with: ReporterWith {
                path: Some("a".into()),
                url: Some("http://x".into()),
                ..ReporterWith::default()
            },
            ..builtin("std/report/json@v1")
        }];
        let err = validate_reporters(&both).unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn streaming_reporters_validate_destinations_and_capabilities() {
        // Streaming reporters stream; a url/endpoint is required.
        let ok = vec![
            ReporterConfig {
                during_run: true,
                with: ReporterWith {
                    url: Some("http://localhost:7999".into()),
                    ..ReporterWith::default()
                },
                ..builtin("std/report/serve@v1")
            },
            ReporterConfig {
                during_run: true,
                with: ReporterWith {
                    url: Some("http://alloy:8429/api/v1/metrics/write".into()),
                    histogram: Some("native".into()),
                    ..ReporterWith::default()
                },
                ..builtin("std/report/prometheus-rw@v1")
            },
            ReporterConfig {
                during_run: true,
                with: ReporterWith {
                    endpoint: Some("http://otel:4317".into()),
                    ..ReporterWith::default()
                },
                ..builtin("std/report/otel@v1")
            },
        ];
        assert!(validate_reporters(&ok).is_ok(), "{}", {
            validate_reporters(&ok).unwrap_err()
        });

        // `during_run` on a final-only reporter is a configuration error.
        let bad = vec![ReporterConfig {
            during_run: true,
            with: ReporterWith {
                path: Some("out.json".into()),
                ..ReporterWith::default()
            },
            ..builtin("std/report/json@v1")
        }];
        let err = validate_reporters(&bad).unwrap_err();
        assert!(err.contains("final-only"), "{err}");

        // Missing destinations.
        let err = validate_reporters(&[builtin("std/report/prometheus-rw@v1")]).unwrap_err();
        assert!(err.contains("with.url"), "{err}");
        let err = validate_reporters(&[builtin("std/report/otel@v1")]).unwrap_err();
        assert!(err.contains("with.endpoint"), "{err}");

        // `histogram` is prom-rw-only, and must name a known mode.
        let bad = vec![ReporterConfig {
            with: ReporterWith {
                url: Some("http://x".into()),
                histogram: Some("weird".into()),
                ..ReporterWith::default()
            },
            ..builtin("std/report/prometheus-rw@v1")
        }];
        let err = validate_reporters(&bad).unwrap_err();
        assert!(err.contains("histogram"), "{err}");
        let bad = vec![ReporterConfig {
            with: ReporterWith {
                url: Some("http://x".into()),
                histogram: Some("classic".into()),
                ..ReporterWith::default()
            },
            ..builtin("std/report/serve@v1")
        }];
        let err = validate_reporters(&bad).unwrap_err();
        assert!(err.contains("prometheus-rw"), "{err}");
    }

    #[test]
    fn reserved_labels_and_non_scalars_are_rejected() {
        let mut r = builtin("std/report/serve@v1");
        r.with.url = Some("http://x".into());
        r.labels
            .insert("run_id".into(), Value::String("mine".into()));
        let err = validate_reporters(&[r]).unwrap_err();
        assert!(err.contains("reserved"), "{err}");

        let mut r = builtin("std/report/serve@v1");
        r.with.url = Some("http://x".into());
        r.labels.insert("worker".into(), Value::String("0".into()));
        assert!(validate_reporters(&[r]).is_err());

        let mut r = builtin("std/report/serve@v1");
        r.with.url = Some("http://x".into());
        r.labels.insert("ok".into(), Value::from(42));
        r.labels.insert("also_ok".into(), Value::String("v".into()));
        assert!(validate_reporters(&[r.clone()]).is_ok());
        r.labels.insert("bad".into(), Value::Array(vec![]));
        let err = validate_reporters(&[r]).unwrap_err();
        assert!(err.contains("scalar"), "{err}");
    }

    #[test]
    fn delivery_knobs_validate() {
        let mut r = builtin("std/report/serve@v1");
        r.with.url = Some("http://x".into());
        r.with.retry = Some(RetryConfig::default());
        r.with.final_timeout = Some("30s".into());
        r.with.max_pending = Some(24);
        assert!(validate_reporters(&[r.clone()]).is_ok());

        let mut bad = r.clone();
        bad.with.retry = Some(RetryConfig {
            max_attempts: 5,
            backoff: "nonsense".into(),
        });
        assert!(validate_reporters(&[bad]).unwrap_err().contains("backoff"));

        let mut bad = r.clone();
        bad.with.retry = Some(RetryConfig {
            max_attempts: 0,
            backoff: "1s..30s".into(),
        });
        assert!(validate_reporters(&[bad])
            .unwrap_err()
            .contains("max_attempts"));

        let mut bad = r.clone();
        bad.with.final_timeout = Some("whenever".into());
        assert!(validate_reporters(&[bad])
            .unwrap_err()
            .contains("final_timeout"));

        let mut bad = r.clone();
        bad.with.max_pending = Some(0);
        assert!(validate_reporters(&[bad])
            .unwrap_err()
            .contains("max_pending"));

        let mut bad = r;
        bad.max_series = Some(0);
        assert!(validate_reporters(&[bad])
            .unwrap_err()
            .contains("max_series"));
    }

    #[test]
    fn legacy_with_max_samples_still_reads() {
        let mut r = builtin("std/report/json@v1");
        r.with.max_samples = Some(7);
        assert_eq!(r.max_samples(), 7);
        // Engine-level wins over the legacy key.
        r.max_samples = Some(3);
        assert_eq!(r.max_samples(), 3);
        let r = builtin("std/report/json@v1");
        assert_eq!(r.max_samples(), MAX_STORED_STEP_SAMPLES);
        assert_eq!(r.max_series(), DEFAULT_MAX_SERIES);
    }

    #[test]
    fn env_refs_resolve_host_side_and_missing_vars_fail() {
        std::env::set_var("PERFSCALE_TEST_TOKEN", "s3cret");
        assert_eq!(
            resolve_env_refs("Bearer ${PERFSCALE_TEST_TOKEN}").unwrap(),
            "Bearer s3cret"
        );
        assert!(resolve_env_refs("Bearer ${PERFSCALE_UNSET_VAR_XYZ}").is_err());

        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            "Bearer ${PERFSCALE_UNSET_VAR_XYZ}".to_string(),
        );
        let r = vec![ReporterConfig {
            with: ReporterWith {
                url: Some("http://x".into()),
                headers,
                ..ReporterWith::default()
            },
            ..builtin("std/report/json@v1")
        }];
        let err = validate_reporters(&r).unwrap_err();
        assert!(err.contains("PERFSCALE_UNSET_VAR_XYZ"), "{err}");
    }

    #[test]
    fn wasm_reporter_ids_parse_and_validate_the_module() {
        assert_eq!(
            wasm_reporter_module("wasm:./out/r.wasm"),
            Some("./out/r.wasm")
        );
        assert_eq!(wasm_reporter_module("wasm:"), None);
        assert_eq!(wasm_reporter_module("json"), None);

        // The module must exist, compile and export the reporter world
        // (builds without `wasm-libs` reject the reporter outright).
        let r = vec![builtin("wasm:/definitely/missing.wasm")];
        let err = validate_reporters(&r).unwrap_err();
        #[cfg(feature = "wasm-libs")]
        assert!(err.contains("failed to read"), "{err}");
        #[cfg(not(feature = "wasm-libs"))]
        assert!(err.contains("wasm-libs"), "{err}");

        // `path`/`url` are built-in destinations; they do not apply to wasm.
        let r = vec![ReporterConfig {
            with: ReporterWith {
                path: Some("out.txt".into()),
                ..ReporterWith::default()
            },
            ..builtin("wasm:./r.wasm")
        }];
        let err = validate_reporters(&r).unwrap_err();
        assert!(err.contains("do not apply to WASM reporters"), "{err}");

        // WASM reporters are final-only.
        let r = vec![ReporterConfig {
            during_run: true,
            ..builtin("wasm:./r.wasm")
        }];
        let err = validate_reporters(&r).unwrap_err();
        assert!(err.contains("final-only"), "{err}");

        // A wasm reporter needs no destination and passes the dest check.
        let r = vec![ReporterConfig {
            with: ReporterWith {
                output: Some("out".into()),
                config: Some(serde_json::json!({"style": "short"})),
                ..ReporterWith::default()
            },
            ..builtin("wasm:./r.wasm")
        }];
        let err = validate_reporters(&r).unwrap_err();
        assert!(!err.contains("needs a destination"), "{err}");
    }
}
