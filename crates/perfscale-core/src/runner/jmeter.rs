//! JMeter process runner.
//!
//! Runs an existing test plan headless via `jmeter -n -t <plan>`, streaming
//! stdout/stderr live. Every run writes sample results to a private CSV
//! `.jtl` (`-l <temp>/results.jtl`) with a forced deterministic field set —
//! the runner's own `-J` saveservice properties are appended after any user
//! properties, so the required settings always win. JMeter's own log goes to
//! a private `-j <temp>/jmeter.log` instead of the caller's working
//! directory. Both temp files are removed when the run ends.
//!
//! The `.jtl` is parsed incrementally (bounded memory: samples fold into HDR
//! histograms and counters, never a Vec of samples) into the shared
//! [`Metrics`](crate::metrics::Metrics) store, so the run ends with the full
//! k6-compatible summary (`http_req_duration` with p50/p90/p95/p99,
//! `http_req_failed`, `http_reqs`, plus `http_req_latency`/`http_req_connect`
//! and `data_received`/`data_sent` when the plan reports them), per-label
//! series, optional `std/thresholds@v1` gates, and the `report@v1` document.
//!
//! A plan with zero samples (e.g. an empty thread group) falls back to
//! translating jmeter's final console `summary =` line (avg/min/max only —
//! the console summary carries no percentiles). A malformed *nonempty* `.jtl`
//! is a run failure, never silently passed.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::metrics::Metrics;
use crate::report::stream::MetricSnapshot;
use crate::report::v1::ReportContext;
use crate::runner::{LogLine, LogSource, RunOutput};
use crate::step::actions::HttpSample;
use crate::step::thresholds;

/// Per-run options for a JMeter execution — load-shape-independent knobs the
/// CLI resolves from flags and the config file. [`JMeterOpts::default`] is
/// the bare `run_streaming` behavior: no `-J` properties, no gates, no
/// streaming, no report.
#[derive(Default)]
pub struct JMeterOpts {
    /// User `-J` properties (`-Jkey=value`), passed in order. The runner's
    /// required saveservice properties are appended after these, so user
    /// properties can never corrupt the `.jtl` format.
    pub properties: Vec<(String, String)>,
    /// Metrics reporting: reporter list and run labels. A non-empty reporter
    /// list makes the runner emit the machine-readable `report: {…}`
    /// (report@v1) line at the end of the run; `during_run` is set by the
    /// caller (CLI `wire_streaming`) together with [`Self::metrics_tx`].
    pub report: Option<crate::report::ReportRunConfig>,
    /// During-run snapshot pump: when set, the runner tails the `.jtl` while
    /// jmeter runs and sends a cumulative [`MetricSnapshot`] every
    /// [`Self::snapshot_interval_ms`], plus one terminal snapshot at the end.
    pub metrics_tx: Option<mpsc::Sender<MetricSnapshot>>,
    /// Snapshot interval for the `.jtl` tail pump; `0` selects the report
    /// block's `interval_ms` (or its default), clamped to ≥
    /// [`crate::report::MIN_INTERVAL_MS`].
    pub snapshot_interval_ms: u64,
    /// `std/thresholds@v1` gate parameter sets from the config's `after:`
    /// block (step-level `severity`/`message` already merged into `with`).
    /// Evaluated over the run's metric snapshot after the last sample;
    /// a violated `fail` gate or a rejected gate exits the run non-zero.
    pub thresholds: Vec<Value>,
    /// Distinct JMeter `label` values tracked as separate series before
    /// folding into the `__jmeter_other__` bucket. `0` selects
    /// [`DEFAULT_MAX_LABELS`]. Bounds memory and report cardinality.
    pub max_labels: usize,
}

/// Default cap on distinct per-label series ([`JMeterOpts::max_labels`]).
pub const DEFAULT_MAX_LABELS: usize = 64;

/// The series label carrying the JMeter sampler label.
const LABEL_KEY: &str = "label";
/// Bucket for labels beyond the cardinality cap.
const OVERFLOW_LABEL: &str = "__jmeter_other__";

/// The saveservice field set every run forces, appended after user
/// properties so these always win. CSV output with headers, the four
/// required fields plus the optional timings/sizes the runner folds into
/// standard metrics, and no failure messages (they can carry arbitrary
/// multi-line text, which would break line-oriented tailing).
const REQUIRED_JTL_PROPS: &[(&str, &str)] = &[
    ("jmeter.save.saveservice.output_format", "csv"),
    ("jmeter.save.saveservice.print_field_names", "true"),
    ("jmeter.save.saveservice.timeStamp", "true"),
    ("jmeter.save.saveservice.elapsed", "true"),
    ("jmeter.save.saveservice.label", "true"),
    ("jmeter.save.saveservice.responseCode", "true"),
    ("jmeter.save.saveservice.responseMessage", "false"),
    ("jmeter.save.saveservice.threadName", "true"),
    ("jmeter.save.saveservice.dataType", "false"),
    ("jmeter.save.saveservice.success", "true"),
    ("jmeter.save.saveservice.failureMessage", "false"),
    ("jmeter.save.saveservice.bytes", "true"),
    ("jmeter.save.saveservice.sentBytes", "true"),
    ("jmeter.save.saveservice.grpThreads", "true"),
    ("jmeter.save.saveservice.allThreads", "true"),
    ("jmeter.save.saveservice.URL", "true"),
    ("jmeter.save.saveservice.Latency", "true"),
    ("jmeter.save.saveservice.IdleTime", "false"),
    ("jmeter.save.saveservice.Connect", "true"),
    ("jmeter.save.saveservice.assertionResults", "false"),
    ("jmeter.save.saveservice.subresults", "false"),
    ("jmeter.save.saveservice.samplerData", "false"),
    ("jmeter.save.saveservice.responseHeaders", "false"),
    ("jmeter.save.saveservice.requestHeaders", "false"),
];

/// Spawn jmeter in non-GUI mode and return its live output plus final exit
/// code. Equivalent to [`run_streaming_opts`] with default options.
pub async fn run_streaming(plan: PathBuf) -> Result<RunOutput, String> {
    run_streaming_opts(plan, JMeterOpts::default()).await
}

/// Spawn jmeter with the given options and return its live output plus final
/// exit code.
///
/// Streams raw stdout/stderr while the run is in progress, tails the private
/// `.jtl` for live metric snapshots when `opts.metrics_tx` is set, then —
/// once the process exits — parses the remaining `.jtl`, emits the
/// k6-compatible summary, evaluates any thresholds gates (a `thresholds:
/// {…}` line, like the native engine), and emits the `report: {…}` line when
/// reporters are configured.
///
/// JMeter exits non-zero on plan/startup errors; sample failures inside a
/// plan do NOT change the exit code unless the plan itself gates on them.
/// The runner adds two failure sources of its own: a malformed nonempty
/// `.jtl` and a violated `severity: fail` thresholds gate both turn a
/// zero exit into `Some(1)` — test feedback, like k6 thresholds.
pub async fn run_streaming_opts(plan: PathBuf, opts: JMeterOpts) -> Result<RunOutput, String> {
    let run_id = Uuid::new_v4().to_string();
    let prefix = std::env::temp_dir().join(format!("perfscale-jmeter-{run_id}"));
    let jtl_path = PathBuf::from(format!("{}.jtl", prefix.display()));
    let log_path = PathBuf::from(format!("{}.log", prefix.display()));

    let started = std::time::Instant::now();
    let started_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let started_at = crate::summary::iso8601_utc(started_ms as u64 / 1000);

    let mut child = spawn_jmeter(&plan, &opts.properties, &jtl_path, &log_path)?;
    let pid = child.id();

    let (tx, rx) = mpsc::channel::<LogLine>(512);
    let tx_stdout = tx.clone();
    let tx_stderr = tx.clone();

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    // The final `summary =` line (vs. the periodic `summary +` deltas) is
    // captured while streaming — it is the fallback for zero-sample plans.
    let final_summary: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let summary_slot = Arc::clone(&final_summary);

    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.starts_with("summary =") {
                *summary_slot.lock().expect("summary slot poisoned") = Some(line.clone());
            }
            if tx_stdout
                .send(LogLine {
                    source: LogSource::Stdout,
                    text: line,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx_stderr
                .send(LogLine {
                    source: LogSource::Stderr,
                    text: line,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let parser = Arc::new(Mutex::new(JtlParser::new(opts.max_labels)));
    let identity = StreamIdentity {
        run_id: run_id.clone(),
        started_ms,
        labels: opts
            .report
            .as_ref()
            .map(|r| r.labels.clone())
            .unwrap_or_default(),
    };

    // Live `.jtl` tail: only when a snapshot consumer is attached. The pump
    // ticks on the configured interval; a full channel drops the snapshot
    // (values are cumulative — the next tick restores them), so a slow
    // reporter never blocks the run.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let tail_handle = opts.metrics_tx.clone().map(|metrics_tx| {
        let parser = Arc::clone(&parser);
        let jtl = jtl_path.clone();
        let identity = identity.clone();
        let interval = if opts.snapshot_interval_ms > 0 {
            opts.snapshot_interval_ms
        } else {
            opts.report
                .as_ref()
                .map(|r| r.interval_ms)
                .unwrap_or_else(|| crate::report::ReportRunConfig::default().interval_ms)
        }
        .max(crate::report::MIN_INTERVAL_MS);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(interval));
            let mut stop = stop_rx;
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = stop.changed() => break,
                }
                pump_jtl(&parser, &jtl, false).await;
                let snap = {
                    let p = parser.lock().expect("jtl parser poisoned");
                    build_snapshot(&p.metrics, &identity, p.samples, false)
                };
                let _ = metrics_tx.try_send(snap);
            }
        })
    });

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let code = match child.wait().await {
            Ok(status) => {
                debug!(%run_id, ?status, "jmeter exited");
                status.code()
            }
            Err(e) => {
                warn!(%run_id, error = %e, "jmeter wait error");
                None
            }
        };

        // Stop the live tail before the final parse so the parser lock is
        // uncontended and the terminal snapshot reflects every sample.
        let _ = stop_tx.send(true);
        if let Some(h) = tail_handle {
            let _ = h.await;
        }

        pump_jtl(&parser, &jtl_path, true).await;

        let wall_secs = started.elapsed().as_secs_f64();
        let mut lines: Vec<LogLine> = Vec::new();
        let mut fatal_jtl = false;
        let mut gate_failed = false;

        let (metrics_snapshot, samples, malformed, overflowed, parse_fatal) = {
            let p = parser.lock().expect("jtl parser poisoned");
            (
                jmeter_metric_snapshot(&p.metrics),
                p.samples,
                p.malformed,
                p.overflowed,
                p.fatal.clone(),
            )
        };

        if let Some(err) = parse_fatal {
            fatal_jtl = true;
            lines.push(LogLine {
                source: LogSource::System,
                text: format!("jmeter results: {err}"),
            });
        } else if samples == 0 && malformed > 0 {
            fatal_jtl = true;
            lines.push(LogLine {
                source: LogSource::System,
                text: format!(
                    "jmeter results: every row in results.jtl was malformed ({malformed}) — refusing to pass a run with no trustworthy samples"
                ),
            });
        } else {
            if malformed > 0 {
                lines.push(LogLine {
                    source: LogSource::System,
                    text: format!(
                        "jmeter results: skipped {malformed} malformed row(s) in results.jtl"
                    ),
                });
            }
            if overflowed > 0 {
                lines.push(LogLine {
                    source: LogSource::System,
                    text: format!(
                        "jmeter results: label cardinality cap reached — {overflowed} sample(s) folded into the '{OVERFLOW_LABEL}' series"
                    ),
                });
            }
            if samples == 0 {
                // Zero-sample plan (e.g. an empty thread group): fall back
                // to the console summary translation.
                let final_line = final_summary.lock().expect("summary slot poisoned").clone();
                match final_line.as_deref() {
                    Some(line) => {
                        for line in translate_summary(line) {
                            lines.push(LogLine {
                                source: LogSource::Stdout,
                                text: line,
                            });
                        }
                    }
                    None => lines.push(LogLine {
                        source: LogSource::System,
                        text: "jmeter produced no samples and no console `summary =` line — the plan may have no samplers".into(),
                    }),
                }
            } else {
                let summary = {
                    let p = parser.lock().expect("jtl parser poisoned");
                    jmeter_summary_lines(&p.metrics, wall_secs)
                };
                for line in summary {
                    lines.push(LogLine {
                        source: LogSource::Stdout,
                        text: line,
                    });
                }
            }
        }

        // `after:` thresholds gates over the run's metric snapshot — same
        // evaluation and `thresholds: {…}` line as the native engine. A
        // rejected gate (unknown metric, bad expression) becomes a synthetic
        // `fail` summary, so the CLI exits non-zero through the standard
        // thresholds path instead of passing a run with valid metrics.
        let mut gate_results = Vec::new();
        for params in &opts.thresholds {
            match thresholds::evaluate(params, &metrics_snapshot) {
                Ok(result) => gate_results.push(result),
                Err(e) => {
                    lines.push(LogLine {
                        source: LogSource::System,
                        text: format!("thresholds gate rejected: {e}"),
                    });
                    gate_results.push(gate_config_error(&e));
                }
            }
        }
        let combined = thresholds::combine(gate_results);
        if let Some(ref t) = combined {
            gate_failed = t.status == "fail";
            lines.push(LogLine {
                source: LogSource::Stdout,
                text: format!(
                    "thresholds: {}",
                    serde_json::to_string(t).expect("thresholds summary is always serializable")
                ),
            });
        }

        // The terminal cumulative snapshot — the streaming reporters' final
        // flush guarantees it lands before the channel closes.
        if let Some(metrics_tx) = &opts.metrics_tx {
            let snap = {
                let p = parser.lock().expect("jtl parser poisoned");
                build_snapshot(&p.metrics, &identity, p.samples, true)
            };
            let _ = metrics_tx.send(snap).await;
        }

        // Machine-readable report@v1 line when end-of-run reporters are
        // configured; the CLI parses it and drives delivery.
        if let Some(report_cfg) = opts.report.as_ref().filter(|r| !r.reporters.is_empty()) {
            let max_samples = report_cfg.reporters.iter().map(|r| r.max_samples()).max();
            let report = {
                let p = parser.lock().expect("jtl parser poisoned");
                crate::report::v1::build_report_v1(
                    &p.metrics,
                    combined.as_ref(),
                    None,
                    &ReportContext {
                        run_id: run_id.clone(),
                        engine: "jmeter".into(),
                        perfscale_version: env!("CARGO_PKG_VERSION").to_string(),
                        vus: None,
                        started_at,
                        wall_secs,
                        labels: report_cfg.labels.clone(),
                        step_order: Vec::new(),
                        iterations: samples,
                        vus_gauge: 0,
                        max_samples,
                    },
                )
            };
            lines.push(LogLine {
                source: LogSource::Stdout,
                text: format!(
                    "report: {}",
                    serde_json::to_string(&report).expect("report is always serializable")
                ),
            });
        }

        for line in lines {
            let _ = tx.send(line).await;
        }

        cleanup_temp(&prefix).await;

        let final_code = if fatal_jtl || gate_failed {
            Some(1)
        } else {
            code
        };
        // tx dropped here → channel closes
        let _ = exit_tx.send(final_code);
    });

    Ok(RunOutput {
        lines: rx,
        exit: exit_rx,
        pid,
    })
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StreamIdentity {
    run_id: String,
    started_ms: i64,
    labels: Map<String, Value>,
}

fn spawn_jmeter(
    plan: &Path,
    properties: &[(String, String)],
    jtl_path: &Path,
    log_path: &Path,
) -> Result<tokio::process::Child, String> {
    let mut cmd = Command::new("jmeter");
    cmd.arg("-n")
        .arg("-t")
        .arg(plan)
        .arg("-j")
        .arg(log_path)
        .arg("-l")
        .arg(jtl_path);
    for (k, v) in properties {
        cmd.arg(format!("-J{k}={v}"));
    }
    // Required JTL settings last: a later -J wins in JMeter, so user
    // properties can never break the parser's expected format.
    for (k, v) in REQUIRED_JTL_PROPS {
        cmd.arg(format!("-J{k}={v}"));
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| jmeter_exec_error(&e))
}

fn jmeter_exec_error(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "jmeter not found in PATH — install from https://jmeter.apache.org/download_jmeter.cgi (requires a JRE)".into()
    } else {
        format!("Failed to spawn jmeter: {e}")
    }
}

async fn cleanup_temp(prefix: &Path) {
    for suffix in [".jtl", ".log"] {
        let _ =
            tokio::fs::remove_file(PathBuf::from(format!("{}{suffix}", prefix.display()))).await;
    }
}

/// Column positions in the deterministic CSV header. The four required
/// fields are checked at header parse; the rest are folded in when present.
#[derive(Debug, Clone)]
struct JtlColumns {
    time_stamp: usize,
    elapsed: usize,
    label: usize,
    success: usize,
    latency: Option<usize>,
    connect: Option<usize>,
    bytes: Option<usize>,
    sent_bytes: Option<usize>,
    width: usize,
}

impl JtlColumns {
    fn parse(header_line: &str) -> Result<Self, String> {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(header_line.as_bytes());
        let header = reader
            .records()
            .next()
            .ok_or("empty results.jtl header")?
            .map_err(|e| format!("malformed results.jtl header: {e}"))?;
        let idx = |name: &str| header.iter().position(|h| h.trim() == name);
        let missing: Vec<&str> = ["timeStamp", "elapsed", "label", "success"]
            .into_iter()
            .filter(|name| idx(name).is_none())
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "results.jtl header is missing required column(s): {}",
                missing.join(", ")
            ));
        }
        Ok(Self {
            time_stamp: idx("timeStamp").expect("checked above"),
            elapsed: idx("elapsed").expect("checked above"),
            label: idx("label").expect("checked above"),
            success: idx("success").expect("checked above"),
            latency: idx("Latency"),
            connect: idx("Connect"),
            bytes: idx("bytes"),
            sent_bytes: idx("sentBytes"),
            width: header.len(),
        })
    }
}

/// Incremental CSV `.jtl` parser: keeps only the header, the byte offset,
/// the unparsed tail (a possibly incomplete trailing record), and the
/// cumulative metrics — never a Vec of samples.
struct JtlParser {
    columns: Option<JtlColumns>,
    offset: u64,
    pending: Vec<u8>,
    metrics: Metrics,
    /// Samples folded into the store.
    samples: u64,
    /// Rows skipped because a required value was malformed or the arity
    /// didn't match the header.
    malformed: u64,
    /// Samples whose label exceeded the cardinality cap.
    overflowed: u64,
    labels_seen: HashSet<String>,
    max_labels: usize,
    /// Header-level failure — the file can never parse.
    fatal: Option<String>,
}

impl JtlParser {
    fn new(max_labels: usize) -> Self {
        Self {
            columns: None,
            offset: 0,
            pending: Vec::new(),
            metrics: Metrics::default(),
            samples: 0,
            malformed: 0,
            overflowed: 0,
            labels_seen: HashSet::new(),
            max_labels: if max_labels == 0 {
                DEFAULT_MAX_LABELS
            } else {
                max_labels
            },
            fatal: None,
        }
    }

    /// Feed newly read bytes. Mid-run passes hold back a possibly incomplete
    /// trailing record (everything after the last newline) for the next
    /// pass; the final pass consumes the tail too.
    fn feed(&mut self, bytes: &[u8], final_flush: bool) {
        if self.fatal.is_some() {
            return;
        }
        self.offset += bytes.len() as u64;
        self.pending.extend_from_slice(bytes);
        let upto = if final_flush {
            self.pending.len()
        } else {
            match self.pending.iter().rposition(|b| *b == b'\n') {
                Some(i) => i + 1,
                None => return,
            }
        };
        let chunk: Vec<u8> = self.pending.drain(..upto).collect();
        let text = String::from_utf8_lossy(&chunk);

        let body = if self.columns.is_none() {
            match text.split_once('\n') {
                Some((header_line, rest)) => {
                    match JtlColumns::parse(header_line.trim_end_matches('\r')) {
                        Ok(cols) => {
                            self.columns = Some(cols);
                            rest
                        }
                        Err(e) => {
                            self.fatal = Some(e);
                            return;
                        }
                    }
                }
                // The final pass can see a header-only file with no newline.
                None if final_flush => {
                    match JtlColumns::parse(text.trim_end()) {
                        Ok(cols) => {
                            self.columns = Some(cols);
                        }
                        Err(e) => self.fatal = Some(e),
                    }
                    return;
                }
                None => return,
            }
        } else {
            &*text
        };

        let width = self.columns.as_ref().expect("parsed above").width;
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_reader(body.as_bytes());
        for record in reader.records() {
            match record {
                Ok(rec) => {
                    // Tolerate blank lines (the final pass may end on one).
                    if rec.len() == 1 && rec[0].trim().is_empty() {
                        continue;
                    }
                    if rec.len() != width {
                        self.malformed += 1;
                        continue;
                    }
                    self.fold_sample(&rec);
                }
                Err(_) => self.malformed += 1,
            }
        }
    }

    fn fold_sample(&mut self, rec: &csv::StringRecord) {
        let cols = self.columns.clone().expect("columns parsed");
        let elapsed: f64 = match rec[cols.elapsed].trim().parse() {
            Ok(v) if v >= 0.0 => v,
            _ => {
                self.malformed += 1;
                return;
            }
        };
        let ts: i64 = match rec[cols.time_stamp].trim().parse() {
            Ok(v) => v,
            _ => {
                self.malformed += 1;
                return;
            }
        };
        let failed = match rec[cols.success].trim() {
            "true" => false,
            "false" => true,
            _ => {
                self.malformed += 1;
                return;
            }
        };
        let _ = ts; // timestamps order the file; rates come from wall time
        let raw_label = rec[cols.label].to_string();
        let label = if self.labels_seen.contains(&raw_label) {
            raw_label
        } else if self.labels_seen.len() < self.max_labels {
            self.labels_seen.insert(raw_label.clone());
            raw_label
        } else {
            self.overflowed += 1;
            OVERFLOW_LABEL.to_string()
        };
        let labels = BTreeMap::from([(LABEL_KEY.to_string(), label)]);

        let sample = HttpSample {
            duration_ms: elapsed,
            status: 0,
            failed,
        };
        self.metrics.record_aggregate(&sample);
        self.metrics.record_labeled(&sample, &labels);

        let mut extra = Map::new();
        let opt_f64 = |col: Option<usize>| -> Option<f64> {
            col.and_then(|i| rec.get(i))
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| *v >= 0.0)
        };
        if let Some(v) = opt_f64(cols.latency) {
            extra.insert(
                "http_req_latency".to_string(),
                Value::from(vec![Value::from(v)]),
            );
        }
        if let Some(v) = opt_f64(cols.connect) {
            extra.insert(
                "http_req_connect".to_string(),
                Value::from(vec![Value::from(v)]),
            );
        }
        if let Some(v) = opt_f64(cols.bytes) {
            extra.insert("data_received".to_string(), Value::from(v));
        }
        if let Some(v) = opt_f64(cols.sent_bytes) {
            extra.insert("data_sent".to_string(), Value::from(v));
        }
        if !extra.is_empty() {
            self.metrics
                .add_counters_and_rates_labeled(&extra, failed, &[], &labels);
        }
        self.samples += 1;
    }
}

/// Read the `.jtl` forward from the parser's offset in bounded chunks and
/// fold every complete record into the parser. A missing file (jmeter died
/// before writing anything) is not an error here — the exit path decides
/// between the console fallback and a startup failure.
async fn pump_jtl(parser: &Arc<Mutex<JtlParser>>, path: &Path, final_flush: bool) {
    const CHUNK: usize = 1 << 20; // 1 MiB
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return,
    };
    loop {
        let offset = parser.lock().expect("jtl parser poisoned").offset;
        if file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
            return;
        }
        let mut buf = vec![0u8; CHUNK];
        let n = match file.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => return,
        };
        if n == 0 {
            break;
        }
        parser
            .lock()
            .expect("jtl parser poisoned")
            .feed(&buf[..n], false);
    }
    if final_flush {
        parser.lock().expect("jtl parser poisoned").feed(&[], true);
    }
}

/// The k6-compatible summary block, rendered from the same store fields as
/// the native engine's `http` family plus the generic custom-metric shapes.
/// Empty when no samples were recorded (the caller falls back to the console
/// summary).
fn jmeter_summary_lines(m: &Metrics, wall_secs: f64) -> Vec<String> {
    if m.total == 0 {
        return Vec::new();
    }
    let h = &m.durations_micros;
    let pct = |q: f64| -> f64 { h.value_at_quantile(q) as f64 / 1000.0 };
    let rps = m.total as f64 / wall_secs.max(0.001);
    let err = m.failures as f64 / m.total as f64 * 100.0;
    let mut lines = vec![
        format!(
            "http_req_duration......: avg={avg:.2}ms p(50)={p50:.2}ms p(90)={p90:.2}ms p(95)={p95:.2}ms p(99)={p99:.2}ms min={min:.2}ms max={max:.2}ms",
            avg = h.mean() / 1000.0,
            p50 = pct(0.50),
            p90 = pct(0.90),
            p95 = pct(0.95),
            p99 = pct(0.99),
            min = h.min() as f64 / 1000.0,
            max = h.max() as f64 / 1000.0,
        ),
        format!("http_req_failed........: {err:.2}%"),
        format!("http_reqs..............: {total} {rps:.2}/s", total = m.total),
    ];
    for (name, h) in &m.hists {
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
    for (name, total) in &m.counters {
        if m.rates.contains_key(name) {
            continue;
        }
        let rate = total / wall_secs.max(0.001);
        lines.push(format!("{name}: {total:.0} {rate:.2}/s"));
    }
    lines
}

/// A rejected gate (unknown metric, malformed expression) as a synthetic
/// `fail` summary — the CLI fails the run through the standard thresholds
/// path (`thresholds: {…}` line → non-zero exit), never silently passing.
fn gate_config_error(err: &str) -> thresholds::ThresholdsSummary {
    thresholds::ThresholdsSummary {
        status: "fail".into(),
        message: format!("config error: {err}"),
        violations: Vec::new(),
        checks: Vec::new(),
    }
}

/// The JMeter threshold/snapshot aggregate map: the store's metric snapshot
/// plus the aggregate request counter (`http_reqs`), which the store keeps
/// as the `total` field rather than a named counter. One helper feeds both
/// the final thresholds evaluation and every live [`MetricSnapshot`], so the
/// two can never drift.
fn jmeter_metric_snapshot(m: &Metrics) -> BTreeMap<String, crate::step::thresholds::MetricAgg> {
    let mut snap = m.metric_snapshot();
    if m.total > 0 {
        snap.insert(
            "http_reqs".to_string(),
            crate::step::thresholds::MetricAgg::counter(m.total as f64),
        );
    }
    snap
}

/// One cumulative snapshot from the store — the same shape the native
/// engine's pump emits (aggregate map plus label-aware series with HDR
/// bucket counts).
fn build_snapshot(
    m: &Metrics,
    identity: &StreamIdentity,
    samples: u64,
    terminal: bool,
) -> MetricSnapshot {
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let series = m
        .labeled_series()
        .iter()
        .map(|((name, labels), s)| {
            let agg = match s.kind {
                thresholds::MetricKind::Sample => thresholds::MetricAgg::sample(
                    s.hist.as_ref().expect("sample series carry a histogram"),
                ),
                thresholds::MetricKind::Counter => thresholds::MetricAgg::counter(s.counter),
                thresholds::MetricKind::Rate => thresholds::MetricAgg::rate(s.rate.0, s.rate.1),
            };
            let buckets = s
                .hist
                .as_ref()
                .map(|h| {
                    let mut out: Vec<crate::report::stream::BucketCount> = Vec::new();
                    let mut accounted = 0u64;
                    for b in h.iter_all() {
                        let n = b.count_since_last_iteration();
                        if n > 0 {
                            out.push(crate::report::stream::BucketCount {
                                le_ms: h.highest_equivalent(b.value_iterated_to()) as f64 / 1000.0,
                                count: n,
                            });
                            accounted += n;
                            if accounted >= h.len() {
                                break;
                            }
                        }
                    }
                    out
                })
                .unwrap_or_default();
            crate::report::stream::SeriesSnapshot {
                name: name.clone(),
                labels: labels.clone(),
                agg,
                gauge: s.is_gauge,
                buckets,
            }
        })
        .collect();
    MetricSnapshot {
        ts_ms,
        metrics: jmeter_metric_snapshot(m),
        iterations_completed: samples,
        vus: 0,
        gpu: Vec::new(),
        run_id: identity.run_id.clone(),
        started_ms: identity.started_ms,
        labels: identity.labels.clone(),
        series,
        terminal,
    }
}

/// Translate jmeter's final console summary
/// (`summary =  N in HH:MM:SS =  R/s Avg: A Min: L Max: H Err: E (P%)`)
/// into the k6-compatible summary block. Fallback for zero-sample plans —
/// the console line has no percentiles, so the duration line carries
/// `avg`/`min`/`max` only.
///
/// A zero-sample run prints placeholders instead of numbers
/// (`= ******/s Avg: 0 Min: 9223372036854775807 Max: -9223372036854775808`);
/// translate that shape into explicit zeros rather than emitting nothing or
/// leaking the sentinel values.
fn translate_summary(line: &str) -> Vec<String> {
    if line.starts_with("summary =") {
        let after = line.trim_start_matches("summary =").trim_start();
        if after.starts_with('0') && after[1..].trim_start().starts_with("in ") {
            return vec![
                "http_req_duration......: avg=0.00ms min=0.00ms max=0.00ms".to_string(),
                "http_req_failed........: 0.00%".to_string(),
                "http_reqs..............: 0 0.00/s".to_string(),
            ];
        }
    }
    let re = regex::Regex::new(
        r"summary =\s+(\d+) in \S+ =\s+([\d.]+)/s Avg:\s+(\d+) Min:\s+(\d+) Max:\s+(\d+) Err:\s+\d+ \(([\d.]+)%\)",
    )
    .expect("jmeter summary regex compiles");

    let Some(c) = re.captures(line) else {
        return vec![];
    };
    let (total, rps, avg, min, max, err_pct) = (&c[1], &c[2], &c[3], &c[4], &c[5], &c[6]);

    vec![
        format!("http_req_duration......: avg={avg}.00ms min={min}.00ms max={max}.00ms"),
        format!("http_req_failed........: {err_pct}%"),
        format!("http_reqs..............: {total} {rps}/s"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "timeStamp,elapsed,label,responseCode,threadName,success,bytes,sentBytes,grpThreads,allThreads,URL,Latency,Connect";

    fn parser() -> JtlParser {
        JtlParser::new(0)
    }

    fn feed_all(p: &mut JtlParser, text: &str) {
        p.feed(text.as_bytes(), true);
    }

    #[test]
    fn header_parse_requires_the_four_columns() {
        let err = JtlColumns::parse("timeStamp,elapsed,label").unwrap_err();
        assert!(err.contains("success"), "{err}");
        let err = JtlColumns::parse("foo,bar").unwrap_err();
        assert!(err.contains("timeStamp"), "{err}");
        assert!(err.contains("label"), "{err}");
    }

    #[test]
    fn header_parse_locates_optional_columns() {
        let cols = JtlColumns::parse(HEADER).unwrap();
        assert_eq!(cols.time_stamp, 0);
        assert_eq!(cols.elapsed, 1);
        assert_eq!(cols.label, 2);
        assert_eq!(cols.success, 5);
        assert_eq!(cols.latency, Some(11));
        assert_eq!(cols.connect, Some(12));
        assert_eq!(cols.bytes, Some(6));
        assert_eq!(cols.sent_bytes, Some(7));
        // Missing optional columns are fine.
        let cols = JtlColumns::parse("timeStamp,elapsed,label,success").unwrap();
        assert!(cols.latency.is_none() && cols.bytes.is_none());
    }

    #[test]
    fn feed_handles_quoted_labels_and_optional_fields() {
        let mut p = parser();
        feed_all(
            &mut p,
            &format!(
                "{HEADER}\n1720000000000,120,\"Login, warm\",200,tg,true,1024,256,1,1,http://x/,80,15\n1720000000100,45,plain,200,tg,false,512,128,1,1,http://x/,40,5\n"
            ),
        );
        assert!(p.fatal.is_none());
        assert_eq!(p.samples, 2);
        assert_eq!(p.malformed, 0);
        assert_eq!(p.metrics.total, 2);
        assert_eq!(p.metrics.failures, 1);
        // Quoted label survives with its comma.
        assert!(p.labels_seen.contains("Login, warm"));
        // Optional fields folded: latency/connect histograms, byte counters.
        let snap = p.metrics.metric_snapshot();
        assert_eq!(snap["http_req_latency"].count, 2.0);
        assert_eq!(snap["http_req_connect"].count, 2.0);
        assert_eq!(snap["data_received"].count, 1536.0);
        assert_eq!(snap["data_sent"].count, 384.0);
        // Per-label series exist.
        assert!(p
            .metrics
            .labeled_series()
            .keys()
            .any(|(n, l)| n == "http_req_duration"
                && l.get(LABEL_KEY) == Some(&"Login, warm".into())));
    }

    #[test]
    fn feed_counts_malformed_rows_without_dropping_good_ones() {
        let mut p = parser();
        feed_all(
            &mut p,
            &format!(
                "{HEADER}\n\
                 not-a-ts,120,x,200,tg,true,1,1,1,1,u,80,15\n\
                 1720000000000,abc,x,200,tg,true,1,1,1,1,u,80,15\n\
                 1720000000000,120,x,200,tg,maybe,1,1,1,1,u,80,15\n\
                 1720000000000,120,x,200,tg,true\n\
                 1720000000000,120,ok,200,tg,true,1,1,1,1,u,80,15\n"
            ),
        );
        assert_eq!(p.samples, 1);
        assert_eq!(p.malformed, 4);
    }

    #[test]
    fn feed_mid_run_holds_back_incomplete_trailing_record() {
        let mut p = parser();
        p.feed(
            format!("{HEADER}\n1720000000000,120,ok,200,tg,true,1,1,1,1,u,80,15\n1720000")
                .as_bytes(),
            false,
        );
        assert_eq!(p.samples, 1, "only the newline-terminated row parses");
        p.feed(b"000100,30,second,200,tg,true,1,1,1,1,u,20,3\n", false);
        assert_eq!(p.samples, 2);
        p.feed(&[], true);
        assert_eq!(p.samples, 2);
    }

    #[test]
    fn feed_header_only_file_is_zero_samples_not_fatal() {
        let mut p = parser();
        feed_all(&mut p, &format!("{HEADER}\n"));
        assert!(p.fatal.is_none());
        assert_eq!(p.samples, 0);
        assert_eq!(p.malformed, 0);
    }

    #[test]
    fn feed_missing_required_header_is_fatal() {
        let mut p = parser();
        feed_all(&mut p, "timeStamp,elapsed\n1720000000000,120\n");
        assert!(p.fatal.is_some());
        assert_eq!(p.samples, 0);
    }

    #[test]
    fn label_cardinality_overflow_folds_into_bucket() {
        let mut p = JtlParser::new(2);
        let rows = (0..4)
            .map(|i| format!("172000000000{i},10,label{i},200,tg,true,1,1,1,1,u,10,2"))
            .collect::<Vec<_>>()
            .join("\n");
        feed_all(&mut p, &format!("{HEADER}\n{rows}\n"));
        assert_eq!(p.samples, 4);
        assert_eq!(p.overflowed, 2);
        assert!(p.labels_seen.len() == 2);
        let keys: Vec<_> = p
            .metrics
            .labeled_series()
            .keys()
            .filter(|(n, _)| n == "http_req_duration")
            .map(|(_, l)| l[LABEL_KEY].clone())
            .collect();
        assert!(keys.contains(&OVERFLOW_LABEL.to_string()), "{keys:?}");
    }

    #[test]
    fn summary_lines_have_full_percentiles_and_rps() {
        let mut p = parser();
        let rows = (1..=100)
            .map(|i| format!("1720000000000,{i},x,200,tg,true,100,50,1,1,u,{i},{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        feed_all(&mut p, &format!("{HEADER}\n{rows}\n"));
        let lines = jmeter_summary_lines(&p.metrics, 10.0);
        assert!(lines[0].starts_with("http_req_duration"), "{}", lines[0]);
        assert!(lines[0].contains("p(95)="), "{}", lines[0]);
        assert!(lines[0].contains("min=1.00ms"), "{}", lines[0]);
        // HDR histograms (2 sig figs) quantize: assert within tolerance.
        let avg: f64 = lines[0]
            .split("avg=")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .trim_end_matches("ms")
            .parse()
            .unwrap();
        assert!((avg - 50.5).abs() < 0.5, "avg={avg}");
        assert_eq!(lines[1], "http_req_failed........: 0.00%");
        assert_eq!(lines[2], "http_reqs..............: 100 10.00/s");
        assert!(lines
            .iter()
            .any(|l| l.starts_with("http_req_latency: avg=")));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("http_req_connect: avg=")));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("data_received: 10000 1000.00/s")));
        // Parseable by the shared summary parser.
        let summary = crate::summary::parse_summary(&lines.join("\n")).expect("parseable");
        assert_eq!(summary.total_requests, 100);
        assert!(summary.p95_ms.is_some());
        assert!(summary.p99_ms.is_some());
    }

    #[test]
    fn summary_lines_empty_when_no_samples() {
        assert!(jmeter_summary_lines(&Metrics::default(), 1.0).is_empty());
    }

    #[test]
    fn thresholds_evaluate_over_jtl_metrics() {
        let mut p = parser();
        feed_all(
            &mut p,
            &format!(
                "{HEADER}\n1720000000000,100,x,200,tg,true,1,1,1,1,u,80,15\n1720000000100,200,x,500,tg,false,1,1,1,1,u,150,20\n"
            ),
        );
        let snap = p.metrics.metric_snapshot();
        let pass = thresholds::evaluate(
            &serde_json::json!({ "http_req_duration": ["p95<500"] }),
            &snap,
        )
        .unwrap();
        assert_eq!(pass.status, "pass");
        let fail = thresholds::evaluate(
            &serde_json::json!({ "http_req_failed": ["rate<0.4"], "severity": "fail", "message": "SLO" }),
            &snap,
        )
        .unwrap();
        assert_eq!(fail.status, "fail");
        assert!(fail.message.contains("SLO"));
        // Unknown metric is a gate error, not a pass.
        assert!(thresholds::evaluate(&serde_json::json!({ "nope": ["p95<1"] }), &snap).is_err());
    }

    #[test]
    fn http_reqs_count_is_available_to_thresholds_and_snapshots() {
        let mut p = parser();
        feed_all(
            &mut p,
            &format!(
                "{HEADER}\n1720000000000,100,x,200,tg,true,1,1,1,1,u,80,15\n1720000000100,200,x,500,tg,false,1,1,1,1,u,150,20\n"
            ),
        );
        // Threshold map: `http_reqs` count gates evaluate.
        let snap = jmeter_metric_snapshot(&p.metrics);
        assert_eq!(snap["http_reqs"].count, 2.0);
        let pass =
            thresholds::evaluate(&serde_json::json!({ "http_reqs": ["count==2"] }), &snap).unwrap();
        assert_eq!(pass.status, "pass");
        let fail =
            thresholds::evaluate(&serde_json::json!({ "http_reqs": ["count==1"] }), &snap).unwrap();
        assert_eq!(fail.status, "fail");
        // Live snapshot aggregate map carries the same entry.
        let identity = StreamIdentity {
            run_id: "r1".into(),
            started_ms: 1000,
            labels: Map::new(),
        };
        let snapshot = build_snapshot(&p.metrics, &identity, p.samples, false);
        assert_eq!(snapshot.metrics["http_reqs"].count, 2.0);
    }

    #[test]
    fn rejected_gate_yields_fail_summary_parseable_by_cli() {
        let err = thresholds::evaluate(
            &serde_json::json!({ "nope": ["p95<1"] }),
            &Metrics::default().metric_snapshot(),
        )
        .unwrap_err();
        let summary = gate_config_error(&err);
        assert_eq!(summary.status, "fail");
        assert!(summary.message.starts_with("config error: "));
        // The emitted `thresholds: {…}` line is what the CLI parses to fail
        // the run — pin that contract.
        let line = format!("thresholds: {}", serde_json::to_string(&summary).unwrap());
        let parsed = crate::summary::parse_thresholds(&line).expect("parseable");
        assert_eq!(parsed.status, "fail");
        assert!(parsed.message.contains("unknown metric 'nope'"));
    }

    #[test]
    fn snapshot_carries_aggregates_and_labeled_series() {
        let mut p = parser();
        feed_all(
            &mut p,
            &format!("{HEADER}\n1720000000000,100,x,200,tg,true,1,1,1,1,u,80,15\n"),
        );
        let identity = StreamIdentity {
            run_id: "r1".into(),
            started_ms: 1000,
            labels: Map::new(),
        };
        let snap = build_snapshot(&p.metrics, &identity, p.samples, true);
        assert!(snap.terminal);
        assert_eq!(snap.iterations_completed, 1);
        assert!(snap.metrics.contains_key("http_req_duration"));
        let series = snap
            .series
            .iter()
            .find(|s| s.name == "http_req_duration")
            .expect("duration series");
        assert_eq!(series.labels[LABEL_KEY], "x");
        assert!(!series.buckets.is_empty(), "HDR buckets ride the snapshot");
    }

    #[test]
    fn report_v1_maps_jtl_metrics_and_labels() {
        let mut p = parser();
        feed_all(
            &mut p,
            &format!(
                "{HEADER}\n1720000000000,100,login,200,tg,true,1024,256,1,1,u,80,15\n1720000000100,200,search,500,tg,false,2048,512,1,1,u,150,20\n"
            ),
        );
        let report = crate::report::v1::build_report_v1(
            &p.metrics,
            None,
            None,
            &ReportContext {
                run_id: "r1".into(),
                engine: "jmeter".into(),
                perfscale_version: "test".into(),
                vus: None,
                started_at: "2026-01-01T00:00:00Z".into(),
                wall_secs: 2.0,
                labels: Map::new(),
                step_order: Vec::new(),
                iterations: p.samples,
                vus_gauge: 0,
                max_samples: None,
            },
        );
        assert_eq!(report.run.engine, "jmeter");
        let fam = report
            .metrics
            .iter()
            .find(|f| f.name == "http_req_duration")
            .expect("duration family");
        assert!(fam.recorded);
        let labels: Vec<_> = fam
            .series
            .iter()
            .map(|s| s.labels[LABEL_KEY].as_str().unwrap().to_string())
            .collect();
        assert!(labels.contains(&"login".to_string()), "{labels:?}");
        assert!(labels.contains(&"search".to_string()), "{labels:?}");
        let failed = report
            .metrics
            .iter()
            .find(|f| f.name == "http_req_failed")
            .expect("failed family");
        let total: f64 = failed.series.iter().filter_map(|s| s.total).sum();
        let fails: f64 = failed.series.iter().filter_map(|s| s.failed).sum();
        assert_eq!((total, fails), (2.0, 1.0));
        // JSON rendering works — this is what `--report json:…` writes.
        assert!(report.to_json().contains("\"http_req_duration\""));
        assert!(report.to_junit_xml().contains("testsuite"));
    }

    #[test]
    fn required_props_force_csv_and_win_over_user_props() {
        let mut cmd = Command::new("jmeter");
        cmd.arg("-n")
            .arg("-t")
            .arg(PathBuf::from("plan.jmx"))
            .arg("-j")
            .arg(PathBuf::from("/tmp/x.log"))
            .arg("-l")
            .arg(PathBuf::from("/tmp/x.jtl"));
        for (k, v) in [
            ("vus".to_string(), "50".to_string()),
            (
                "jmeter.save.saveservice.output_format".to_string(),
                "xml".to_string(),
            ),
        ] {
            cmd.arg(format!("-J{k}={v}"));
        }
        for (k, v) in REQUIRED_JTL_PROPS {
            cmd.arg(format!("-J{k}={v}"));
        }
        let rendered = format!("{cmd:?}");
        assert!(rendered.contains("-Jvus=50"), "{rendered}");
        assert!(rendered.contains("-l"), "{rendered}");
        // The user's xml override appears BEFORE the forced csv setting —
        // JMeter's last-wins semantics make csv effective.
        let user = rendered
            .find("-Jjmeter.save.saveservice.output_format=xml")
            .unwrap();
        let forced = rendered
            .rfind("-Jjmeter.save.saveservice.output_format=csv")
            .unwrap();
        assert!(user < forced);
    }

    fn jmeter_available() -> bool {
        std::process::Command::new("jmeter")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[test]
    fn jmeter_exec_error_not_found_suggests_install() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let msg = jmeter_exec_error(&e);
        assert!(msg.contains("jmeter not found in PATH"));
        assert!(msg.contains("jmeter.apache.org"));
    }

    #[test]
    fn jmeter_exec_error_other_kind_reports_generic_failure() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let msg = jmeter_exec_error(&e);
        assert!(msg.contains("Failed to spawn jmeter"));
        assert!(msg.contains("denied"));
    }

    #[test]
    fn translate_summary_parses_final_line() {
        let lines = translate_summary(
            "summary =   1200 in 00:00:15 =   80.0/s Avg:     4 Min:     1 Max:    42 Err:     3 (0.25%)",
        );
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("avg=4.00ms"));
        assert!(lines[0].contains("min=1.00ms"));
        assert!(lines[0].contains("max=42.00ms"));
        assert_eq!(lines[1], "http_req_failed........: 0.25%");
        assert_eq!(lines[2], "http_reqs..............: 1200 80.0/s");
        let summary = crate::summary::parse_summary(&lines.join("\n")).expect("parseable");
        assert_eq!(summary.total_requests, 1200);
        assert_eq!(summary.requests_per_sec, 80.0);
        assert_eq!(summary.avg_ms, Some(4.0));
        assert!(
            summary.p95_ms.is_none(),
            "no percentiles in the console summary"
        );
    }

    #[test]
    fn translate_summary_zero_sample_placeholders_become_zeros() {
        let lines = translate_summary(
            "summary =      0 in 00:00:00 = ******/s Avg:     0 Min: 9223372036854775807 Max: -9223372036854775808 Err:     0 (0.00%)",
        );
        assert_eq!(
            lines,
            vec![
                "http_req_duration......: avg=0.00ms min=0.00ms max=0.00ms",
                "http_req_failed........: 0.00%",
                "http_reqs..............: 0 0.00/s",
            ]
        );
        // Never leak the sentinel min/max.
        assert!(!lines.iter().any(|l| l.contains("9223372036854775807")));
        // The shared summary parser treats an all-zero block as "no HTTP
        // metrics" (None) — consistent with a zero-sample run.
        assert!(crate::summary::parse_summary(&lines.join("\n")).is_none());
    }

    #[test]
    fn translate_summary_ignores_progress_lines() {
        assert!(translate_summary(
            "summary +    600 in 00:00:07 =   85.7/s Avg:     4 Min:     1 Max:    20 Err:     0 (0.00%)"
        )
        .is_empty());
        assert!(translate_summary("unrelated log line").is_empty());
    }

    #[tokio::test]
    async fn run_streaming_missing_binary_is_clear_error() {
        if jmeter_available() {
            eprintln!("skipping: jmeter installed");
            return;
        }
        let err = run_streaming(PathBuf::from("plan.jmx")).await.unwrap_err();
        assert!(err.contains("jmeter not found in PATH"), "{err}");
    }

    #[tokio::test]
    async fn run_streaming_end_to_end_with_real_jmeter() {
        if !jmeter_available() {
            eprintln!("skipping: jmeter not installed");
            return;
        }
        // A minimal valid plan: one thread, one iteration, no samplers —
        // zero-sample fallback to the console summary.
        let dir = tempfile::tempdir().unwrap();
        let plan_path = dir.path().join("plan.jmx");
        tokio::fs::write(&plan_path, MINIMAL_PLAN).await.unwrap();

        let RunOutput {
            mut lines,
            exit,
            pid,
        } = run_streaming(plan_path).await.unwrap();
        assert!(
            pid.is_some(),
            "expected a pid for the spawned jmeter process"
        );

        let mut collected = Vec::new();
        while let Some(line) = lines.recv().await {
            collected.push(line.text);
        }
        assert!(!collected.is_empty(), "expected output lines from jmeter");
        assert!(
            collected.iter().any(|l| l.starts_with("http_reqs")),
            "translated k6-compatible summary expected: {collected:?}"
        );
        assert_eq!(exit.await.unwrap(), Some(0));
    }

    #[tokio::test]
    async fn run_streaming_real_jmeter_with_sampler_produces_jtl_metrics() {
        if !jmeter_available() {
            eprintln!("skipping: jmeter not installed");
            return;
        }
        // One HTTP sampler against an unreachable port: samples are recorded
        // as failures, which is test feedback — jmeter still exits 0.
        let dir = tempfile::tempdir().unwrap();
        let plan_path = dir.path().join("plan.jmx");
        tokio::fs::write(&plan_path, HTTP_PLAN).await.unwrap();

        let RunOutput {
            mut lines, exit, ..
        } = run_streaming(plan_path).await.unwrap();
        let mut collected = Vec::new();
        while let Some(line) = lines.recv().await {
            collected.push(line.text);
        }
        let summary = collected
            .iter()
            .find(|l| l.starts_with("http_req_duration"))
            .expect("JTL-derived duration line");
        assert!(summary.contains("p(95)="), "{summary}");
        assert!(
            collected
                .iter()
                .any(|l| l.starts_with("http_req_failed........: 100.00%")),
            "all samples failed against the unreachable target: {collected:?}"
        );
        assert_eq!(exit.await.unwrap(), Some(0), "sample failures are feedback");

        // Temp files are cleaned up.
        let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("perfscale-jmeter-")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    /// Smallest .jmx jmeter accepts: a test plan with an empty thread group.
    const MINIMAL_PLAN: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<jmeterTestPlan version="1.2" properties="5.0" jmeter="5.6.3">
  <hashTree>
    <TestPlan guiclass="TestPlanGui" testclass="TestPlan" testname="plan" enabled="true">
      <elementProp name="TestPlan.user_defined_variables" elementType="Arguments" guiclass="ArgumentsPanel" testclass="Arguments" testname="User Defined Variables" enabled="true">
        <collectionProp name="Arguments.arguments"/>
      </elementProp>
    </TestPlan>
    <hashTree>
      <ThreadGroup guiclass="ThreadGroupGui" testclass="ThreadGroup" testname="tg" enabled="true">
        <stringProp name="ThreadGroup.num_threads">1</stringProp>
        <stringProp name="ThreadGroup.ramp_time">1</stringProp>
        <elementProp name="ThreadGroup.main_controller" elementType="LoopController" guiclass="LoopControlPanel" testclass="LoopController" testname="Loop Controller" enabled="true">
          <boolProp name="LoopController.continue_forever">false</boolProp>
          <stringProp name="LoopController.loops">1</stringProp>
        </elementProp>
      </ThreadGroup>
      <hashTree/>
    </hashTree>
  </hashTree>
</jmeterTestPlan>
"#;

    /// One thread, one HTTP sampler against an unreachable local port.
    const HTTP_PLAN: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<jmeterTestPlan version="1.2" properties="5.0" jmeter="5.6.3">
  <hashTree>
    <TestPlan guiclass="TestPlanGui" testclass="TestPlan" testname="plan" enabled="true">
      <elementProp name="TestPlan.user_defined_variables" elementType="Arguments" guiclass="ArgumentsPanel" testclass="Arguments" testname="User Defined Variables" enabled="true">
        <collectionProp name="Arguments.arguments"/>
      </elementProp>
    </TestPlan>
    <hashTree>
      <ThreadGroup guiclass="ThreadGroupGui" testclass="ThreadGroup" testname="tg" enabled="true">
        <stringProp name="ThreadGroup.num_threads">1</stringProp>
        <stringProp name="ThreadGroup.ramp_time">1</stringProp>
        <elementProp name="ThreadGroup.main_controller" elementType="LoopController" guiclass="LoopControlPanel" testclass="LoopController" testname="Loop Controller" enabled="true">
          <boolProp name="LoopController.continue_forever">false</boolProp>
          <stringProp name="LoopController.loops">1</stringProp>
        </elementProp>
      </ThreadGroup>
      <hashTree>
        <HTTPSamplerProxy guiclass="HttpTestSampleGui" testclass="HTTPSamplerProxy" testname="home" enabled="true">
          <stringProp name="HTTPSampler.domain">127.0.0.1</stringProp>
          <stringProp name="HTTPSampler.port">1</stringProp>
          <stringProp name="HTTPSampler.path">/</stringProp>
        </HTTPSamplerProxy>
        <hashTree/>
      </hashTree>
    </hashTree>
  </hashTree>
</jmeterTestPlan>
"#;
}
