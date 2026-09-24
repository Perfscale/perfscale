//! Per-library run metrics (RFC 005 "metrics"): calls, errors, and call
//! durations per declared alias, aggregated over the whole run.
//!
//! One [`LibraryMetrics`] per run, shared (`Arc`) by every generator: the
//! hot path costs two atomic increments plus a short histogram lock per
//! library call. Durations reuse the same HDR histogram settings as step
//! durations (≤1% quantile error, 1µs..1h clamp), so the `libraries:` summary
//! line reports the same p50/p95/max shape the other metric lines do.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

// Same bounds as the step-duration histogram (`step::runner::Metrics`):
// sub-microsecond calls clamp to 1µs, the 1-hour ceiling is far beyond any
// sane library call, two significant digits → ≤1% quantile error.
const HIST_LOW_MICROS: u64 = 1;
const HIST_HIGH_MICROS: u64 = 3_600_000_000;
const HIST_SIGFIGS: u8 = 2;

/// Per-alias aggregates as reported in the run summary's `libraries:` section
/// and the `SummaryExport.libraries` map.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LibraryAliasSummary {
    /// Total `${alias.fn(...)}` calls (successful and failed).
    pub calls: u64,
    /// Calls that returned an error (unknown function, bad arguments).
    pub errors: u64,
    /// Median call duration, milliseconds.
    pub p50_ms: f64,
    /// 95th-percentile call duration, milliseconds.
    pub p95_ms: f64,
    /// Slowest call, milliseconds.
    pub max_ms: f64,
}

#[derive(Debug)]
struct AliasMetrics {
    alias: String,
    calls: AtomicU64,
    errors: AtomicU64,
    durations_micros: Mutex<hdrhistogram::Histogram<u64>>,
}

/// Run-scoped recorder, pre-seeded with every declared alias so the hot path
/// never allocates: `record` is a linear scan over a handful of entries.
#[derive(Debug)]
pub struct LibraryMetrics {
    entries: Vec<AliasMetrics>,
}

impl LibraryMetrics {
    /// A recorder for the given aliases (declaration order).
    pub fn new(aliases: impl IntoIterator<Item = String>) -> Self {
        Self {
            entries: aliases
                .into_iter()
                .map(|alias| AliasMetrics {
                    alias,
                    calls: AtomicU64::new(0),
                    errors: AtomicU64::new(0),
                    durations_micros: Mutex::new(
                        hdrhistogram::Histogram::new_with_bounds(
                            HIST_LOW_MICROS,
                            HIST_HIGH_MICROS,
                            HIST_SIGFIGS,
                        )
                        .expect("static histogram bounds are valid"),
                    ),
                })
                .collect(),
        }
    }

    /// Record one library call: outcome plus wall-clock duration. Unknown
    /// aliases (should not happen — generators attach declared aliases only)
    /// are dropped rather than panicking on the hot path.
    pub fn record(&self, alias: &str, elapsed: Duration, ok: bool) {
        let Some(entry) = self.entries.iter().find(|e| e.alias == alias) else {
            return;
        };
        entry.calls.fetch_add(1, Ordering::Relaxed);
        if !ok {
            entry.errors.fetch_add(1, Ordering::Relaxed);
        }
        let micros = elapsed.as_micros() as u64;
        let _ = entry
            .durations_micros
            .lock()
            .unwrap()
            .record(micros.clamp(HIST_LOW_MICROS, HIST_HIGH_MICROS));
    }

    /// Aggregated per-alias summary, `None` when not a single library call
    /// happened — non-library runs stay byte-identical to before.
    pub fn summary(&self) -> Option<std::collections::BTreeMap<String, LibraryAliasSummary>> {
        let mut out = std::collections::BTreeMap::new();
        for entry in &self.entries {
            let calls = entry.calls.load(Ordering::Relaxed);
            if calls == 0 {
                continue;
            }
            let h = entry.durations_micros.lock().unwrap();
            let ms = |micros: u64| micros as f64 / 1000.0;
            out.insert(
                entry.alias.clone(),
                LibraryAliasSummary {
                    calls,
                    errors: entry.errors.load(Ordering::Relaxed),
                    p50_ms: ms(h.value_at_quantile(0.50)),
                    p95_ms: ms(h.value_at_quantile(0.95)),
                    max_ms: ms(h.max()),
                },
            );
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_calls_errors_and_durations_per_alias() {
        let m = LibraryMetrics::new(["random".to_string(), "ids".to_string()]);
        m.record("random", Duration::from_micros(1500), true);
        m.record("random", Duration::from_micros(2500), false);
        m.record("ids", Duration::from_micros(500), true);

        let summary = m.summary().unwrap();
        let random = &summary["random"];
        assert_eq!(random.calls, 2);
        assert_eq!(random.errors, 1);
        assert!(random.p50_ms > 0.0 && random.p95_ms >= random.p50_ms);
        assert!(random.max_ms >= random.p95_ms);
        let ids = &summary["ids"];
        assert_eq!(ids.calls, 1);
        assert_eq!(ids.errors, 0);
    }

    #[test]
    fn no_calls_means_no_summary() {
        let m = LibraryMetrics::new(["random".to_string()]);
        assert!(m.summary().is_none());
        // An alias that was never called does not appear either.
        m.record("random", Duration::from_micros(100), true);
        let other = LibraryMetrics::new(["a".to_string(), "b".to_string()]);
        other.record("a", Duration::from_micros(100), true);
        let summary = other.summary().unwrap();
        assert!(summary.contains_key("a") && !summary.contains_key("b"));
    }

    #[test]
    fn unknown_alias_is_dropped_not_panicked() {
        let m = LibraryMetrics::new(["random".to_string()]);
        m.record("nope", Duration::from_micros(100), true);
        assert!(m.summary().is_none());
    }
}
