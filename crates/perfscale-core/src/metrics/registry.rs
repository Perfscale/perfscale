//! The metrics family registry: typed declarations of what a step family may
//! emit, registered process-globally like custom actions
//! ([`crate::step::actions::register_action`]) and GPU collectors
//! ([`crate::gpu::register_gpu_collector`]).
//!
//! The registry drives three things (RFC 010):
//!
//! - **Catalog**: [`catalog()`] lists every declared metric (family, name,
//!   kind, unit, description) — what `perfscale metrics` prints.
//! - **Validation**: the runner's fold of a step's `metrics` object warns
//!   when a registered name's JSON shape disagrees with its declared kind
//!   ([`kind_mismatch_warning`]). Unknown names pass through untouched.
//! - **Presentation**: [`Metrics::summary_lines`](super::Metrics) and
//!   [`Metrics::stats_line`](super::Metrics) ask every registered family for
//!   its own rendering after the generic fallback lines.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock, RwLock};

use super::store::Metrics;

/// How a metric aggregates — declared by the family, checked against the
/// JSON shape a step actually emits at fold time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// A monotonically summed scalar (JSON number), e.g. `http_reqs`.
    Counter,
    /// A point-in-time scalar, e.g. `perfscale_vus`.
    Gauge,
    /// Duration/rate samples emitted as a JSON array, aggregated into an HDR
    /// histogram, e.g. `http_req_duration`.
    Histogram,
    /// Per-invocation failed/total samples, e.g. `http_req_failed`.
    Rate,
}

impl MetricKind {
    /// Lowercase label for `perfscale metrics` output.
    pub fn as_str(&self) -> &'static str {
        match self {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Histogram => "histogram",
            MetricKind::Rate => "rate",
        }
    }
}

/// One declared metric: its name as emitted under the step output's reserved
/// `metrics` key, how it aggregates, its unit, and a one-line description.
#[derive(Debug, Clone, Copy)]
pub struct MetricDef {
    pub name: &'static str,
    pub kind: MetricKind,
    /// Display unit ("ms", "tok/s", "count", …) — informational, for the
    /// catalog; rendering keeps the historical shapes.
    pub unit: &'static str,
    pub description: &'static str,
}

/// Run-liveness context handed to [`MetricsFamily::start_samplers`] at the
/// start of the VU phase: the run's shared metrics handle (record into it
/// like any step would) and the soft-stop flag to poll.
pub struct SamplerContext {
    pub metrics: Arc<super::ShardedMetrics>,
    pub stop: Arc<AtomicBool>,
}

/// A family of related metrics, declared by the crate that owns the steps
/// emitting them. Core registers the built-in `http`, `llm` and `pubsub`
/// families;
/// downstream (pro) crates register theirs next to their `register_action`
/// calls.
///
/// All rendering hooks default to "nothing extra": the family's metrics then
/// render through the generic fallback shapes (counter / rate / histogram),
/// which is what unregistered metrics use.
pub trait MetricsFamily: Send + Sync {
    /// Family id, e.g. `"http"`, `"llm"`, `"pro/webrtc"`.
    fn family(&self) -> &'static str;

    /// Every metric this family may emit. Derived names (the runner's
    /// mechanical `<histogram>_failed` rates) are not listed.
    fn defs(&self) -> &'static [MetricDef];

    /// Start background samplers at the beginning of the VU phase — for
    /// metrics produced between steps (e.g. periodic WebRTC `getStats`
    /// polling). Default: no samplers.
    fn start_samplers(&self, _ctx: &SamplerContext) {}

    /// Extra fields appended to the periodic `[stats]` line (leading space
    /// included, e.g. `" llm_reqs=9 llm_tok_s=38.7"`). Default: none.
    fn stats_extras(&self, _store: &Metrics) -> String {
        String::new()
    }

    /// Family-owned summary lines, appended after the generic lines (this is
    /// where the `http` family renders the `http_req_*` triple). Default:
    /// none — the family's metrics appear only in the generic shapes.
    fn summary_lines(&self, _store: &Metrics, _wall_secs: f64) -> Vec<String> {
        Vec::new()
    }
}

fn registry() -> &'static RwLock<Vec<Arc<dyn MetricsFamily>>> {
    static REGISTRY: OnceLock<RwLock<Vec<Arc<dyn MetricsFamily>>>> = OnceLock::new();
    // Seeded with the built-in families, `http` first: summary rendering
    // appends family lines in registration order, and the `http_req_*` block
    // must keep its historical position.
    REGISTRY.get_or_init(|| {
        RwLock::new(vec![
            Arc::new(super::builtin::HttpFamily),
            Arc::new(super::builtin::LlmFamily),
            Arc::new(super::builtin::PubsubFamily),
            Arc::new(super::builtin::EngineFamily),
        ])
    })
}

/// Register a custom [`MetricsFamily`]. Typically called once at startup by a
/// downstream crate, next to its `register_action` calls. Families are
/// consulted in registration order (built-ins first).
pub fn register_family(family: Arc<dyn MetricsFamily>) {
    registry().write().unwrap().push(family);
}

/// The registered families, in registration order (built-ins first).
pub(crate) fn families() -> Vec<Arc<dyn MetricsFamily>> {
    registry().read().unwrap().clone()
}

/// Every declared metric as `(family, def)` pairs, in registration order —
/// what `perfscale metrics` prints.
pub fn catalog() -> Vec<(&'static str, MetricDef)> {
    let mut out = Vec::new();
    for f in families() {
        for def in f.defs() {
            out.push((f.family(), *def));
        }
    }
    out
}

/// Look up a metric's declaration by name across all registered families.
pub(crate) fn lookup(name: &str) -> Option<MetricDef> {
    families()
        .iter()
        .flat_map(|f| f.defs().iter().copied())
        .find(|d| d.name == name)
}

/// Fold-time kind validation (RFC 010): when `name` is registered and the
/// emitted JSON shape disagrees with the declared kind — a scalar for a
/// `Histogram`, an array for a scalar kind — return the warning text for the
/// run log. Unknown names and matching shapes return `None`.
pub(crate) fn kind_mismatch_warning(name: &str, value_is_array: bool) -> Option<String> {
    let def = lookup(name)?;
    let mismatch = match def.kind {
        MetricKind::Histogram => !value_is_array,
        MetricKind::Counter | MetricKind::Gauge | MetricKind::Rate => value_is_array,
    };
    if !mismatch {
        return None;
    }
    let actual = if value_is_array { "array" } else { "scalar" };
    Some(format!(
        "metric '{name}' is declared as {} (family '{}') but was emitted as {actual} — check the emitting step",
        def.kind.as_str(),
        lookup_family(name).unwrap_or("?"),
    ))
}

fn lookup_family(name: &str) -> Option<&'static str> {
    families()
        .iter()
        .find(|f| f.defs().iter().any(|d| d.name == name))
        .map(|f| f.family())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFamily;

    impl MetricsFamily for TestFamily {
        fn family(&self) -> &'static str {
            "test/rfc010"
        }
        fn defs(&self) -> &'static [MetricDef] {
            &[
                MetricDef {
                    name: "test010_counter",
                    kind: MetricKind::Counter,
                    unit: "count",
                    description: "registry test counter",
                },
                MetricDef {
                    name: "test010_hist",
                    kind: MetricKind::Histogram,
                    unit: "ms",
                    description: "registry test histogram",
                },
            ]
        }
    }

    /// register → catalog round-trip, and the built-in families are seeded.
    #[test]
    fn register_family_appears_in_catalog_alongside_builtins() {
        register_family(Arc::new(TestFamily));

        let names: Vec<&str> = catalog().iter().map(|(_, d)| d.name).collect();
        assert!(names.contains(&"test010_counter"), "{names:?}");
        assert!(names.contains(&"test010_hist"), "{names:?}");
        // Built-ins are seeded before any registration.
        assert!(names.contains(&"http_req_duration"), "{names:?}");
        assert!(names.contains(&"llm_ttft_ms"), "{names:?}");

        let (family, def) = catalog()
            .into_iter()
            .find(|(_, d)| d.name == "test010_hist")
            .unwrap();
        assert_eq!(family, "test/rfc010");
        assert_eq!(def.kind, MetricKind::Histogram);
        assert_eq!(def.unit, "ms");
    }

    /// Kind validation: registered name + wrong shape warns; right shape and
    /// unknown names pass through silently.
    #[test]
    fn kind_mismatch_warning_flags_shape_disagreement() {
        register_family(Arc::new(TestFamily));

        let w = kind_mismatch_warning("test010_hist", false).expect("scalar histogram warns");
        assert!(w.contains("test010_hist"), "{w}");
        assert!(w.contains("histogram"), "{w}");
        assert!(w.contains("test/rfc010"), "{w}");

        let w = kind_mismatch_warning("test010_counter", true).expect("array counter warns");
        assert!(w.contains("counter"), "{w}");

        assert!(kind_mismatch_warning("test010_hist", true).is_none());
        assert!(kind_mismatch_warning("test010_counter", false).is_none());
        assert!(kind_mismatch_warning("never_registered_metric", false).is_none());
        assert!(kind_mismatch_warning("never_registered_metric", true).is_none());
    }

    /// Default rendering hooks add nothing: a registered family without
    /// overrides must not perturb `stats_line` or the summary.
    #[test]
    fn default_family_rendering_is_silent() {
        register_family(Arc::new(TestFamily));

        let mut m = Metrics::default();
        m.add_counters(
            serde_json::json!({ "test010_counter": 3.0, "test010_hist": [10.0] })
                .as_object()
                .unwrap(),
        );
        let lines = m.summary_lines(1.0, 1, 1);
        // Generic fallback shapes only — no family-appended lines.
        assert!(lines.iter().any(|l| l.starts_with("test010_counter: 3 ")));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("test010_hist: avg=") && l.contains("count=1")));
        assert_eq!(lines.len(), 4, "vus + iterations + 2 generic: {lines:?}");

        let stats = m.stats_line(1, 0, 5.0, 1);
        assert_eq!(stats, "[stats] ts=1 rps=0.00 reqs=0 iters=1");
    }
}
