//! Shared bounded delivery for streaming reporters (issue #12).
//!
//! One [`DeliveryWorker`] per reporter: snapshots arrive on a bounded FIFO
//! queue ([`SnapshotQueue`]), are encoded into sink items, batched by
//! `batch_size` and `interval`, and delivered with exactly one request in
//! flight. A slow or unreachable backend backs up ONLY its own queue — VUs
//! and other reporters never wait on it.
//!
//! # Semantics
//!
//! - **Bounded, drop-oldest**: both the snapshot queue and the sealed-batch
//!   backlog are capped (`max_pending`). Overflow drops the OLDEST entry and
//!   increments `perfscale_report_dropped_snapshots_total{reporter}` by the
//!   number of snapshots it held. Snapshots are cumulative, so the next one
//!   restores every counter and histogram — only time resolution is lost.
//! - **Batching**: the open batch is sealed in chunks of at most
//!   `batch_size` items — one oversized snapshot splits into sequential
//!   batches, in order, and whatever is open flushes on every `interval`
//!   tick.
//! - **Retries**: transient failures (network errors, HTTP 429/5xx,
//!   transient gRPC statuses) retry with exponential backoff inside the
//!   configured range; a server `Retry-After` hint wins when present. After
//!   `max_attempts` the batch is dropped and counted in
//!   `perfscale_report_errors_total{reporter}`. Permanent failures (other
//!   4xx, non-retryable gRPC statuses) are poison: dropped immediately.
//! - **Final flush**: when the input closes, the backlog is COALESCED to the
//!   latest cumulative snapshot (the engine's terminal one) — older batches
//!   are strict prefixes of it, so delivering them after the terminal values
//!   would be wrong; they are discarded (counted) and only the terminal
//!   snapshot is delivered, synchronously, bounded by `final_timeout`. A
//!   failed or timed-out flush warns — it never fails the load run.
//! - **CPU gate** (optional, the serve reporter's `max_cpu_percent`): while
//!   the host CPU is at or above the limit, delivery pauses; snapshots pile
//!   into the bounded queue (drop-oldest applies). The final flush ignores
//!   the gate — the VUs have stopped, the CPU is free.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::stream::MetricSnapshot;

/// `perfscale_report_dropped_snapshots_total{reporter}` — queue/backlog
/// overflow drops (oldest first).
pub const DROPPED_SNAPSHOTS_METRIC: &str = "perfscale_report_dropped_snapshots_total";
/// `perfscale_report_dropped_series_total{reporter}` — series dropped by the
/// `max_series` cardinality guard.
pub const DROPPED_SERIES_METRIC: &str = "perfscale_report_dropped_series_total";
/// `perfscale_report_errors_total{reporter}` — batches that exhausted their
/// retries, plus poison drops.
pub const REPORT_ERRORS_METRIC: &str = "perfscale_report_errors_total";

/// Per-run reporter counters, keyed by `(metric, reporter)` (issue #12).
/// One instance per run — concurrent runs (e.g. perfscaled shards) never
/// contaminate each other. The fan-out injects them into every snapshot as
/// counter series (`perfscale_report_dropped_series{reporter=…}`, etc.), so
/// each reporter's own backend receives them; the CLI folds them into the
/// end-of-run `report@v1` document.
#[derive(Debug, Default)]
pub struct ReporterCounters {
    inner: Mutex<BTreeMap<(String, String), u64>>,
    /// Drop accounting is idempotent per run: the same dropped series key or
    /// snapshot identity is counted once, however many batches/encodes would
    /// otherwise report it again (a snapshot split across batches, the
    /// terminal flush re-encoding a snapshot).
    seen_series: Mutex<std::collections::BTreeSet<(String, String)>>,
    seen_snapshots: Mutex<std::collections::BTreeSet<(String, String, i64)>>,
}

impl ReporterCounters {
    pub fn inc(&self, metric: &str, reporter: &str, by: u64) {
        let mut guard = self.inner.lock().unwrap();
        *guard
            .entry((metric.to_string(), reporter.to_string()))
            .or_insert(0) += by;
    }

    /// Count dropped series keys; a key already counted for this reporter in
    /// this run (e.g. the terminal flush re-encoding the same snapshot) does
    /// not increment again.
    pub fn note_dropped_series(&self, reporter: &str, keys: impl Iterator<Item = String>) {
        let mut seen = self.seen_series.lock().unwrap();
        let mut new = 0u64;
        for k in keys {
            if seen.insert((reporter.to_string(), k)) {
                new += 1;
            }
        }
        drop(seen);
        if new > 0 {
            self.inc(DROPPED_SERIES_METRIC, reporter, new);
        }
    }

    /// Count dropped snapshots by identity `(run_id, ts_ms)`; a snapshot
    /// split across several batches counts at most once for this reporter.
    pub fn note_dropped_snapshots(&self, reporter: &str, ids: impl Iterator<Item = (String, i64)>) {
        let mut seen = self.seen_snapshots.lock().unwrap();
        let mut new = 0u64;
        for id in ids {
            if seen.insert((reporter.to_string(), id.0, id.1)) {
                new += 1;
            }
        }
        drop(seen);
        if new > 0 {
            self.inc(DROPPED_SNAPSHOTS_METRIC, reporter, new);
        }
    }

    pub fn get(&self, metric: &str, reporter: &str) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .get(&(metric.to_string(), reporter.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// `(metric, reporter, value)` triples, sorted.
    pub fn snapshot(&self) -> Vec<(String, String, u64)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|((m, r), v)| (m.clone(), r.clone(), *v))
            .collect()
    }
}

/// Process-wide fallback registry, used by the legacy
/// [`super::serve::DuringRunShipper`] facade path (which predates per-run
/// counters) and by tests. New run wiring should always pass a per-run
/// [`ReporterCounters`].
static COUNTERS: OnceLock<Mutex<BTreeMap<(String, String), u64>>> = OnceLock::new();

fn global_counters() -> &'static Mutex<BTreeMap<(String, String), u64>> {
    COUNTERS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Increment a reporter counter in the process-wide fallback registry.
pub fn inc_counter(metric: &str, reporter: &str, by: u64) {
    let mut guard = global_counters().lock().unwrap();
    *guard
        .entry((metric.to_string(), reporter.to_string()))
        .or_insert(0) += by;
}

/// Read a reporter counter (tests and run-end reporting).
pub fn counter_value(metric: &str, reporter: &str) -> u64 {
    global_counters()
        .lock()
        .unwrap()
        .get(&(metric.to_string(), reporter.to_string()))
        .copied()
        .unwrap_or(0)
}

/// All reporter counters: `(metric, reporter, value)`.
pub fn all_counters() -> Vec<(String, String, u64)> {
    global_counters()
        .lock()
        .unwrap()
        .iter()
        .map(|((m, r), v)| (m.clone(), r.clone(), *v))
        .collect()
}

/// Increment in the per-run registry when set, else the process-wide one.
pub(crate) fn inc_with(
    counters: &Option<std::sync::Arc<ReporterCounters>>,
    metric: &str,
    reporter: &str,
    by: u64,
) {
    match counters {
        Some(c) => c.inc(metric, reporter, by),
        None => inc_counter(metric, reporter, by),
    }
}

/// Idempotent dropped-series accounting in the per-run registry; the legacy
/// fallback counts every report (no memory).
pub(crate) fn note_dropped_series_with(
    counters: &Option<std::sync::Arc<ReporterCounters>>,
    reporter: &str,
    keys: impl Iterator<Item = String>,
) {
    match counters {
        Some(c) => c.note_dropped_series(reporter, keys),
        None => inc_counter(DROPPED_SERIES_METRIC, reporter, keys.count() as u64),
    }
}

/// Idempotent dropped-snapshot accounting in the per-run registry; the
/// legacy fallback counts every reported id.
pub(crate) fn note_dropped_snapshots_with(
    counters: &Option<std::sync::Arc<ReporterCounters>>,
    reporter: &str,
    ids: impl Iterator<Item = (String, i64)> + Clone,
) {
    match counters {
        Some(c) => c.note_dropped_snapshots(reporter, ids),
        None => inc_counter(DROPPED_SNAPSHOTS_METRIC, reporter, ids.count() as u64),
    }
}

/// A snapshot's drop identity: `run_id` + `ts_ms` (the pump stamps one
/// snapshot per tick, so the pair is unique within a run).
pub(crate) fn snap_id(snap: &MetricSnapshot) -> (String, i64) {
    (snap.run_id.clone(), snap.ts_ms)
}

/// The registered reporter counter family names (no `_total` suffix) —
/// used to refresh/dedupe injected series and to exempt them from the
/// `max_series` guard (the engine-bounded self-counters must not eat the
/// user-metric cardinality budget).
pub(crate) const REPORTER_COUNTER_FAMILIES: [&str; 3] = [
    "perfscale_report_dropped_series",
    "perfscale_report_dropped_snapshots",
    "perfscale_report_errors",
];

/// True for a reporter's own counter series name, with or without the
/// `_total` export suffix.
pub(crate) fn is_reporter_counter(name: &str) -> bool {
    let bare = name.strip_suffix("_total").unwrap_or(name);
    REPORTER_COUNTER_FAMILIES.contains(&bare)
}

/// The counter series the fan-out injects into every snapshot: registered
/// family names WITHOUT the `_total` suffix (the exporters add it, like for
/// every counter), one series per reporter with a nonzero count.
pub fn counter_series(counters: &ReporterCounters) -> Vec<super::stream::SeriesSnapshot> {
    counters
        .snapshot()
        .into_iter()
        .map(|(metric, reporter, value)| {
            // `perfscale_report_dropped_series_total` → family name minus
            // the suffix.
            let name = metric.strip_suffix("_total").unwrap_or(&metric).to_string();
            super::stream::SeriesSnapshot {
                name,
                labels: BTreeMap::from([("reporter".to_string(), reporter)]),
                agg: crate::step::thresholds::MetricAgg::counter(value as f64),
                gauge: false,
                buckets: Vec::new(),
            }
        })
        .collect()
}

/// The outcome of one delivery attempt.
pub enum PostOutcome {
    Delivered,
    /// Transient failure — retry after the given hint (a server's
    /// `Retry-After`) or the exponential backoff when `None`.
    Retry {
        after: Option<Duration>,
        /// Short reason for logs, e.g. `status 503` — never a secret.
        reason: String,
    },
    /// Permanent failure — drop the batch, never retry.
    Poison(String),
}

/// A streaming sink: encodes cumulative snapshots into items and delivers
/// batches of them. One instance per configured reporter.
pub trait Sink: Send + 'static {
    /// The unit of batching (a decoded sample, a protobuf timeseries, …).
    type Item: Send + Sync;

    /// Reporter id, for counters and logs, e.g. `std/report/otel@v1`.
    fn reporter(&self) -> &str;

    /// Encode one cumulative snapshot into items (empty allowed).
    fn encode(&mut self, snap: &MetricSnapshot) -> Vec<Self::Item>;

    /// Deliver one sealed batch. `seq` is the batch's monotonically
    /// increasing sequence number, reused across retries so receivers can
    /// deduplicate.
    fn send<'a>(&'a mut self, items: &'a [Self::Item], seq: u64) -> BoxFuture<'a, PostOutcome>;
}

/// CPU-gate reader: reports the host busy CPU% (`None` off-Linux).
pub type CpuReader = std::sync::Arc<dyn Fn() -> Option<f64> + Send + Sync>;

/// Per-reporter delivery configuration, resolved from the engine keys and
/// `with:` knobs.
pub struct DeliveryConfig {
    /// Batch seal interval (the reporter's own `interval_ms`).
    pub interval: Duration,
    /// Seal a batch once it holds this many items, even between ticks.
    pub batch_size: usize,
    /// Bound on queued snapshots AND sealed-but-undelivered batches.
    pub max_pending: usize,
    /// Retry policy for transient failures.
    pub retry: RetryPolicy,
    /// Bound on the synchronous final flush at run end.
    pub final_timeout: Duration,
    /// Optional CPU gate: `(limit_percent, reader)`. While the reader
    /// reports a busy CPU ≥ limit, delivery pauses.
    pub cpu_gate: Option<(f64, CpuReader)>,
    /// Per-run reporter counters; `None` falls back to the process-wide
    /// registry (legacy facade path).
    pub counters: Option<std::sync::Arc<ReporterCounters>>,
}

/// Resolved retry policy (parsed from `with.retry`).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
        }
    }
}

/// The bounded snapshot queue between the fan-out and one reporter's
/// delivery worker. Overflow drops the OLDEST snapshot and counts it.
pub struct SnapshotQueue {
    reporter: String,
    max_pending: usize,
    counters: Option<std::sync::Arc<ReporterCounters>>,
    inner: Mutex<QueueInner>,
    notify: Notify,
}

struct QueueInner {
    queue: VecDeque<MetricSnapshot>,
    closed: bool,
}

impl SnapshotQueue {
    pub fn new(reporter: String, max_pending: usize) -> Self {
        Self {
            reporter,
            max_pending: max_pending.max(1),
            counters: None,
            inner: Mutex::new(QueueInner {
                queue: VecDeque::new(),
                closed: false,
            }),
            notify: Notify::new(),
        }
    }

    /// Attach the per-run counter registry.
    pub fn with_counters(mut self, counters: std::sync::Arc<ReporterCounters>) -> Self {
        self.counters = Some(counters);
        self
    }

    /// Enqueue a snapshot; on overflow the OLDEST queued snapshot is dropped
    /// and counted (cumulative values recover on the next snapshot).
    pub fn push(&self, snap: MetricSnapshot) {
        let mut guard = self.inner.lock().unwrap();
        if guard.closed {
            return;
        }
        if guard.queue.len() >= self.max_pending {
            let dropped = guard.queue.pop_front().expect("len checked");
            note_dropped_snapshots_with(
                &self.counters,
                &self.reporter,
                std::iter::once(snap_id(&dropped)),
            );
        }
        guard.queue.push_back(snap);
        drop(guard);
        self.notify.notify_one();
    }

    /// No more snapshots: the worker drains and runs its final flush.
    pub fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.notify.notify_one();
    }

    fn pop(&self) -> Option<MetricSnapshot> {
        self.inner.lock().unwrap().queue.pop_front()
    }

    fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }

    fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().queue.is_empty()
    }

    /// Wait until a push/close notification arrives.
    async fn notified(&self) {
        self.notify.notified().await;
    }
}

/// A sealed batch awaiting delivery.
struct Batch<Item> {
    seq: u64,
    items: Vec<Item>,
    /// Identities of the snapshots the batch carries (idempotent drop
    /// accounting — a snapshot split across batches counts once).
    snapshot_ids: std::collections::BTreeSet<(String, i64)>,
    /// Delivery attempts so far (the initial try counts as 1).
    attempts: u32,
    /// Current backoff delay; doubles per failure up to the policy cap.
    backoff: Duration,
    /// Do not attempt delivery before this instant.
    not_before: Instant,
}

/// Spawn one reporter's delivery worker over its snapshot queue.
pub fn spawn<S: Sink>(
    sink: S,
    cfg: DeliveryConfig,
    queue: std::sync::Arc<SnapshotQueue>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(DeliveryWorker { sink, cfg, queue }.run())
}

/// Fan-out pump: forward every snapshot from the engine's channel to all
/// reporter queues, then close them (the workers run their final flush).
/// One snapshot pump feeds every streaming reporter; a slow reporter backs
/// up only its own queue.
///
/// With `counters` set, the reporter delivery counters
/// (`perfscale_report_dropped_series` / `_dropped_snapshots` /
/// `_errors`, family names without the `_total` suffix the exporters add)
/// are injected into every snapshot as `{reporter=…}`-labeled counter
/// series — each reporter's own backend receives them like any other metric.
pub fn spawn_fanout(
    mut rx: tokio::sync::mpsc::Receiver<MetricSnapshot>,
    queues: Vec<std::sync::Arc<SnapshotQueue>>,
    counters: Option<std::sync::Arc<ReporterCounters>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(mut snap) = rx.recv().await {
            if let Some(c) = &counters {
                snap.series.extend(counter_series(c));
            }
            for q in &queues {
                q.push(snap.clone());
            }
        }
        for q in &queues {
            q.close();
        }
    })
}

struct DeliveryWorker<S: Sink> {
    sink: S,
    cfg: DeliveryConfig,
    queue: std::sync::Arc<SnapshotQueue>,
}

impl<S: Sink> DeliveryWorker<S> {
    async fn run(mut self) {
        let reporter = self.sink.reporter().to_string();
        let mut ticker = tokio::time::interval(self.cfg.interval);
        ticker.tick().await; // consume the immediate first tick
        let mut open: Vec<S::Item> = Vec::new();
        // (snapshot id, items) pairs for the open batch, for idempotent drop
        // accounting.
        let mut open_snaps: VecDeque<((String, i64), u64)> = VecDeque::new();
        let mut pending: VecDeque<Batch<S::Item>> = VecDeque::new();
        let mut seq: u64 = 0;
        // The latest snapshot seen — the terminal cumulative state the final
        // flush delivers.
        let mut last_snap: Option<MetricSnapshot> = None;

        /// Drain the queue into the open batch, sealing chunks of at most
        /// `batch_size` items (order preserved).
        macro_rules! absorb {
            () => {
                while let Some(snap) = self.queue.pop() {
                    let items = self.sink.encode(&snap);
                    open_snaps.push_back((snap_id(&snap), items.len() as u64));
                    open.extend(items);
                    last_snap = Some(snap);
                    while open.len() >= self.cfg.batch_size.max(1) {
                        let (items, snapshots) =
                            seal_prefix(&mut open, &mut open_snaps, self.cfg.batch_size.max(1));
                        seq += 1;
                        self.seal(
                            &mut pending,
                            Batch::new(seq, items, snapshots, self.cfg.retry.backoff_min),
                        );
                    }
                }
            };
        }

        loop {
            absorb!();

            let due = if self.cpu_gated() {
                None
            } else {
                pending.front().map(|b| b.not_before)
            };

            tokio::select! {
                _ = self.queue.notified() => {}
                _ = ticker.tick() => {
                    if !open.is_empty() {
                        let ids = open_snaps.iter().map(|(id, _)| id.clone()).collect();
                        seq += 1;
                        self.seal(&mut pending, Batch::new(seq, std::mem::take(&mut open), ids, self.cfg.retry.backoff_min));
                        open_snaps.clear();
                    }
                }
                _ = async {
                    match due {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending().await,
                    }
                } => {}
            }

            // After a notification, drain before delivering so a bursting
            // pump fills batches promptly.
            absorb!();

            if !self.cpu_gated()
                && pending
                    .front()
                    .is_some_and(|b| b.not_before <= Instant::now())
            {
                let mut batch = pending.pop_front().expect("front checked above");
                batch.attempts += 1;
                match self.sink.send(&batch.items, batch.seq).await {
                    PostOutcome::Delivered => {}
                    PostOutcome::Retry { after, reason } => {
                        if batch.attempts >= self.cfg.retry.max_attempts {
                            inc_with(&self.cfg.counters, REPORT_ERRORS_METRIC, &reporter, 1);
                            note_dropped_snapshots_with(
                                &self.cfg.counters,
                                &reporter,
                                batch.snapshot_ids.iter().cloned(),
                            );
                            tracing::warn!(
                                reporter = reporter,
                                seq = batch.seq,
                                attempts = batch.attempts,
                                "metrics batch exhausted retries — dropping ({reason})"
                            );
                        } else {
                            let delay = after.unwrap_or(batch.backoff);
                            batch.not_before = Instant::now() + delay;
                            batch.backoff = (batch.backoff * 2).min(self.cfg.retry.backoff_max);
                            pending.push_front(batch);
                        }
                    }
                    PostOutcome::Poison(reason) => {
                        inc_with(&self.cfg.counters, REPORT_ERRORS_METRIC, &reporter, 1);
                        note_dropped_snapshots_with(
                            &self.cfg.counters,
                            &reporter,
                            batch.snapshot_ids.iter().cloned(),
                        );
                        tracing::warn!(
                            reporter = reporter,
                            seq = batch.seq,
                            "dropping undeliverable metrics batch ({reason})"
                        );
                    }
                }
            }

            if self.queue.is_closed() && self.queue.is_empty() {
                break;
            }
        }

        self.final_flush(last_snap, pending, open_snaps).await;
    }

    /// Push a sealed batch onto the bounded backlog; overflow drops the
    /// OLDEST batch and counts it.
    fn seal(&self, pending: &mut VecDeque<Batch<S::Item>>, batch: Batch<S::Item>) {
        if pending.len() >= self.cfg.max_pending {
            let dropped = pending.pop_front().expect("len checked");
            note_dropped_snapshots_with(
                &self.cfg.counters,
                self.sink.reporter(),
                dropped.snapshot_ids.iter().cloned(),
            );
            tracing::warn!(
                reporter = self.sink.reporter(),
                seq = dropped.seq,
                max_pending = self.cfg.max_pending,
                "metrics backlog full — dropped the oldest batch"
            );
        }
        pending.push_back(batch);
    }

    fn cpu_gated(&self) -> bool {
        match &self.cfg.cpu_gate {
            Some((limit, reader)) if *limit > 0.0 => reader().is_some_and(|cpu| cpu >= *limit),
            _ => false,
        }
    }

    /// Coalescing final flush at run end. Every queued or open batch is a
    /// strict cumulative prefix of the latest snapshot, and sending older
    /// values after the terminal ones would be wrong — so the backlog and
    /// the open batch are discarded (counted as dropped snapshots) and only
    /// the terminal cumulative snapshot is encoded and delivered, chunked
    /// into `batch_size` batches, in order, with the configured retry
    /// policy, bounded by `final_timeout`. A failure warns and never fails
    /// the run; the CPU gate does not apply (the VUs have stopped).
    async fn final_flush(
        &mut self,
        last_snap: Option<MetricSnapshot>,
        pending: VecDeque<Batch<S::Item>>,
        open_snaps: VecDeque<((String, i64), u64)>,
    ) {
        let reporter = self.sink.reporter().to_string();
        // The coalesced-away backlog: every distinct snapshot it held counts
        // once, even when a snapshot was split across several batches.
        let discarded: std::collections::BTreeSet<(String, i64)> = pending
            .iter()
            .flat_map(|b| b.snapshot_ids.iter().cloned())
            .chain(open_snaps.iter().map(|(id, _)| id.clone()))
            .collect();
        if !discarded.is_empty() {
            note_dropped_snapshots_with(&self.cfg.counters, &reporter, discarded.iter().cloned());
        }
        let Some(mut snap) = last_snap else {
            return;
        };
        // Refresh the injected reporter counters: drops/errors since the
        // fan-out last forwarded must be visible in the terminal export.
        if let Some(c) = &self.cfg.counters {
            snap.series
                .retain(|s| !REPORTER_COUNTER_FAMILIES.contains(&s.name.as_str()));
            snap.series.extend(counter_series(c));
        }
        let items = self.sink.encode(&snap);
        if items.is_empty() {
            return;
        }
        let batch_size = self.cfg.batch_size.max(1);
        let retry = self.cfg.retry;
        let mut seq = u64::MAX - items.len().div_ceil(batch_size) as u64 - 1;
        let flush = async {
            for chunk in items.chunks(batch_size) {
                seq += 1;
                let mut attempts = 0u32;
                let mut backoff = retry.backoff_min;
                loop {
                    attempts += 1;
                    match self.sink.send(chunk, seq).await {
                        PostOutcome::Delivered => break,
                        PostOutcome::Poison(reason) => {
                            inc_with(&self.cfg.counters, REPORT_ERRORS_METRIC, &reporter, 1);
                            tracing::warn!(
                                reporter = reporter,
                                seq,
                                "dropping undeliverable terminal metrics batch ({reason})"
                            );
                            break;
                        }
                        PostOutcome::Retry { after, reason } => {
                            if attempts >= retry.max_attempts {
                                inc_with(&self.cfg.counters, REPORT_ERRORS_METRIC, &reporter, 1);
                                tracing::warn!(
                                    reporter = reporter,
                                    seq,
                                    "metrics target unreachable during final flush — the remaining terminal batches were dropped ({reason})"
                                );
                                return;
                            }
                            tokio::time::sleep(after.unwrap_or(backoff)).await;
                            backoff = (backoff * 2).min(retry.backoff_max);
                        }
                    }
                }
            }
        };
        let final_timeout = self.cfg.final_timeout;
        if tokio::time::timeout(final_timeout, flush).await.is_err() {
            tracing::warn!(
                reporter = reporter,
                timeout_ms = self.cfg.final_timeout.as_millis(),
                "final metrics flush timed out — the terminal batch was dropped"
            );
        }
    }
}

impl<Item> Batch<Item> {
    fn new(
        seq: u64,
        items: Vec<Item>,
        snapshot_ids: std::collections::BTreeSet<(String, i64)>,
        backoff_min: Duration,
    ) -> Self {
        Self {
            seq,
            items,
            snapshot_ids,
            attempts: 0,
            backoff: backoff_min,
            not_before: Instant::now(),
        }
    }
}

/// Split the open batch's first `n` items into a sealed batch, preserving
/// order; returns the items and the identities of the snapshots they came
/// from (a snapshot split across two batches appears in both — the counter
/// registry deduplicates it).
fn seal_prefix<Item>(
    open: &mut Vec<Item>,
    open_snaps: &mut VecDeque<((String, i64), u64)>,
    n: usize,
) -> (Vec<Item>, std::collections::BTreeSet<(String, i64)>) {
    let items: Vec<Item> = open.drain(..n).collect();
    let mut remaining = n as u64;
    let mut ids = std::collections::BTreeSet::new();
    while remaining > 0 {
        let Some((id, front)) = open_snaps.front_mut() else {
            break;
        };
        ids.insert(id.clone());
        if *front <= remaining {
            remaining -= *front;
            open_snaps.pop_front();
        } else {
            *front -= remaining;
        }
    }
    (items, ids)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use serde_json::{Map, Value};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::report::serve::ServeSink;
    use crate::step::thresholds::{MetricAgg, MetricKind};

    fn test_cfg() -> DeliveryConfig {
        DeliveryConfig {
            interval: Duration::from_millis(200),
            batch_size: 500,
            max_pending: 24,
            retry: RetryPolicy {
                max_attempts: 5,
                backoff_min: Duration::from_millis(100),
                backoff_max: Duration::from_millis(500),
            },
            final_timeout: Duration::from_secs(5),
            cpu_gate: None,
            counters: None,
        }
    }

    fn sample_snapshot(ts_ms: i64, count: f64) -> MetricSnapshot {
        let mut metrics = BTreeMap::new();
        metrics.insert(
            "http_req_duration".to_string(),
            MetricAgg {
                kind: MetricKind::Sample,
                avg: 10.0,
                min: 1.0,
                max: 50.0,
                p50: 8.0,
                p90: 20.0,
                p95: 30.0,
                p99: 40.0,
                count,
                rate: 0.0,
            },
        );
        MetricSnapshot {
            ts_ms,
            metrics,
            iterations_completed: 7,
            vus: 1,
            gpu: Vec::new(),
            run_id: String::new(),
            started_ms: 0,
            labels: Map::new(),
            series: Vec::new(),
            terminal: false,
        }
    }

    /// `id` isolates the test's counters: the registry is process-wide and
    /// tests run in parallel.
    fn spawn_serve(
        server: &MockServer,
        cfg: DeliveryConfig,
        id: &str,
    ) -> (Arc<SnapshotQueue>, tokio::task::JoinHandle<()>) {
        let queue = Arc::new(SnapshotQueue::new(id.to_string(), cfg.max_pending));
        let sink =
            ServeSink::new(server.uri(), Map::new(), None, None, usize::MAX).with_reporter_id(id);
        let worker = spawn(sink, cfg, Arc::clone(&queue));
        (queue, worker)
    }

    /// Poll the server's recorded requests until `n` arrive or time out.
    async fn wait_requests(server: &MockServer, n: usize, timeout: Duration) -> Vec<Request> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let reqs = server.received_requests().await.unwrap_or_default();
            if reqs.len() >= n || std::time::Instant::now() >= deadline {
                return reqs;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn bodies(reqs: &[Request]) -> Vec<serde_json::Value> {
        reqs.iter()
            .map(|r| serde_json::from_slice(&r.body).expect("batch body is JSON"))
            .collect()
    }

    #[tokio::test]
    async fn seals_a_batch_at_batch_size() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // One snapshot flattens to 8 samples; batch_size 8 → one batch per
        // snapshot.
        let cfg = DeliveryConfig {
            batch_size: 8,
            ..test_cfg()
        };
        let (queue, _worker) = spawn_serve(&server, cfg, "test/seal");
        queue.push(sample_snapshot(1000, 4.0));
        queue.push(sample_snapshot(2000, 9.0));

        let reqs = wait_requests(&server, 2, Duration::from_secs(3)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 2, "one POST per full batch");
        assert_eq!(bodies[0]["seq"], 1);
        assert_eq!(bodies[1]["seq"], 2);
        assert_eq!(bodies[0]["samples"].as_array().unwrap().len(), 8);
    }

    #[tokio::test]
    async fn flushes_open_batch_on_interval_tick() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let (queue, _worker) = spawn_serve(&server, test_cfg(), "test/tick");
        queue.push(sample_snapshot(1000, 4.0));

        let reqs = wait_requests(&server, 1, Duration::from_secs(3)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 1, "tick flushed the open batch");
        assert_eq!(bodies[0]["seq"], 1);
        assert_eq!(bodies[0]["samples"].as_array().unwrap().len(), 8);
        assert!(bodies[0].get("taskId").is_none());
    }

    #[tokio::test]
    async fn queue_overflow_drops_oldest_and_counts() {
        // Never deliver: a snapshot queue of 3 overflows on the 4th push,
        // dropping the OLDEST snapshot; the counter moves.
        let queue = Arc::new(SnapshotQueue::new("test/overflow".to_string(), 3));
        let before = counter_value(DROPPED_SNAPSHOTS_METRIC, "test/overflow");
        for i in 0..5 {
            queue.push(sample_snapshot(i * 1000, i as f64));
        }
        assert_eq!(queue.inner.lock().unwrap().queue.len(), 3);
        let oldest = queue.inner.lock().unwrap().queue[0].ts_ms;
        assert_eq!(oldest, 2000, "the two oldest snapshots were dropped");
        assert_eq!(
            counter_value(DROPPED_SNAPSHOTS_METRIC, "test/overflow") - before,
            2
        );
    }

    #[tokio::test]
    async fn retries_429_and_5xx_with_backoff_and_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let cfg = DeliveryConfig {
            batch_size: 500,
            ..test_cfg()
        };
        let (queue, _worker) = spawn_serve(&server, cfg, "test/retry429");
        queue.push(sample_snapshot(1000, 4.0));

        let reqs = wait_requests(&server, 2, Duration::from_secs(4)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 2, "429 → retry");
        assert_eq!(bodies[0]["seq"], 1);
        assert_eq!(bodies[1]["seq"], 1, "same seq across retries");
    }

    #[tokio::test]
    async fn poison_4xx_is_dropped_and_counted_without_retry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;

        let cfg = DeliveryConfig {
            batch_size: 500,
            ..test_cfg()
        };
        let reporter = "test/poison";
        let before = counter_value(REPORT_ERRORS_METRIC, reporter);
        let (queue, worker) = spawn_serve(&server, cfg, reporter);
        queue.push(sample_snapshot(1000, 4.0));
        // Wait for the mid-run attempt, then close: the terminal snapshot's
        // flush attempt follows. Both are poisoned, neither retried.
        wait_requests(&server, 1, Duration::from_secs(3)).await;
        queue.close();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker exits after the final flush")
            .unwrap();

        let reqs = server.received_requests().await.unwrap_or_default();
        assert_eq!(
            reqs.len(),
            2,
            "mid-run + terminal, both dropped without retry"
        );
        assert_eq!(counter_value(REPORT_ERRORS_METRIC, reporter) - before, 2);
    }

    #[tokio::test]
    async fn exhausted_retries_drop_and_count_the_batch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let cfg = DeliveryConfig {
            batch_size: 500,
            retry: RetryPolicy {
                max_attempts: 3,
                backoff_min: Duration::from_millis(50),
                backoff_max: Duration::from_millis(100),
            },
            ..test_cfg()
        };
        let reporter = "test/exhausted";
        let before = counter_value(REPORT_ERRORS_METRIC, reporter);
        let (queue, worker) = spawn_serve(&server, cfg, reporter);
        queue.push(sample_snapshot(1000, 4.0));

        let reqs = wait_requests(&server, 3, Duration::from_secs(5)).await;
        assert_eq!(reqs.len(), 3, "initial try + 2 retries, then dropped");
        assert_eq!(counter_value(REPORT_ERRORS_METRIC, reporter) - before, 1);

        // The final flush re-encodes the terminal snapshot and retries it —
        // another max_attempts attempts, then the run still exits.
        queue.close();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker exits after the final flush")
            .unwrap();
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            6,
            "the terminal batch gets its own bounded attempts"
        );
        assert_eq!(counter_value(REPORT_ERRORS_METRIC, reporter) - before, 2);
    }

    #[tokio::test]
    async fn final_flush_delivers_the_terminal_snapshot() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // batch_size above the total: only the close can flush.
        let (queue, worker) = spawn_serve(&server, test_cfg(), "test/finalflush");
        queue.push(sample_snapshot(1000, 4.0));
        let mut terminal = sample_snapshot(2000, 9.0);
        terminal.terminal = true;
        queue.push(terminal);
        queue.close();

        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker exits after the final flush")
            .unwrap();
        let reqs = server.received_requests().await.unwrap_or_default();
        let bodies = bodies(&reqs);
        // The backlog coalesces to the terminal snapshot: exactly one batch,
        // carrying the final cumulative values (count=9) — the older
        // snapshot (count=4) is never sent.
        assert_eq!(
            bodies.len(),
            1,
            "only the terminal snapshot ships: {bodies:?}"
        );
        assert_eq!(bodies[0]["samples"].as_array().unwrap().len(), 8);
        let lat_count = bodies[0]["samples"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["metric"] == "http_req_duration_count")
            .unwrap();
        assert_eq!(lat_count["value"], 9.0);
    }

    #[tokio::test]
    async fn final_flush_is_bounded_by_final_timeout() {
        // An unroutable target: every connect stalls; the drain must end
        // within final_timeout (plus slack), never hanging the run.
        let cfg = DeliveryConfig {
            batch_size: 1,
            retry: RetryPolicy {
                max_attempts: 100,
                backoff_min: Duration::from_secs(60),
                backoff_max: Duration::from_secs(60),
            },
            final_timeout: Duration::from_millis(500),
            ..test_cfg()
        };
        let queue = Arc::new(SnapshotQueue::new("test/timeout".to_string(), 8));
        // Port 1 is reserved and closed: connects fail fast, then the 60s
        // backoff would park the drain without the timeout.
        let sink = ServeSink::new(
            "http://127.0.0.1:1".to_string(),
            Map::new(),
            None,
            None,
            usize::MAX,
        )
        .with_reporter_id("test/timeout");
        let worker = spawn(sink, cfg, Arc::clone(&queue));
        queue.push(sample_snapshot(1000, 4.0));
        queue.close();
        let start = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker exits — the final timeout bounds the drain")
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "final flush ended promptly: {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn fan_out_isolates_a_dead_reporter_from_a_live_one() {
        let live = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&live)
            .await;

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let live_queue = Arc::new(SnapshotQueue::new("test/live".to_string(), 24));
        let dead_queue = Arc::new(SnapshotQueue::new("test/dead".to_string(), 24));

        let live_sink = ServeSink::new(live.uri(), Map::new(), None, None, usize::MAX);
        let dead_sink = ServeSink::new(
            "http://127.0.0.1:1".to_string(),
            Map::new(),
            None,
            None,
            usize::MAX,
        );
        let cfg = DeliveryConfig {
            batch_size: 500,
            ..test_cfg()
        };
        let live_worker = spawn(live_sink, cfg, Arc::clone(&live_queue));
        let dead_worker = spawn(
            dead_sink,
            DeliveryConfig {
                batch_size: 500,
                retry: RetryPolicy {
                    max_attempts: 2,
                    backoff_min: Duration::from_millis(50),
                    backoff_max: Duration::from_millis(100),
                },
                ..test_cfg()
            },
            Arc::clone(&dead_queue),
        );
        let fanout = spawn_fanout(rx, vec![live_queue, dead_queue], None);

        for i in 1..=3 {
            tx.send(sample_snapshot(i * 1000, i as f64)).await.unwrap();
        }
        drop(tx);

        // The live reporter receives batches while the dead one burns
        // retries: with batch_size 500 the three snapshots ride one tick
        // batch.
        let reqs = wait_requests(&live, 1, Duration::from_secs(5)).await;
        assert!(!reqs.is_empty(), "a dead reporter never blocks a live one");
        fanout.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), live_worker)
            .await
            .expect("live worker exits")
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), dead_worker).await;
        // The terminal cumulative snapshot (count=3) is the last thing the
        // live backend receives.
        let reqs = live.received_requests().await.unwrap_or_default();
        let last = bodies(&reqs).pop().unwrap();
        let lat_count = last["samples"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["metric"] == "http_req_duration_count")
            .unwrap();
        assert_eq!(lat_count["value"], 3.0);
    }

    #[tokio::test]
    async fn cpu_gate_pauses_delivery_and_the_final_flush_ignores_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let cpu = Arc::new(AtomicU64::new(95));
        let reader = {
            let cpu = Arc::clone(&cpu);
            move || Some(cpu.load(Ordering::Relaxed) as f64)
        };
        let cfg = DeliveryConfig {
            batch_size: 500,
            cpu_gate: Some((90.0, Arc::new(reader))),
            ..test_cfg()
        };
        let (queue, worker) = spawn_serve(&server, cfg, "test/cpugate");

        // Gated: the snapshot queues, nothing is POSTed.
        queue.push(sample_snapshot(1000, 4.0));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "gated worker must not POST"
        );

        // Recovered: the queued snapshot ships.
        cpu.store(10, Ordering::Relaxed);
        queue.push(sample_snapshot(2000, 9.0));
        let reqs = wait_requests(&server, 2, Duration::from_secs(3)).await;
        assert_eq!(reqs.len(), 2, "queued + fresh snapshot both ship");

        // Gate back on; the final flush ignores it (the VUs have stopped).
        cpu.store(95, Ordering::Relaxed);
        queue.push(sample_snapshot(3000, 12.0));
        queue.close();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker exits after the final flush")
            .unwrap();
        let reqs = server.received_requests().await.unwrap_or_default();
        assert_eq!(
            reqs.len(),
            3,
            "2 mid-run after the gate opens + the terminal flush despite the gate"
        );
        // The terminal cumulative values (count=12) are the last thing sent.
        let last = bodies(&reqs).pop().unwrap();
        let lat_count = last["samples"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["metric"] == "http_req_duration_count")
            .unwrap();
        assert_eq!(lat_count["value"], 12.0);
        let _ = Value::Null;
    }

    #[tokio::test]
    async fn oversized_snapshot_splits_into_bounded_batches() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // One snapshot with 3 metrics → 3×6 + 2 gauges = 20 samples;
        // batch_size 8 → batches of 8, 8, 4, in order.
        let mut snap = sample_snapshot(1000, 4.0);
        for (name, count) in [("b_lat", 6.0), ("c_lat", 2.0)] {
            snap.metrics.insert(
                name.to_string(),
                MetricAgg {
                    kind: MetricKind::Sample,
                    avg: 10.0,
                    min: 1.0,
                    max: 50.0,
                    p50: 8.0,
                    p90: 20.0,
                    p95: 30.0,
                    p99: 40.0,
                    count,
                    rate: 0.0,
                },
            );
        }
        let cfg = DeliveryConfig {
            batch_size: 8,
            ..test_cfg()
        };
        let (queue, _worker) = spawn_serve(&server, cfg, "test/split");
        queue.push(snap);

        let reqs = wait_requests(&server, 3, Duration::from_secs(3)).await;
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 3, "20 samples split into 8+8+4");
        let sizes: Vec<usize> = bodies
            .iter()
            .map(|b| b["samples"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, vec![8, 8, 4], "no batch exceeds batch_size");
        let seqs: Vec<u64> = bodies.iter().map(|b| b["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![1, 2, 3], "order preserved");
        // Order within: the first batch opens with the first metric.
        assert_eq!(bodies[0]["samples"][0]["metric"], "b_lat");
    }

    #[tokio::test]
    async fn dropped_backlog_batch_counts_its_snapshots() {
        // An unreachable target: batch 1 (2 snapshots) parks on a long
        // backoff; batch 2's seal overflows max_pending=1 and drops batch 1
        // — the counter must move by 2, not 1.
        let reporter = "test/dropcount";
        let counters = Arc::new(ReporterCounters::default());
        let queue = Arc::new(
            SnapshotQueue::new(reporter.to_string(), 24).with_counters(Arc::clone(&counters)),
        );
        let sink = ServeSink::new(
            "http://127.0.0.1:1".to_string(),
            Map::new(),
            None,
            None,
            usize::MAX,
        )
        .with_reporter_id(reporter);
        let cfg = DeliveryConfig {
            interval: Duration::from_millis(100),
            batch_size: 500,
            max_pending: 1,
            retry: RetryPolicy {
                max_attempts: 100,
                backoff_min: Duration::from_secs(60),
                backoff_max: Duration::from_secs(60),
            },
            final_timeout: Duration::from_millis(300),
            cpu_gate: None,
            counters: Some(Arc::clone(&counters)),
        };
        let worker = spawn(sink, cfg, Arc::clone(&queue));

        // Two snapshots before the first tick → one 2-snapshot batch.
        queue.push(sample_snapshot(1000, 1.0));
        queue.push(sample_snapshot(2000, 2.0));
        // Wait for batch 1's failed attempt (connection refused is fast).
        tokio::time::sleep(Duration::from_millis(400)).await;
        // Two more → batch 2; sealing it drops batch 1 (2 snapshots).
        queue.push(sample_snapshot(3000, 3.0));
        queue.push(sample_snapshot(4000, 4.0));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            counters.get(DROPPED_SNAPSHOTS_METRIC, reporter),
            2,
            "the dropped batch held 2 snapshots"
        );
        queue.close();
        let _ = tokio::time::timeout(Duration::from_secs(3), worker).await;
    }

    #[tokio::test]
    async fn final_flush_terminal_first_no_older_values_after() {
        // An older batch fails permanently (poison 400), then the target
        // recovers: the terminal snapshot must still be attempted, and no
        // older cumulative values may arrive after it.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/metrics"))
            .respond_with(ResponseTemplate::new(400))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let cfg = DeliveryConfig {
            interval: Duration::from_millis(100),
            batch_size: 500,
            ..test_cfg()
        };
        let (queue, worker) = spawn_serve(&server, cfg, "test/terminalorder");
        // Snapshot 1 (count=4) is delivered mid-run and poisoned (400).
        queue.push(sample_snapshot(1000, 4.0));
        let reqs = wait_requests(&server, 1, Duration::from_secs(3)).await;
        assert_eq!(reqs.len(), 1, "the older batch was attempted");
        // Snapshot 2 (count=9, terminal) arrives after the poison drop.
        let mut terminal = sample_snapshot(2000, 9.0);
        terminal.terminal = true;
        queue.push(terminal);
        queue.close();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker exits")
            .unwrap();

        let reqs = server.received_requests().await.unwrap_or_default();
        let bodies = bodies(&reqs);
        assert_eq!(bodies.len(), 2, "poisoned older batch + terminal batch");
        let last = bodies.last().unwrap();
        let lat_count = last["samples"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["metric"] == "http_req_duration_count")
            .unwrap();
        assert_eq!(lat_count["value"], 9.0, "the terminal values landed");
        let count_of = |b: &serde_json::Value| {
            b["samples"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["metric"] == "http_req_duration_count")
                .unwrap()["value"]
                .as_f64()
                .unwrap()
        };
        assert_eq!(count_of(bodies.first().unwrap()), 4.0, "the older batch");
        assert!(
            bodies.iter().skip(1).all(|b| count_of(b) == 9.0),
            "no older cumulative values after the terminal batch"
        );
    }

    #[test]
    fn per_run_counters_are_isolated() {
        let a = ReporterCounters::default();
        let b = ReporterCounters::default();
        a.inc(DROPPED_SERIES_METRIC, "r1", 3);
        b.inc(DROPPED_SERIES_METRIC, "r1", 7);
        b.inc(REPORT_ERRORS_METRIC, "r2", 1);
        assert_eq!(a.get(DROPPED_SERIES_METRIC, "r1"), 3);
        assert_eq!(b.get(DROPPED_SERIES_METRIC, "r1"), 7);
        assert_eq!(a.get(REPORT_ERRORS_METRIC, "r2"), 0);
        assert_eq!(a.snapshot().len(), 1);
        assert_eq!(b.snapshot().len(), 2);
    }

    #[tokio::test]
    async fn fanout_injects_counter_series_into_snapshots() {
        let counters = Arc::new(ReporterCounters::default());
        counters.inc(DROPPED_SNAPSHOTS_METRIC, "std/report/otel@v1", 5);
        let queue = Arc::new(SnapshotQueue::new("test/inject".to_string(), 8));
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let fanout = spawn_fanout(rx, vec![Arc::clone(&queue)], Some(Arc::clone(&counters)));
        tx.send(sample_snapshot(1000, 4.0)).await.unwrap();
        drop(tx);
        fanout.await.unwrap();
        let snap = queue.pop().expect("snapshot forwarded");
        let counter = snap
            .series
            .iter()
            .find(|s| s.name == "perfscale_report_dropped_snapshots")
            .expect("counter series injected (family name, no _total suffix)");
        assert_eq!(counter.agg.count, 5.0);
        assert_eq!(
            counter.labels.get("reporter").map(String::as_str),
            Some("std/report/otel@v1")
        );
    }

    #[tokio::test]
    async fn snapshot_split_across_batches_counts_once_when_dropped() {
        // A snapshot (run_id "r", ts 1000) with 20 samples splits into three
        // batches (8+8+4). With max_pending 1 behind a parked retry, batches
        // 1 and 2 are both dropped on overflow — the snapshot must count ONCE.
        let reporter = "test/splitdrop";
        let counters = Arc::new(ReporterCounters::default());
        let queue = Arc::new(
            SnapshotQueue::new(reporter.to_string(), 24).with_counters(Arc::clone(&counters)),
        );
        let sink = ServeSink::new(
            "http://127.0.0.1:1".to_string(),
            Map::new(),
            None,
            None,
            usize::MAX,
        )
        .with_reporter_id(reporter);
        let cfg = DeliveryConfig {
            interval: Duration::from_secs(60), // no tick during the test
            batch_size: 8,
            max_pending: 1,
            retry: RetryPolicy {
                max_attempts: 100,
                backoff_min: Duration::from_secs(60),
                backoff_max: Duration::from_secs(60),
            },
            final_timeout: Duration::from_millis(300),
            cpu_gate: None,
            counters: Some(Arc::clone(&counters)),
        };
        let worker = spawn(sink, cfg, Arc::clone(&queue));

        let mut snap = sample_snapshot(1000, 4.0);
        snap.run_id = "r-split".to_string();
        for name in ["b_lat", "c_lat"] {
            snap.metrics.insert(
                name.to_string(),
                MetricAgg {
                    kind: MetricKind::Sample,
                    avg: 10.0,
                    min: 1.0,
                    max: 50.0,
                    p50: 8.0,
                    p90: 20.0,
                    p95: 30.0,
                    p99: 40.0,
                    count: 2.0,
                    rate: 0.0,
                },
            );
        }
        queue.push(snap);
        // Wait until the head batch's attempt has failed (conn refused) and
        // the overflow drops have happened.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            counters.get(DROPPED_SNAPSHOTS_METRIC, reporter),
            1,
            "one snapshot dropped once, though two of its batches were dropped"
        );
        queue.close();
        let _ = tokio::time::timeout(Duration::from_secs(3), worker).await;
        // The final flush discards the parked third batch — still the same
        // snapshot, still counted once.
        assert_eq!(counters.get(DROPPED_SNAPSHOTS_METRIC, reporter), 1);
    }
}
