//! During-run metrics streaming (`report:` run-config block).
//!
//! # Overview
//!
//! While a run is in progress the native engine can emit a cumulative
//! [`MetricSnapshot`](stream::MetricSnapshot) of every metric family every
//! `interval_ms` over an optional channel (`run_native`'s `metrics_tx`), and a
//! [`DuringRunShipper`](stream::DuringRunShipper) batches those snapshots into
//! samples and POSTs them to `<url>/api/v1/metrics` — the same endpoint the
//! end-of-run summary report uses. Shipping is adaptive: a CPU gate drops and
//! holds work while the host is saturated (the VU loop always wins), pending
//! batches are bounded with drop-oldest, and failures retry with exponential
//! backoff.
//!
//! Off by default: with no `report` block (or `during_run: false`) the engine
//! spawns no task and no channel traffic exists — the zero-overhead path.

pub mod cpu;
pub mod stream;

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use stream::{snapshot_to_samples, DuringRunShipper, MetricSnapshot, Sample};

/// Minimum snapshot/flush interval — below 1s the sampler and shipper would
/// spend the run servicing timers instead of letting the VUs work.
pub const MIN_INTERVAL_MS: u64 = 1000;

/// Metrics reporting for a run (`report:` block).
///
/// `url` selects the target (a `perfscale serve` or controlplane base URL);
/// it is `Option` here because the embedding config file keeps `url` in its
/// own `report:` section and the caller (CLI, agent) may also supply it via
/// a flag — the resolved URL wins and is what the shipper POSTs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReportRunConfig {
    /// Base URL of the report target, e.g. `http://localhost:7999`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Stream metric snapshots during the run (batched POSTs), not just the
    /// summary at the end. Off by default.
    #[serde(default)]
    pub during_run: bool,

    /// Snapshot/flush interval in milliseconds (default 5000, clamped to
    /// ≥ 1000 at use).
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,

    /// Flush a batch once it holds this many samples, even between ticks
    /// (default 500).
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// CPU gate: while the host's busy CPU% is at or above this value the
    /// shipper drops incoming snapshots and holds pending batches (no POSTs).
    /// `0.0` disables the gate. Off-Linux (no reading) the gate is inert.
    #[serde(default = "default_max_cpu_percent")]
    pub max_cpu_percent: f64,

    /// Maximum sealed batches awaiting delivery; beyond that the oldest is
    /// dropped with a warning (default 24).
    #[serde(default = "default_max_pending")]
    pub max_pending: usize,
}

pub(crate) fn default_interval_ms() -> u64 {
    5000
}

pub(crate) fn default_batch_size() -> usize {
    500
}

pub(crate) fn default_max_cpu_percent() -> f64 {
    90.0
}

pub(crate) fn default_max_pending() -> usize {
    24
}

impl Default for ReportRunConfig {
    fn default() -> Self {
        Self {
            url: None,
            during_run: false,
            interval_ms: default_interval_ms(),
            batch_size: default_batch_size(),
            max_cpu_percent: default_max_cpu_percent(),
            max_pending: default_max_pending(),
        }
    }
}

impl ReportRunConfig {
    /// Snapshot/flush interval, clamped to [`MIN_INTERVAL_MS`].
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.max(MIN_INTERVAL_MS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_empty_block_with_all_defaults() {
        let cfg: ReportRunConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, ReportRunConfig::default());
        assert!(!cfg.during_run);
        assert_eq!(cfg.interval_ms, 5000);
        assert_eq!(cfg.batch_size, 500);
        assert_eq!(cfg.max_cpu_percent, 90.0);
        assert_eq!(cfg.max_pending, 24);
        assert!(cfg.url.is_none());
    }

    #[test]
    fn interval_is_clamped_to_the_minimum() {
        let cfg = ReportRunConfig {
            interval_ms: 5,
            ..ReportRunConfig::default()
        };
        assert_eq!(cfg.interval(), Duration::from_millis(1000));
        let cfg = ReportRunConfig::default();
        assert_eq!(cfg.interval(), Duration::from_millis(5000));
    }
}
