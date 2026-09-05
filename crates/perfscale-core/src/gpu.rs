//! Run-level GPU metrics collection.
//!
//! # Overview
//!
//! When the run config has a `gpu:` section (`gpu.enabled: true`), the native
//! engine samples the host's GPUs for the whole VU phase — one snapshot per
//! device every `interval_ms` — so the load profile can be correlated with
//! GPU state (crucial for `std/llm@v1` runs against a local server such as
//! Ollama or vLLM, where the GPU *is* the system under test).
//!
//! Two built-in sources:
//!
//! - `nvidia-smi` (default) — shells out to the `nvidia-smi` binary with a
//!   CSV query (`utilization.gpu`, `memory.used/total`, `temperature.gpu`,
//!   `power.draw`).
//! - `dcgm` — HTTP GET against a [dcgm-exporter](https://github.com/NVIDIA/dcgm-exporter)
//!   endpoint and parses the Prometheus text format
//!   (`DCGM_FI_DEV_GPU_UTIL`, `DCGM_FI_DEV_FB_USED/FREE`,
//!   `DCGM_FI_DEV_GPU_TEMP`, `DCGM_FI_DEV_POWER_USAGE`).
//!
//! GPU collection is strictly best-effort: no GPU, a missing `nvidia-smi`
//! binary, or an unreachable dcgm-exporter produces ONE warning at run start
//! and the run continues without GPU metrics — it never fails the run.
//!
//! # Extension seam
//!
//! Downstream (proprietary) crates plug in richer collectors via
//! [`register_gpu_collector`] — the same pattern as
//! [`crate::step::pubsub::register_pubsub_driver`]. A registered collector
//! shadows the built-in of the same name, so a pro build can replace
//! `nvidia-smi` with a NVML-based sampler exporting detailed metrics, and new
//! sources (`rocm-smi`, `powermetrics`, …) become selectable via
//! `gpu.source` without core changes. Collectors with more to say than the
//! built-in fields carry it on [`GpuSample::extra`] — flattened into the
//! summary timeseries under the collector's own key names.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::runner::{LogLine, LogSource};

// ---------------------------------------------------------------------------
// Configuration (`gpu:` section of the run config)
// ---------------------------------------------------------------------------

/// GPU metrics collection for a run (`gpu:` next to `vus`/`duration`).
///
/// Off by default; set `enabled: true` to sample every GPU (or the
/// `devices` subset) for the duration of the VU phase. Failures are
/// non-fatal: one warning is logged and the run continues without GPU
/// metrics.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GpuConfig {
    /// Master switch. When `false` the whole section is ignored.
    #[serde(default)]
    pub enabled: bool,

    /// Sampling interval in milliseconds (default 1000).
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,

    /// Collector source: `nvidia-smi` (default) or `dcgm`. Downstream crates
    /// may register additional names via [`register_gpu_collector`].
    #[serde(default = "default_source")]
    pub source: String,

    /// dcgm-exporter metrics endpoint, used with `source: dcgm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dcgm_url: Option<String>,

    /// Restrict sampling to these GPU indices (default: all devices the
    /// source reports).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devices: Option<Vec<u32>>,
}

fn default_interval_ms() -> u64 {
    1000
}

fn default_source() -> String {
    "nvidia-smi".to_string()
}

/// dcgm-exporter's default listen address.
pub const DEFAULT_DCGM_URL: &str = "http://127.0.0.1:9400/metrics";

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_ms: default_interval_ms(),
            source: default_source(),
            dcgm_url: None,
            devices: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Samples and summary
// ---------------------------------------------------------------------------

/// One GPU snapshot. Optional fields are `None` when the source reported
/// `N/A` (e.g. power draw on some virtualized GPUs) — they are omitted from
/// the JSON timeseries and excluded from aggregates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuSample {
    /// Sample time, milliseconds since the Unix epoch.
    pub ts_ms: u64,
    /// GPU index as reported by the source.
    pub index: u32,
    /// SM utilization, percent (0–100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization_pct: Option<f64>,
    /// Framebuffer memory in use, MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_used_mib: Option<f64>,
    /// Total framebuffer memory, MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_total_mib: Option<f64>,
    /// GPU core temperature, °C.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f64>,
    /// Board power draw, watts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_w: Option<f64>,
    /// Extra numeric fields from downstream (pro) collectors — SM/memory
    /// clocks, throttle-reason bitmasks, per-process aggregates. Flattened
    /// into the sample's JSON under their own keys; always empty from the
    /// built-in sources. Not aggregated by [`summarize`] — pro collectors
    /// expose their own rollups for these.
    #[serde(default, flatten)]
    pub extra: std::collections::BTreeMap<String, f64>,
}

impl GpuSample {
    /// A bare sample carrying only the device index; metric fields are
    /// filled in by parsers (and `ts_ms` by the sampling loop).
    fn bare(index: u32) -> Self {
        Self {
            ts_ms: 0,
            index,
            utilization_pct: None,
            memory_used_mib: None,
            memory_total_mib: None,
            temperature_c: None,
            power_w: None,
            extra: std::collections::BTreeMap::new(),
        }
    }
}

/// Per-device aggregates plus the raw timeseries, as embedded in the run
/// summary's `gpu` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuDeviceSummary {
    /// GPU index as reported by the source.
    pub index: u32,
    /// Every sample taken during the run, oldest first.
    pub samples: Vec<GpuSample>,
    /// Mean SM utilization over the run, percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_utilization_pct: Option<f64>,
    /// Peak SM utilization, percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_utilization_pct: Option<f64>,
    /// Peak framebuffer memory in use, MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_used_mib: Option<f64>,
    /// Total framebuffer memory, MiB (last reported value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_total_mib: Option<f64>,
    /// Peak GPU core temperature, °C.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_temperature_c: Option<f64>,
    /// Peak board power draw, watts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_power_w: Option<f64>,
}

/// The `gpu` section of the run summary: how the data was collected plus one
/// entry per device. Serialized as one machine-readable `gpu: {...}` line at
/// the end of a run (same framing as the `thresholds: {...}` gate line), and
/// embedded into `--summary-export` JSON by the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuSummary {
    /// Collector that produced the data (`nvidia-smi`, `dcgm`, …).
    pub source: String,
    /// Configured sampling interval, milliseconds.
    pub interval_ms: u64,
    /// Per-device timeseries + aggregates, ordered by GPU index.
    pub devices: Vec<GpuDeviceSummary>,
}

impl GpuSummary {
    /// Compact human-readable block for the end-of-run console summary:
    ///
    /// ```text
    /// gpu: 1 device, 30 samples every 1000ms (nvidia-smi)
    /// gpu0: util avg=45.2% max=98.0% vram max=12288/24576MiB temp max=67C power max=250.1W
    /// ```
    pub fn console_lines(&self) -> Vec<String> {
        let total_samples: usize = self.devices.iter().map(|d| d.samples.len()).sum();
        let mut lines = vec![format!(
            "gpu: {} device{}, {} samples every {}ms ({})",
            self.devices.len(),
            if self.devices.len() == 1 { "" } else { "s" },
            total_samples,
            self.interval_ms,
            self.source,
        )];
        for d in &self.devices {
            let f = |v: Option<f64>, unit: &str| {
                v.map(|x| format!("{x:.1}{unit}"))
                    .unwrap_or_else(|| "n/a".into())
            };
            let vram = match (d.max_memory_used_mib, d.memory_total_mib) {
                (Some(used), Some(total)) => format!("{used:.0}/{total:.0}MiB"),
                (Some(used), None) => format!("{used:.0}MiB"),
                _ => "n/a".into(),
            };
            lines.push(format!(
                "gpu{}: util avg={} max={} vram max={} temp max={} power max={}",
                d.index,
                f(d.avg_utilization_pct, "%"),
                f(d.max_utilization_pct, "%"),
                vram,
                f(d.max_temperature_c, "C"),
                f(d.max_power_w, "W"),
            ));
        }
        lines
    }
}

/// Aggregate raw samples into a [`GpuSummary`]. `None` when nothing was
/// sampled (collector never succeeded, or the device filter matched no GPU).
pub fn summarize(samples: Vec<GpuSample>, source: &str, interval_ms: u64) -> Option<GpuSummary> {
    if samples.is_empty() {
        return None;
    }
    let mut by_device: std::collections::BTreeMap<u32, Vec<GpuSample>> =
        std::collections::BTreeMap::new();
    for s in samples {
        by_device.entry(s.index).or_default().push(s);
    }
    let devices = by_device
        .into_iter()
        .map(|(index, samples)| {
            let vals = |pick: fn(&GpuSample) -> Option<f64>| -> Vec<f64> {
                samples.iter().filter_map(pick).collect()
            };
            let avg =
                |vs: &[f64]| (!vs.is_empty()).then(|| vs.iter().sum::<f64>() / vs.len() as f64);
            let max = |vs: &[f64]| vs.iter().copied().reduce(f64::max);
            let last = |pick: fn(&GpuSample) -> Option<f64>| -> Option<f64> {
                samples.iter().rev().find_map(pick)
            };
            let utils = vals(|s| s.utilization_pct);
            GpuDeviceSummary {
                index,
                avg_utilization_pct: avg(&utils),
                max_utilization_pct: max(&utils),
                max_memory_used_mib: max(&vals(|s| s.memory_used_mib)),
                memory_total_mib: last(|s| s.memory_total_mib),
                max_temperature_c: max(&vals(|s| s.temperature_c)),
                max_power_w: max(&vals(|s| s.power_w)),
                samples,
            }
        })
        .collect();
    Some(GpuSummary {
        source: source.to_string(),
        interval_ms,
        devices,
    })
}

// ---------------------------------------------------------------------------
// Collector seam
// ---------------------------------------------------------------------------

/// Boxed future returned by [`GpuCollector::sample`] — the trait stays
/// object-safe the same way [`crate::step::pubsub::PubSubDriver`] does.
pub type GpuSampleFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<GpuSample>, String>> + Send + 'a>>;

/// A pluggable GPU metrics source supplied by this crate (`nvidia-smi`,
/// `dcgm`) or a downstream one (proprietary NVML/rocm-smi/powermetrics
/// collectors, test fakes).
///
/// Implementations take ONE snapshot of every GPU they can see and return it
/// — the run's sampling loop owns the interval, timestamps, and device
/// filtering. Return `Err` only when the source is unusable (binary missing,
/// endpoint unreachable); the run treats the first `Err` as "no GPU metrics"
/// and stops polling with a single warning.
pub trait GpuCollector: Send + Sync {
    /// Collector name as used in `gpu.source` (e.g. `"nvidia-smi"`).
    fn name(&self) -> &'static str;

    /// Take one snapshot of every visible GPU. `GpuSample::ts_ms` is
    /// overwritten by the sampling loop — implementations may leave it 0.
    fn sample<'a>(&'a self) -> GpuSampleFuture<'a>;
}

fn collector_registry() -> &'static RwLock<Vec<Arc<dyn GpuCollector>>> {
    static REGISTRY: OnceLock<RwLock<Vec<Arc<dyn GpuCollector>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a custom [`GpuCollector`]. Typically called once at startup by a
/// downstream (proprietary) crate; a collector registered under a built-in
/// name (`nvidia-smi`, `dcgm`) shadows it, and new names become selectable
/// via `gpu.source`.
pub fn register_gpu_collector(collector: Arc<dyn GpuCollector>) {
    collector_registry().write().unwrap().push(collector);
}

/// Resolve the collector for a config. Registered collectors (pro seam) win
/// over the built-ins so a downstream crate can replace them; `dcgm` is
/// constructed per run because its endpoint URL is config, not identity.
fn resolve_collector(config: &GpuConfig) -> Result<Arc<dyn GpuCollector>, String> {
    if let Some(c) = collector_registry()
        .read()
        .unwrap()
        .iter()
        .find(|c| c.name() == config.source)
        .cloned()
    {
        return Ok(c);
    }
    match config.source.as_str() {
        "nvidia-smi" => Ok(Arc::new(NvidiaSmiCollector)),
        "dcgm" => Ok(Arc::new(DcgmCollector::new(
            config
                .dcgm_url
                .clone()
                .unwrap_or_else(|| DEFAULT_DCGM_URL.to_string()),
        ))),
        other => Err(format!(
            "unknown gpu source '{other}' — use 'nvidia-smi' or 'dcgm' (or a collector registered via register_gpu_collector)"
        )),
    }
}

// ---------------------------------------------------------------------------
// Built-in: nvidia-smi
// ---------------------------------------------------------------------------

/// The nvidia-smi CSV query, in field order. Kept in one place so the parser
/// and the spawned command cannot drift apart.
const NVIDIA_SMI_FIELDS: [&str; 6] = [
    "index",
    "utilization.gpu",
    "memory.used",
    "memory.total",
    "temperature.gpu",
    "power.draw",
];

/// Samples NVIDIA GPUs by shelling out to `nvidia-smi` once per tick.
pub struct NvidiaSmiCollector;

impl GpuCollector for NvidiaSmiCollector {
    fn name(&self) -> &'static str {
        "nvidia-smi"
    }

    fn sample<'a>(&'a self) -> GpuSampleFuture<'a> {
        Box::pin(async {
            let output = tokio::process::Command::new("nvidia-smi")
                .arg(format!("--query-gpu={}", NVIDIA_SMI_FIELDS.join(",")))
                .arg("--format=csv,noheader,nounits")
                .output()
                .await
                .map_err(|e| format!("failed to run nvidia-smi: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "nvidia-smi exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            parse_nvidia_smi_csv(&String::from_utf8_lossy(&output.stdout))
        })
    }
}

/// Parse `nvidia-smi --format=csv,noheader,nounits` output into samples.
///
/// One line per GPU: `0, 45, 1234, 24576, 67, 250.41` — fields follow
/// [`NVIDIA_SMI_FIELDS`]. Any field may be `N/A` (e.g. power draw on
/// virtualized GPUs); it maps to `None` instead of failing the line. A line
/// with an unparsable index or the wrong field count fails the whole batch:
/// that means the query and parser drifted apart, which is a bug, not data.
pub fn parse_nvidia_smi_csv(output: &str) -> Result<Vec<GpuSample>, String> {
    let mut samples = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() != NVIDIA_SMI_FIELDS.len() {
            return Err(format!(
                "unexpected nvidia-smi output line ({} fields, expected {}): '{line}'",
                fields.len(),
                NVIDIA_SMI_FIELDS.len()
            ));
        }
        let num = |s: &str| -> Option<f64> {
            if s.eq_ignore_ascii_case("n/a") {
                None
            } else {
                s.parse().ok()
            }
        };
        let index: u32 = fields[0]
            .parse()
            .map_err(|_| format!("unexpected nvidia-smi gpu index: '{line}'"))?;
        let mut s = GpuSample::bare(index);
        s.utilization_pct = num(fields[1]);
        s.memory_used_mib = num(fields[2]);
        s.memory_total_mib = num(fields[3]);
        s.temperature_c = num(fields[4]);
        s.power_w = num(fields[5]);
        samples.push(s);
    }
    Ok(samples)
}

// ---------------------------------------------------------------------------
// Built-in: dcgm-exporter
// ---------------------------------------------------------------------------

/// Samples NVIDIA GPUs over HTTP from a dcgm-exporter Prometheus endpoint.
pub struct DcgmCollector {
    url: String,
    client: reqwest::Client,
}

impl DcgmCollector {
    /// A collector polling `url` (dcgm-exporter's `/metrics`).
    pub fn new(url: String) -> Self {
        Self {
            url,
            client: reqwest::Client::new(),
        }
    }
}

impl GpuCollector for DcgmCollector {
    fn name(&self) -> &'static str {
        "dcgm"
    }

    fn sample<'a>(&'a self) -> GpuSampleFuture<'a> {
        Box::pin(async {
            let body = self
                .client
                .get(&self.url)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .map_err(|e| format!("dcgm-exporter at {} unreachable: {e}", self.url))?
                .error_for_status()
                .map_err(|e| format!("dcgm-exporter at {} failed: {e}", self.url))?
                .text()
                .await
                .map_err(|e| format!("failed to read dcgm-exporter response: {e}"))?;
            Ok(parse_dcgm_metrics(&body))
        })
    }
}

/// Parse dcgm-exporter's Prometheus text format into samples.
///
/// Only the basic OSS metrics are read: `DCGM_FI_DEV_GPU_UTIL` (utilization
/// %), `DCGM_FI_DEV_FB_USED`/`DCGM_FI_DEV_FB_FREE` (framebuffer MiB — total is
/// derived as used+free), `DCGM_FI_DEV_GPU_TEMP` (°C) and
/// `DCGM_FI_DEV_POWER_USAGE` (W). Samples are keyed by the `gpu` label;
/// `NaN` values (dcgm's "not available") map to `None`. Unknown series and
/// `# HELP`/`# TYPE` comments are ignored, so newer exporters with extra
/// metrics parse unchanged.
pub fn parse_dcgm_metrics(body: &str) -> Vec<GpuSample> {
    fn gpu_index(labels: &str) -> Option<u32> {
        for part in labels.split(',') {
            let (k, v) = part.split_once('=')?;
            if k.trim() == "gpu" {
                return v.trim().trim_matches('"').parse().ok();
            }
        }
        None
    }

    let mut by_gpu: std::collections::BTreeMap<u32, GpuSample> = std::collections::BTreeMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `METRIC{labels} value` or bare `METRIC value`.
        let (name, labels, value) = match line.split_once('{') {
            Some((name, rest)) => {
                let Some((labels, value)) = rest.split_once('}') else {
                    continue;
                };
                (name, Some(labels), value.trim())
            }
            None => match line.split_once(char::is_whitespace) {
                Some((name, value)) => (name, None, value.trim()),
                None => continue,
            },
        };
        let value: f64 = match value.split_whitespace().next().unwrap_or("").parse::<f64>() {
            Ok(v) if v.is_finite() => v,
            _ => continue, // NaN / ±Inf / unparsable → treat as "not available"
        };
        let index = labels.and_then(gpu_index).unwrap_or(0);
        let s = by_gpu
            .entry(index)
            .or_insert_with(|| GpuSample::bare(index));
        match name {
            "DCGM_FI_DEV_GPU_UTIL" => s.utilization_pct = Some(value),
            "DCGM_FI_DEV_FB_USED" => s.memory_used_mib = Some(value),
            "DCGM_FI_DEV_GPU_TEMP" => s.temperature_c = Some(value),
            "DCGM_FI_DEV_POWER_USAGE" => s.power_w = Some(value),
            _ => {}
        }
        // FB_FREE carries no state of its own; it completes memory_total.
        if name == "DCGM_FI_DEV_FB_FREE" {
            if let Some(used) = s.memory_used_mib {
                s.memory_total_mib = Some(used + value);
            }
        }
    }
    by_gpu.into_values().collect()
}

// ---------------------------------------------------------------------------
// Sampling session (background task over the VU phase)
// ---------------------------------------------------------------------------

/// A running GPU sampler. Created by [`start`] when the config enables GPU
/// metrics; [`GpuSession::stop`] aborts the background task and aggregates
/// whatever was sampled.
pub struct GpuSession {
    source: String,
    interval_ms: u64,
    samples: Arc<Mutex<Vec<GpuSample>>>,
    task: tokio::task::JoinHandle<()>,
}

impl GpuSession {
    /// Stop sampling and aggregate. `None` when nothing was collected (the
    /// collector failed on its first tick — already warned about — or no
    /// device matched the filter).
    pub fn stop(self) -> Option<GpuSummary> {
        self.task.abort();
        let samples = std::mem::take(&mut *self.samples.lock().unwrap());
        summarize(samples, &self.source, self.interval_ms)
    }
}

/// Start the background GPU sampler for a run, or `None` when the config
/// disables it. Source-resolution failures (unknown `gpu.source`) warn once
/// and disable collection — GPU metrics never fail a run. The returned
/// session samples immediately, then every `interval_ms`; the FIRST sampling
/// error (no `nvidia-smi` binary, unreachable dcgm-exporter, …) logs one
/// warning and stops the collector, and the run proceeds without GPU data.
pub async fn start(config: Option<&GpuConfig>, tx: &mpsc::Sender<LogLine>) -> Option<GpuSession> {
    let config = config.filter(|c| c.enabled)?;
    // Floor at 10ms: below that the sampler would spend the run spawning
    // processes / HTTP requests instead of letting the VUs work.
    let interval_ms = config.interval_ms.max(10);

    let collector = match resolve_collector(config) {
        Ok(c) => c,
        Err(msg) => {
            warn(tx, &format!("gpu metrics disabled: {msg}")).await;
            return None;
        }
    };
    let source = config.source.clone();
    let devices = config.devices.clone();
    let samples = Arc::new(Mutex::new(Vec::new()));
    let task_samples = Arc::clone(&samples);
    let task_source = source.clone();
    let task_tx = tx.clone();

    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
        loop {
            interval.tick().await; // first tick fires immediately
            match collector.sample().await {
                Ok(batch) => {
                    let ts_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let mut guard = task_samples.lock().unwrap();
                    for mut s in batch {
                        if devices.as_ref().is_some_and(|ds| !ds.contains(&s.index)) {
                            continue;
                        }
                        s.ts_ms = ts_ms;
                        guard.push(s);
                    }
                }
                Err(e) => {
                    warn(
                        &task_tx,
                        &format!("gpu metrics unavailable ({task_source}): {e} — continuing without them"),
                    )
                    .await;
                    return;
                }
            }
        }
    });

    Some(GpuSession {
        source,
        interval_ms,
        samples,
        task,
    })
}

async fn warn(tx: &mpsc::Sender<LogLine>, text: &str) {
    // System, not stderr: on the agent wire stderr lines become `[err] `
    // entries, and the controlplane fails runs on those — a best-effort GPU
    // warning must never fail a run (see the module doc).
    let _ = tx
        .send(LogLine {
            source: LogSource::System,
            text: format!("[warn] {text}"),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------
    // nvidia-smi CSV parser
    // -------------------------------------------------------------

    #[test]
    fn nvidia_smi_parses_multiple_gpus() {
        let out = "0, 45, 1234, 24576, 67, 250.41\n1, 0, 11008, 24576, 41, 32.10\n";
        let samples = parse_nvidia_smi_csv(out).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].index, 0);
        assert_eq!(samples[0].utilization_pct, Some(45.0));
        assert_eq!(samples[0].memory_used_mib, Some(1234.0));
        assert_eq!(samples[0].memory_total_mib, Some(24576.0));
        assert_eq!(samples[0].temperature_c, Some(67.0));
        assert_eq!(samples[0].power_w, Some(250.41));
        assert_eq!(samples[1].index, 1);
        assert_eq!(samples[1].utilization_pct, Some(0.0));
    }

    #[test]
    fn nvidia_smi_na_fields_become_none() {
        // Virtualized GPUs commonly report N/A for power and temperature.
        let out = "0, 12, 4096, 24576, N/A, N/A\n";
        let samples = parse_nvidia_smi_csv(out).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].utilization_pct, Some(12.0));
        assert_eq!(samples[0].temperature_c, None);
        assert_eq!(samples[0].power_w, None);
    }

    #[test]
    fn nvidia_smi_rejects_drifted_output() {
        // Wrong field count = parser/query drift → error, not silent data.
        assert!(parse_nvidia_smi_csv("0, 45, 1234\n").is_err());
        assert!(parse_nvidia_smi_csv("not-a-gpu, 45, 1234, 24576, 67, 250.0\n").is_err());
    }

    #[test]
    fn nvidia_smi_empty_output_is_empty_vec() {
        assert_eq!(parse_nvidia_smi_csv("").unwrap(), Vec::new());
        assert_eq!(parse_nvidia_smi_csv("\n  \n").unwrap(), Vec::new());
    }

    // -------------------------------------------------------------
    // dcgm Prometheus parser
    // -------------------------------------------------------------

    const DCGM_FIXTURE: &str = r#"
# HELP DCGM_FI_DEV_GPU_UTIL GPU utilization (in %).
# TYPE DCGM_FI_DEV_GPU_UTIL gauge
DCGM_FI_DEV_GPU_UTIL{gpu="0",UUID="GPU-aaa",device="nvidia0",modelName="NVIDIA H100",Hostname="node1"} 87
DCGM_FI_DEV_GPU_UTIL{gpu="1",UUID="GPU-bbb",device="nvidia1",modelName="NVIDIA H100",Hostname="node1"} 3
# HELP DCGM_FI_DEV_FB_USED Framebuffer memory used (in MiB).
# TYPE DCGM_FI_DEV_FB_USED gauge
DCGM_FI_DEV_FB_USED{gpu="0",UUID="GPU-aaa"} 41088
DCGM_FI_DEV_FB_USED{gpu="1",UUID="GPU-bbb"} 512
DCGM_FI_DEV_FB_FREE{gpu="0",UUID="GPU-aaa"} 40471
DCGM_FI_DEV_FB_FREE{gpu="1",UUID="GPU-bbb"} 81047
DCGM_FI_DEV_GPU_TEMP{gpu="0",UUID="GPU-aaa"} 61
DCGM_FI_DEV_GPU_TEMP{gpu="1",UUID="GPU-bbb"} 34
DCGM_FI_DEV_POWER_USAGE{gpu="0",UUID="GPU-aaa"} 512.34
DCGM_FI_DEV_POWER_USAGE{gpu="1",UUID="GPU-bbb"} 89.07
# A metric this parser does not care about — must be ignored.
DCGM_FI_DEV_SM_CLOCK{gpu="0",UUID="GPU-aaa"} 1980
"#;

    #[test]
    fn dcgm_parses_basic_metrics_per_gpu() {
        let samples = parse_dcgm_metrics(DCGM_FIXTURE);
        assert_eq!(samples.len(), 2);
        let g0 = &samples[0];
        assert_eq!(g0.index, 0);
        assert_eq!(g0.utilization_pct, Some(87.0));
        assert_eq!(g0.memory_used_mib, Some(41088.0));
        assert_eq!(g0.memory_total_mib, Some(41088.0 + 40471.0));
        assert_eq!(g0.temperature_c, Some(61.0));
        assert_eq!(g0.power_w, Some(512.34));
        assert_eq!(samples[1].index, 1);
        assert_eq!(samples[1].utilization_pct, Some(3.0));
    }

    #[test]
    fn dcgm_nan_values_become_none() {
        let body = "DCGM_FI_DEV_GPU_UTIL{gpu=\"0\"} NaN\nDCGM_FI_DEV_FB_USED{gpu=\"0\"} 1024\n";
        let samples = parse_dcgm_metrics(body);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].utilization_pct, None);
        assert_eq!(samples[0].memory_used_mib, Some(1024.0));
    }

    #[test]
    fn dcgm_empty_and_comment_only_bodies_parse_empty() {
        assert_eq!(parse_dcgm_metrics(""), Vec::new());
        assert_eq!(
            parse_dcgm_metrics("# HELP x y\n# TYPE x gauge\n"),
            Vec::new()
        );
    }

    // -------------------------------------------------------------
    // Aggregation
    // -------------------------------------------------------------

    fn sample(index: u32, util: Option<f64>, vram: Option<f64>, ts: u64) -> GpuSample {
        GpuSample {
            ts_ms: ts,
            index,
            utilization_pct: util,
            memory_used_mib: vram,
            memory_total_mib: Some(24576.0),
            temperature_c: Some(60.0),
            power_w: Some(200.0),
            extra: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn summarize_aggregates_per_device() {
        let samples = vec![
            sample(0, Some(10.0), Some(1000.0), 1),
            sample(1, Some(50.0), Some(2000.0), 1),
            sample(0, Some(90.0), Some(3000.0), 2),
            sample(1, None, Some(4000.0), 2), // N/A util excluded from avg
        ];
        let s = summarize(samples, "nvidia-smi", 1000).unwrap();
        assert_eq!(s.source, "nvidia-smi");
        assert_eq!(s.devices.len(), 2);
        let d0 = &s.devices[0];
        assert_eq!(d0.index, 0);
        assert_eq!(d0.samples.len(), 2);
        assert_eq!(d0.avg_utilization_pct, Some(50.0));
        assert_eq!(d0.max_utilization_pct, Some(90.0));
        assert_eq!(d0.max_memory_used_mib, Some(3000.0));
        assert_eq!(d0.memory_total_mib, Some(24576.0));
        assert_eq!(d0.max_temperature_c, Some(60.0));
        assert_eq!(d0.max_power_w, Some(200.0));
        let d1 = &s.devices[1];
        assert_eq!(d1.avg_utilization_pct, Some(50.0));
        assert_eq!(d1.max_memory_used_mib, Some(4000.0));
    }

    #[test]
    fn summarize_empty_is_none() {
        assert!(summarize(Vec::new(), "nvidia-smi", 1000).is_none());
    }

    #[test]
    fn console_lines_render_aggregates_and_na() {
        let s = summarize(
            vec![
                sample(0, Some(45.2), Some(12288.0), 1),
                GpuSample {
                    ts_ms: 2,
                    index: 0,
                    utilization_pct: Some(98.0),
                    memory_used_mib: Some(12288.0),
                    memory_total_mib: Some(24576.0),
                    temperature_c: None,
                    power_w: None,
                    extra: std::collections::BTreeMap::new(),
                },
            ],
            "nvidia-smi",
            1000,
        )
        .unwrap();
        let lines = s.console_lines();
        assert_eq!(
            lines[0],
            "gpu: 1 device, 2 samples every 1000ms (nvidia-smi)"
        );
        assert!(
            lines[1].starts_with("gpu0: util avg=71.6% max=98.0%"),
            "{lines:?}"
        );
        assert!(lines[1].contains("vram max=12288/24576MiB"), "{lines:?}");
        assert!(lines[1].contains("temp max=60.0C"), "{lines:?}");
        assert!(lines[1].contains("power max=200.0W"), "{lines:?}");
    }

    #[test]
    fn gpu_summary_json_round_trips() {
        let s = summarize(vec![sample(0, Some(42.0), Some(2048.0), 7)], "dcgm", 500).unwrap();
        let json = serde_json::to_string(&s).unwrap();
        let back: GpuSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        // Optional N/A fields are omitted from the wire format.
        let mut na = sample(0, None, None, 8);
        na.temperature_c = None;
        let json = serde_json::to_string(&na).unwrap();
        assert!(!json.contains("utilization_pct"), "{json}");
        assert!(!json.contains("memory_used_mib"), "{json}");
        assert!(!json.contains("temperature_c"), "{json}");
    }

    #[test]
    fn gpu_sample_extra_fields_flatten_and_round_trip() {
        // Pro collectors ride extra numeric fields (clocks, throttle bitmask)
        // on the same sample; they serialize flat, next to the built-ins.
        let mut s = sample(0, Some(42.0), Some(2048.0), 7);
        s.extra.insert("pro_gpu_clocks_sm_mhz".into(), 1980.0);
        s.extra.insert("pro_gpu_throttle_reasons".into(), 0.0);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"pro_gpu_clocks_sm_mhz\":1980.0"), "{json}");
        assert!(!json.contains("\"extra\""), "{json}");
        let back: GpuSample = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);

        // Empty extra map adds nothing to the wire format.
        let plain = sample(0, Some(1.0), Some(2.0), 3);
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("pro_gpu"), "{json}");
        let back: GpuSample = serde_json::from_str(&json).unwrap();
        assert!(back.extra.is_empty());
    }

    // -------------------------------------------------------------
    // Config serde
    // -------------------------------------------------------------

    #[test]
    fn gpu_config_defaults() {
        let cfg: GpuConfig = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.interval_ms, 1000);
        assert_eq!(cfg.source, "nvidia-smi");
        assert!(cfg.dcgm_url.is_none());
        assert!(cfg.devices.is_none());

        // Default = disabled, and the section round-trips.
        let cfg = GpuConfig::default();
        assert!(!cfg.enabled);
        let back: GpuConfig =
            serde_json::from_str(serde_json::to_string(&cfg).unwrap().as_str()).unwrap();
        assert!(!back.enabled);
    }

    #[test]
    fn unknown_gpu_source_is_a_resolve_error_not_a_crash() {
        let cfg = GpuConfig {
            enabled: true,
            source: "definitely-not-real".into(),
            ..GpuConfig::default()
        };
        let err = resolve_collector(&cfg).err().expect("unknown source fails");
        assert!(err.contains("unknown gpu source"), "{err}");
    }

    #[test]
    fn dcgm_url_default_applies_at_resolve() {
        let cfg = GpuConfig {
            enabled: true,
            source: "dcgm".into(),
            ..GpuConfig::default()
        };
        let c = resolve_collector(&cfg).unwrap();
        assert_eq!(c.name(), "dcgm");
    }

    // -------------------------------------------------------------
    // Session lifecycle (fake collectors through the public seam)
    // -------------------------------------------------------------

    struct FakeCollector;

    impl GpuCollector for FakeCollector {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn sample<'a>(&'a self) -> GpuSampleFuture<'a> {
            Box::pin(async { Ok(vec![sample(0, Some(55.0), Some(4096.0), 0)]) })
        }
    }

    struct FailingCollector;

    impl GpuCollector for FailingCollector {
        fn name(&self) -> &'static str {
            "failing"
        }

        fn sample<'a>(&'a self) -> GpuSampleFuture<'a> {
            Box::pin(async { Err("no gpu here".to_string()) })
        }
    }

    /// The registry is process-global, so every test touching it runs
    /// serially against the others (and against the runner integration tests
    /// in `step::runner`, which use the same lock file).
    #[tokio::test]
    #[serial_test::serial]
    async fn session_collects_and_stops() {
        register_gpu_collector(Arc::new(FakeCollector));
        let (tx, mut rx) = mpsc::channel(16);
        let cfg = GpuConfig {
            enabled: true,
            interval_ms: 20,
            source: "fake".into(),
            devices: None,
            dcgm_url: None,
        };
        let session = start(Some(&cfg), &tx).await.expect("session starts");
        tokio::time::sleep(Duration::from_millis(75)).await;
        let summary = session.stop().expect("samples collected");
        assert_eq!(summary.source, "fake");
        assert_eq!(summary.devices.len(), 1);
        assert!(
            summary.devices[0].samples.len() >= 2,
            "immediate + interval ticks: {:?}",
            summary.devices[0].samples
        );
        // Timestamps are stamped by the loop, in ms since epoch.
        assert!(summary.devices[0].samples[0].ts_ms > 1_000_000_000_000);
        while rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn session_device_filter_drops_unlisted_gpus() {
        register_gpu_collector(Arc::new(FakeCollector));
        let (tx, _rx) = mpsc::channel(16);
        let cfg = GpuConfig {
            enabled: true,
            interval_ms: 20,
            source: "fake".into(),
            devices: Some(vec![1]), // fake only reports gpu 0
            dcgm_url: None,
        };
        let session = start(Some(&cfg), &tx).await.expect("session starts");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(session.stop().is_none(), "filtered-out gpu → no summary");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn failing_collector_warns_once_and_yields_no_summary() {
        register_gpu_collector(Arc::new(FailingCollector));
        let (tx, mut rx) = mpsc::channel(16);
        let cfg = GpuConfig {
            enabled: true,
            interval_ms: 20,
            source: "failing".into(),
            devices: None,
            dcgm_url: None,
        };
        let session = start(Some(&cfg), &tx).await.expect("session starts");
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(session.stop().is_none());
        let mut warnings = 0;
        while let Ok(line) = rx.try_recv() {
            if line.text.contains("gpu metrics unavailable") {
                warnings += 1;
            }
        }
        assert_eq!(warnings, 1, "exactly one warning, then silence");
    }

    #[tokio::test]
    async fn disabled_or_absent_config_starts_no_session() {
        let (tx, _rx) = mpsc::channel(16);
        assert!(start(None, &tx).await.is_none());
        let cfg = GpuConfig::default(); // enabled: false
        assert!(start(Some(&cfg), &tx).await.is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unknown_source_warns_and_starts_no_session() {
        let (tx, mut rx) = mpsc::channel(16);
        let cfg = GpuConfig {
            enabled: true,
            source: "nope".into(),
            ..GpuConfig::default()
        };
        assert!(start(Some(&cfg), &tx).await.is_none());
        let line = rx.recv().await.expect("warning emitted");
        assert!(line.text.contains("unknown gpu source"), "{}", line.text);
        // Best-effort warnings go out as system lines: stderr would become a
        // run-failing `[err] ` entry on the agent → controlplane wire.
        assert_eq!(line.source, LogSource::System);
    }
}
