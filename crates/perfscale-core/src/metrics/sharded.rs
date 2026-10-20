//! Sharded run-metrics store: the fix for the contention the
//! `metrics_record_contended` bench measured (per-thread throughput fell
//! ~10x at 4 threads and ~35x at 16 on one `Arc<Mutex<Metrics>>`).
//!
//! [`ShardedMetrics`] is a fixed array of [`Mutex<Metrics>`] shards —
//! `2 × available_parallelism` (min 4). Writers land on a per-OS-thread
//! home shard (a thread-local index assigned round-robin on first use), so
//! VU worker threads almost never share a mutex; a tokio task that migrates
//! between worker threads simply changes home shard — correctness never
//! depends on the choice.
//!
//! Readers (the 5s `[stats]` reporter, the during-run snapshot pump, the
//! end-of-run summary, `std/thresholds@v1` gates) call [`merged()`], which
//! folds every shard into one plain [`Metrics`]. Merging is exact, not
//! approximate: all histograms share the same HDR bounds/significant
//! figures, so `add` sums identical bucket layouts with zero extra error;
//! counters and rate pairs sum; gauges take the max (the gauges in use are
//! peak readings — `engine_log_channel_depth_max` — and max is the
//! monotone, shard-order-independent merge for them);
//! `threshold_results` concatenate ([`crate::step::thresholds::combine`]
//! takes the worst status, so order is irrelevant). The merged view feeds
//! the same `summary_lines`/`stats_line`/`metric_snapshot` renderers, so
//! metric names and output bytes are unchanged from the single-mutex
//! layout.
//!
//! [`merged()`]: ShardedMetrics::merged

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde_json::{Map, Value};

use super::store::Metrics;

thread_local! {
    /// This OS thread's home shard index — assigned once on first write.
    static HOME_SHARD: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// Round-robin source for home-shard assignment.
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

/// The run's metrics, internally sharded — drop-in replacement for the
/// previous `Mutex<Metrics>`: same write verbs, merged reads.
///
/// Public (hidden) only so `benches/` can measure the contended shape — not
/// part of the supported API surface.
#[doc(hidden)]
#[derive(Debug)]
pub struct ShardedMetrics {
    shards: Vec<Mutex<Metrics>>,
}

impl Default for ShardedMetrics {
    /// `2 × available_parallelism` shards (min 4): enough that the VU worker
    /// threads of a tokio runtime rarely collide, small enough that a
    /// [`ShardedMetrics::merged`] pass stays cheap for the 5s reporter.
    fn default() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self::with_shards((2 * cpus).max(4))
    }
}

impl ShardedMetrics {
    /// A store with exactly `n` shards (tests pin small counts).
    pub fn with_shards(n: usize) -> Self {
        Self {
            shards: (0..n.max(1))
                .map(|_| Mutex::new(Metrics::default()))
                .collect(),
        }
    }

    /// Seed a store whose first shard holds the given accumulator — a test
    /// seam for code paths that hand-build a `Metrics` (thresholds tests).
    #[cfg(test)]
    pub(crate) fn from_metrics(m: Metrics) -> Self {
        let s = Self::with_shards(1);
        *s.shards[0].lock().unwrap() = m;
        s
    }

    /// This OS thread's home shard: a stable thread-local index, so the
    /// common case is an uncontended lock. Assigned round-robin on first
    /// use; `usize::MAX` marks "unassigned".
    fn home(&self) -> &Mutex<Metrics> {
        let idx = HOME_SHARD.with(|h| {
            let cur = h.get();
            if cur < self.shards.len() {
                cur
            } else {
                let v = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % self.shards.len();
                h.set(v);
                v
            }
        });
        &self.shards[idx]
    }

    /// One request-like step's sample — see [`Metrics::record`].
    pub fn record(&self, s: &crate::step::actions::HttpSample) {
        self.home().lock().unwrap().record(s);
    }

    /// Fold a step's custom `metrics` object — see [`Metrics::add_counters`].
    pub fn add_counters(&self, obj: &Map<String, Value>) {
        self.home().lock().unwrap().add_counters(obj);
    }

    /// One invocation outcome for a failure-rate metric — see
    /// [`Metrics::record_rate`].
    pub fn record_rate(&self, name: &str, failed: bool) {
        self.home().lock().unwrap().record_rate(name, failed);
    }

    /// The runner's per-step fold under ONE shard lock — see
    /// [`Metrics::add_counters_and_rates`].
    pub fn add_counters_and_rates(&self, obj: &Map<String, Value>, failed: bool, rates: &[&str]) {
        self.home()
            .lock()
            .unwrap()
            .add_counters_and_rates(obj, failed, rates);
    }

    /// One duration sample into a named histogram — see
    /// [`Metrics::record_hist_sample`].
    pub(crate) fn record_hist_sample(&self, name: &str, ms: f64) {
        self.home().lock().unwrap().record_hist_sample(name, ms);
    }

    /// Set a gauge's current value in this thread's shard — see
    /// [`Metrics::set_gauge`]. Shards merge gauges with max.
    pub(crate) fn set_gauge(&self, name: &str, value: f64) {
        self.home().lock().unwrap().set_gauge(name, value);
    }

    /// Store a `std/thresholds@v1` outcome — see
    /// [`Metrics::record_threshold_result`].
    pub fn record_threshold_result(&self, result: crate::step::thresholds::ThresholdsSummary) {
        self.home().lock().unwrap().record_threshold_result(result);
    }

    /// One step invocation for the `report@v1` per-step section — see
    /// [`Metrics::record_step`].
    pub(crate) fn record_step(&self, name: &str, ms: f64, failed: bool) {
        self.home().lock().unwrap().record_step(name, ms, failed);
    }

    /// Total requests recorded across all shards.
    pub fn total_requests(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap().total_requests())
            .sum()
    }

    /// Fold every shard into one plain [`Metrics`] — the view all readers
    /// (summary, `[stats]`, snapshots, thresholds) render from. Locks each
    /// shard in turn, never two at once, so a merge cannot deadlock against
    /// writers. Merging is exact (see the module docs).
    pub fn merged(&self) -> Metrics {
        let mut out = Metrics::default();
        for shard in &self.shards {
            out.merge_from(&shard.lock().unwrap());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::actions::HttpSample;

    fn sample(ms: f64, failed: bool) -> HttpSample {
        HttpSample {
            duration_ms: ms,
            status: 200,
            failed,
        }
    }

    /// (a) Merge arithmetic: totals, failure counts, counter sums, histogram
    /// sample counts, and rate pairs equal the sum of what was written,
    /// however the writes spread across shards.
    #[test]
    fn merged_sums_every_kind_of_write() {
        let m = ShardedMetrics::with_shards(4);
        for i in 0..100u64 {
            m.record(&sample(1.0 + i as f64 * 0.01, i % 10 == 0));
            m.add_counters(
                serde_json::json!({ "msgs": 2.0, "batch_ms": [i as f64] })
                    .as_object()
                    .unwrap(),
            );
            m.record_rate("op_failed", i % 4 == 0);
        }
        let v = m.merged();
        assert_eq!(v.total, 100);
        assert_eq!(v.failures, 10);
        assert_eq!(v.counters.get("msgs"), Some(&200.0));
        assert_eq!(v.hists["batch_ms"].len(), 100);
        assert_eq!(v.rates.get("op_failed"), Some(&(100, 25)));
        // The request histogram merged exactly: percentiles match a direct
        // single-store recording of the same samples.
        let mut direct = Metrics::default();
        for i in 0..100u64 {
            direct.record(&sample(1.0 + i as f64 * 0.01, i % 10 == 0));
        }
        assert_eq!(v.durations_micros, direct.durations_micros);
    }

    /// Gauges merge with max (peak readings), not sum, not last-wins.
    #[test]
    fn merged_gauges_take_the_max() {
        let m = std::sync::Arc::new(ShardedMetrics::with_shards(2));
        m.set_gauge("engine_log_channel_depth_max", 5.0);
        // Force the second shard into play from another OS thread.
        let other = std::thread::spawn({
            let m = std::sync::Arc::clone(&m);
            move || m.set_gauge("engine_log_channel_depth_max", 9.0)
        });
        other.join().unwrap();
        let v = m.merged();
        assert_eq!(v.gauges.get("engine_log_channel_depth_max"), Some(&9.0));
    }

    /// (b) Byte-stability: the merged summary renders exactly what a single
    /// store fed the same operations would — sharding changes no metric
    /// name, shape, or value.
    #[test]
    fn merged_summary_is_byte_identical_to_single_store() {
        let sharded = ShardedMetrics::with_shards(8);
        let mut direct = Metrics::default();
        for i in 0..500u64 {
            let s = sample(0.5 + (i % 97) as f64 * 0.02, i % 50 == 0);
            sharded.record(&s);
            direct.record(&s);
            let obj = serde_json::json!({ "ws_msg_rtt": [1.0 + (i % 7) as f64], "ws_msgs": 1.0 });
            let obj = obj.as_object().unwrap();
            sharded.add_counters(obj);
            direct.add_counters(obj);
            sharded.record_rate("ws_msg_failed", i % 50 == 0);
            direct.record_rate("ws_msg_failed", i % 50 == 0);
        }
        sharded.set_gauge("engine_log_channel_depth_max", 3.0);
        direct.set_gauge("engine_log_channel_depth_max", 3.0);
        assert_eq!(
            sharded.merged().summary_lines(10.0, 500, 4),
            direct.summary_lines(10.0, 500, 4)
        );
    }

    /// Writes from many OS threads all land and merge; with fewer threads
    /// than shards the spread uses more than one shard.
    #[test]
    fn concurrent_writers_all_land() {
        let m = std::sync::Arc::new(ShardedMetrics::with_shards(8));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let m = std::sync::Arc::clone(&m);
            handles.push(std::thread::spawn(move || {
                for _ in 0..250 {
                    m.record(&sample(1.0, false));
                    m.record_rate("op_failed", false);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let v = m.merged();
        assert_eq!(v.total, 1000);
        assert_eq!(v.rates.get("op_failed"), Some(&(1000, 0)));
        let used = m
            .shards
            .iter()
            .filter(|s| s.lock().unwrap().total > 0)
            .count();
        assert!(
            used > 1,
            "4 threads over 8 shards should spread, used={used}"
        );
    }
}
