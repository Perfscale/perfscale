//! Engine self-observability (`engine_metrics:` in the run config).
//!
//! The engine measures the system under test but, by default, not itself.
//! When the run config has `engine_metrics.enabled: true`, a probe task
//! wakes every `interval_ms` during the VU phase and records the engine's
//! own overhead into the same [`Metrics`] store every step folds into, so
//! the readings land in the run summary and during-run snapshots next to
//! the workload metrics:
//!
//! - `engine_scheduler_lag_ms` (histogram) — tokio scheduler lag: how late
//!   the probe woke relative to its schedule (actual − expected wake time).
//!   A starved or oversubscribed runtime shows up here before it shows up
//!   in the workload's percentiles.
//! - `engine_log_channel_depth_max` (gauge) — peak pending lines in the
//!   bounded run-log channel (capacity 512) seen so far. A value
//!   approaching the channel capacity means step logging is back-pressuring
//!   the VUs (every `emit` awaits channel room).
//!
//! Off by default: the probe is one timer task plus one store lock per
//! tick, which is cheap but not free — and the probe writes into the very
//! mutex whose contention it helps diagnose, so runs that don't need engine
//! internals should not pay for it (nor see `engine_*` lines in the
//! summary). Nothing is spawned unless `enabled: true`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::store::Metrics;
use crate::runner::LogLine;

/// Name of the scheduler-lag histogram in the store / summary.
pub(crate) const SCHEDULER_LAG_METRIC: &str = "engine_scheduler_lag_ms";
/// Name of the log-channel depth gauge in the store / summary.
pub(crate) const LOG_CHANNEL_DEPTH_METRIC: &str = "engine_log_channel_depth_max";

/// Engine self-observability for a run (`engine_metrics:` next to
/// `vus`/`duration`).
///
/// Off by default; set `enabled: true` to probe scheduler lag and run-log
/// channel depth every `interval_ms` for the duration of the VU phase.
///
/// Unknown keys are rejected (`deny_unknown_fields`) — a typo'd key must
/// fail at load time instead of silently leaving the section at defaults.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EngineMetricsConfig {
    /// Master switch. When `false` the whole section is ignored and no
    /// probe task is spawned.
    #[serde(default)]
    pub enabled: bool,

    /// Probe interval in milliseconds (default 100, minimum 10). The
    /// scheduler-lag reading is measured against this interval, so it is
    /// also the resolution of the lag histogram.
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
}

fn default_interval_ms() -> u64 {
    100
}

impl Default for EngineMetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_ms: default_interval_ms(),
        }
    }
}

/// Start the probe task for the VU phase; `None` when the config is absent
/// or `enabled: false` (the default — zero cost). The returned handle is
/// aborted next to the `[stats]` reporter when the VU phase ends.
pub(crate) fn start(
    config: Option<&EngineMetricsConfig>,
    metrics: Arc<Mutex<Metrics>>,
    log_tx: tokio::sync::mpsc::Sender<LogLine>,
) -> Option<tokio::task::JoinHandle<()>> {
    let config = config.filter(|c| c.enabled)?;
    let period = Duration::from_millis(config.interval_ms.clamp(10, 60_000));
    Some(tokio::spawn(async move {
        // Manual deadline chain (not tokio's `interval`): the lag reading is
        // exactly "how late did this wake-up land", and the next deadline is
        // anchored to the actual wake instant so a late tick measures each
        // slot independently instead of collapsing the backlog into one.
        let mut next = tokio::time::Instant::now() + period;
        let mut max_depth: usize = 0;
        loop {
            tokio::time::sleep_until(next).await;
            let now = tokio::time::Instant::now();
            let lag_ms = now.saturating_duration_since(next).as_secs_f64() * 1000.0;
            next = now + period;
            // Pending lines = hand-out capacity the channel no longer has.
            let depth = log_tx.max_capacity() - log_tx.capacity();
            max_depth = max_depth.max(depth);
            let mut m = metrics.lock().unwrap();
            m.record_hist_sample(SCHEDULER_LAG_METRIC, lag_ms);
            m.set_gauge(LOG_CHANNEL_DEPTH_METRIC, max_depth as f64);
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::catalog;

    /// Off by default: absent config and `enabled: false` both spawn
    /// nothing — the probe must cost zero on regular runs.
    #[test]
    fn probe_is_off_unless_enabled() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let metrics = Arc::new(Mutex::new(Metrics::default()));
            let (tx, _rx) = tokio::sync::mpsc::channel(4);
            assert!(start(None, Arc::clone(&metrics), tx.clone()).is_none());
            let disabled = EngineMetricsConfig::default();
            assert!(start(Some(&disabled), Arc::clone(&metrics), tx.clone()).is_none());
            assert!(!disabled.enabled);
        });
    }

    /// An enabled probe records scheduler-lag samples and the depth gauge
    /// into the shared store, and both names are declared in the catalog.
    #[test]
    fn enabled_probe_records_engine_metrics() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let metrics = Arc::new(Mutex::new(Metrics::default()));
            let (tx, _rx) = tokio::sync::mpsc::channel(8);
            let config = EngineMetricsConfig {
                enabled: true,
                interval_ms: 10,
            };
            let probe = start(Some(&config), Arc::clone(&metrics), tx).expect("spawns");
            tokio::time::sleep(Duration::from_millis(55)).await;
            probe.abort();
            let m = metrics.lock().unwrap();
            let lag = m.hists.get(SCHEDULER_LAG_METRIC).expect("lag histogram");
            assert!(lag.len() >= 2, "expected several ticks, got {}", lag.len());
            assert_eq!(
                m.gauges.get(LOG_CHANNEL_DEPTH_METRIC),
                Some(&0.0),
                "empty channel stays at depth 0"
            );
        });
        for name in [SCHEDULER_LAG_METRIC, LOG_CHANNEL_DEPTH_METRIC] {
            assert!(
                catalog().iter().any(|(family, def)| *family == "engine" && def.name == name),
                "{name} must be declared in the engine family"
            );
        }
    }

    /// The gauge reports the channel's pending lines (capacity handed out).
    #[test]
    fn probe_observes_channel_depth() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let metrics = Arc::new(Mutex::new(Metrics::default()));
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            // Park 3 lines in the channel before the probe starts.
            for _ in 0..3 {
                tx.send(LogLine {
                    source: crate::runner::LogSource::Stdout,
                    text: String::new(),
                })
                .await
                .unwrap();
            }
            let config = EngineMetricsConfig {
                enabled: true,
                interval_ms: 10,
            };
            let probe = start(Some(&config), Arc::clone(&metrics), tx).expect("spawns");
            tokio::time::sleep(Duration::from_millis(35)).await;
            probe.abort();
            let depth = metrics
                .lock()
                .unwrap()
                .gauges
                .get(LOG_CHANNEL_DEPTH_METRIC)
                .copied()
                .unwrap_or(0.0);
            while rx.try_recv().is_ok() {}
            assert_eq!(depth, 3.0, "probe saw the 3 parked lines");
        });
    }
}
