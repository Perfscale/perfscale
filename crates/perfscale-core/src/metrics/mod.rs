//! Metrics module: the run metrics store plus the family registry (RFC 010).
//!
//! - [`store`]: [`Metrics`], the per-run accumulator every step folds into
//!   (HDR histograms, counters, failure rates) and the k6-compatible
//!   summary/`[stats]` rendering.
//! - sharded: [`ShardedMetrics`], the internally sharded wrapper the
//!   runner actually shares across VUs; readers merge the shards on demand.
//! - [`registry`]: [`MetricKind`], [`MetricDef`], the [`MetricsFamily`]
//!   trait, and the process-global registry ([`register_family`],
//!   [`catalog`]). Core seeds the built-in `http` and `llm` families;
//!   downstream (pro) crates register theirs next to their
//!   `register_action` calls.
//!
//! The registry drives the catalog (`perfscale metrics`), fold-time kind
//! validation (a registered name emitted with the wrong JSON shape warns in
//! the run log), and family-owned presentation (families append their own
//! summary lines / `[stats]` extras after the generic fallback, which keeps
//! unregistered names byte-identical to the historical output).
//!
//! [`engine`] holds the optional self-observability probe
//! (`engine_metrics:`): scheduler lag and run-log channel depth, recorded
//! into the same store during the VU phase.

mod builtin;
pub(crate) mod engine;
mod registry;
mod sharded;
mod store;

pub use engine::EngineMetricsConfig;
pub use registry::{
    catalog, register_family, MetricDef, MetricKind, MetricsFamily, SamplerContext,
};
pub use sharded::ShardedMetrics;
pub use store::Metrics;

pub(crate) use registry::{families, kind_mismatch_warning};
