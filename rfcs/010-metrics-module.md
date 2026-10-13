# RFC 010: Metrics module — families, registry, catalog

- **Status**: Draft
- **Author**: Perfscale Team
- **Created**: 2026-10-13
- **Requires**: none (informs RFC 007 webrtc metrics, RFC 008 pro families)
- **Required by**: none

## Summary

Turn metrics from an implicit convention — action authors return a reserved
`metrics` object and hope the names line up with the docs — into a **module
with a registry**: `MetricKind` / `MetricDef` declarations grouped into
`MetricsFamily` implementations, registered process-globally like custom
actions (`register_action`) and GPU collectors (`register_gpu_collector`).
The registry drives three things: a **catalog** (`perfscale metrics` prints
every known metric with its kind, unit, and description), **fold-time kind
validation** (a step emitting a scalar where the family declared a histogram
gets a warning in the run log instead of silent misaggregation), and
**family-owned presentation** (the `[stats]` line and the end-of-run summary
ask families for their rendering instead of hardcoding metric names in the
runner). The built-in `http` and `llm` families ship in core; pro families
(webrtc, fix, soap, pubsub, …) register theirs from their own crates.

## Motivation

- **The metric contract is folklore.** Which names exist, which are
  counters vs histograms, what units they use — all of that lives in doc
  comments and `docs/core/metrics.md`. The controlplane parses `[stats]`
  lines and summary blocks with hardcoded keys; a pro crate adding a metric
  has no way to make it discoverable.
- **Mistakes are silent.** The fold in `execute_step` treats a JSON number as
  a counter and an array as histogram samples purely by shape. An action
  that emits `db_query_duration: 12.3` (scalar instead of array) quietly
  creates a *counter* named like a histogram; the summary shows it in the
  wrong shape and threshold gates misread it. Nobody finds out.
- **The runner hardcodes families.** `Metrics::summary_lines` special-cases
  the `http_req_*` triple; `stats_line` peeks at `llm_tokens_per_sec` by
  literal name to append `llm_reqs`/`llm_tok_s`. Every new family that wants
  a live `[stats]` signal needs a runner patch — impossible for pro crates.
- **Background samplers have no seam.** RFC 007's webrtc stats collection
  (`getStats` polling that outlives a single step) currently pokes the
  shared store through `Context::run_metrics` from inside an action. A
  family that starts its own sampler task at run start is the natural shape,
  and the registry is where that hook belongs.

## Goals

- A `metrics` module in `perfscale-core` owning the run metrics store and
  the family registry: `MetricKind`, `MetricDef`, trait `MetricsFamily`,
  `register_family`, `catalog()`.
- Built-in `http` and `llm` families registered by default, declaring every
  metric those steps emit (plus the run-progress gauges).
- Presentation driven by families: `stats_line` and the end-of-run summary
  consult the registry, with a generic fallback for unregistered names that
  reproduces today's output **byte-for-byte**.
- Kind validation at fold time: a registered name whose JSON shape
  (scalar vs array) disagrees with its declared kind logs a warning into the
  run log. Unknown names pass through untouched.
- `perfscale metrics` CLI: print the catalog as a table.
- Golden tests pinning the exact `[stats]` and summary bytes for http and
  llm runs, so external parsers (controlplane `parse_stats_lines`,
  `crate::summary`) cannot be broken silently.

## Non-goals (this RFC)

- **Changing any output byte.** The summary format, `[stats]` keys, and
  during-run sample naming (Prometheus conventions in
  `report::stream::snapshot_to_samples`) stay exactly as they are. This RFC
  moves *who renders*, not *what is rendered*.
- **WASM guest metrics.** Libraries (RFC 005) already report through
  `LibraryMetrics`; letting a WASM component register or emit into metric
  families is future work (it needs the custom-SDK-steps contract of RFC 009
  first).
- **The controlplane metric catalog UI.** The registry is in-process; a
  controlplane endpoint or UI listing known metrics is future work on top of
  `perfscale metrics`.
- Per-metric cardinality labels, exemplars, or OpenTelemetry export.

## Detailed design

### Module layout

New module `crates/perfscale-core/src/metrics/`:

- `store.rs` — the run metrics accumulator (`Metrics`), moved unchanged from
  `step::runner`. `step::runner` re-exports it, so existing paths
  (`crate::step::runner::Metrics`, the benches) keep compiling.
- `registry.rs` — `MetricKind`, `MetricDef`, trait `MetricsFamily`, the
  process-global registry, `register_family`, `catalog()`, and the
  kind-mismatch check used at fold time.
- `builtin.rs` — the built-in `http` and `llm` families.

### Types

```rust
pub enum MetricKind { Counter, Gauge, Histogram, Rate }

pub struct MetricDef {
    pub name: &'static str,
    pub kind: MetricKind,
    pub unit: &'static str,        // "ms", "tok/s", "count", …
    pub description: &'static str,
}

pub trait MetricsFamily: Send + Sync {
    /// Family id, e.g. "http", "llm", "pro/webrtc".
    fn family(&self) -> &'static str;
    /// Every metric this family may emit.
    fn defs(&self) -> &'static [MetricDef];

    /// Start background samplers at run start. Default: none.
    fn start_samplers(&self, ctx: &SamplerContext) {}

    /// Extra fields appended to the periodic `[stats]` line. Default: none.
    fn stats_extras(&self, store: &Metrics) -> String { String::new() }

    /// Family-owned summary lines, appended after the generic lines.
    /// Default: none — the family's metrics render through the generic
    /// fallback (counter/rate/histogram shapes), which is what unregistered
    /// metrics use today.
    fn summary_lines(&self, store: &Metrics, wall_secs: f64) -> Vec<String> {
        Vec::new()
    }
}
```

The registry is a `OnceLock<RwLock<Vec<Arc<dyn MetricsFamily>>>>` seeded with
the built-in `http` and `llm` families — the exact pattern
`register_gpu_collector` already uses. `catalog()` returns
`(family, MetricDef)` pairs in registration order for the CLI and for
fold-time lookup.

### The store move

`Metrics` (durations histogram, counters, hists, rates, threshold results)
moves from `step/runner.rs` to `metrics/store.rs` without API changes; the
runner keeps working through a re-export. Summary rendering changes
internally only:

- The generic loops over counters / rates / histograms stay exactly as they
  are — that **is** the fallback rendering, and it is what keeps output for
  unregistered names byte-identical.
- The hardcoded `http_req_*` block becomes the `http` family's
  `summary_lines` implementation; the store asks every registered family for
  its lines after the generic loops. The `http` family is registered first,
  so the block lands in the same position.
- `stats_line`'s literal `llm_tokens_per_sec` peek becomes the `llm`
  family's `stats_extras` (`llm_reqs=N llm_tok_s=X`); the store concatenates
  every family's extras. Built-ins only → identical bytes.

Golden tests (written against the pre-refactor output) pin the exact
`[stats]` line and summary block for an http run, an llm run, and a mixed
run.

### Kind validation at fold time

In `execute_step`'s fold of the reserved `metrics` object: when a name **is**
in the registry and the value's shape disagrees with the declared kind —
scalar for `Histogram`, array for `Counter`/`Gauge`/`Rate` — a warning is
emitted into the run log:

```text
metric 'db_query_duration' declared as histogram but folded as counter (scalar value) — check the emitting step
```

It is a warning, not an error: the value still folds as today (unknown-name
behavior), so a buggy step degrades its own metric instead of failing the
run. Unknown names pass through exactly as today.

### `start_samplers` — the background-metric seam

`run_native` calls `start_samplers` on every registered family at the start
of the VU phase with a `SamplerContext` (the run's shared metrics handle and
the stop flag). Phase 1 ships no built-in sampler; the seam exists so RFC
007's webrtc `getStats` polling (today: an action reaching into
`Context::run_metrics`) becomes the `pro/webrtc` family's sampler, and any
pro family can produce metrics between steps without holding a step open.

### Built-in families

- **http**: `http_req_duration` (Histogram, ms), `http_req_failed` (Rate),
  `http_reqs` (Counter), plus the run-progress gauges
  `perfscale_iterations_completed` and `perfscale_vus` (Gauge) that ride
  every during-run snapshot.
- **llm**: `llm_ttft_ms` (Histogram, ms), `llm_tokens_per_sec` (Histogram,
  tok/s), `llm_prompt_tokens` / `llm_completion_tokens` / `llm_chunks`
  (Counters) — everything `std/llm@v1` emits. The `std/llm` step lives in
  core, so its defs live in core too.

### CLI

`perfscale metrics` prints the catalog — family, name, kind, unit,
description — as a plain padded table (no new dependencies), so step authors
and controlplane tooling can see every metric a build knows about,
registered pro families included.

### Pro families (Phase 2)

Pro crates (`pro/webrtc`, `pro/fix`, `pro/soap`, `pro/llm`, `pro/pubsub`)
call `register_family` at startup next to their existing `register_action`
calls — same process-global pattern, no core changes. The webrtc family's
`start_samplers` is the migration target for the current
`Context::run_metrics` stats polling. perfscaled registers the families its
build includes.

## Benefits

- Metric names, kinds, and units become machine-checkable declarations
  instead of documentation folklore.
- Shape mistakes surface as run-log warnings at the first offending sample.
- Pro families get the same presentation and catalog reach as built-ins
  without patching the runner.
- Golden tests make the external-parse contract (summary bytes, `[stats]`
  keys) enforceable in CI.

## Drawbacks

- A second global registry (next to actions and GPU collectors) — accepted:
  the pattern is established, and metric families are exactly as
  process-global as the actions that emit them.
- Families that want custom summary rendering must implement it in Rust;
  there is no declarative format override. Deliberate: the k6-compatible
  grammar is the contract, and free-form rendering would fragment it.

## Tradeoffs

- **Store-driven ordering, not family-driven.** The summary's generic
  section iterates the store's sorted maps (as today), not the registry;
  families only append their own blocks after. Family-driven ordering would
  be tidier but would reshuffle existing output for mixed runs — the
  byte-stability contract wins.
- **Warning, not error, on kind mismatch.** An error would be more
  corrective but turns a third-party step's metric bug into a failed run for
  the user; a warning keeps the run's outcome owned by checks and
  thresholds.
- **`Metrics` stays `pub` (hidden) for benches.** Not part of the supported
  surface; the registry is.

## Non-obvious pitfalls

- **Ordering drift.** Any change that makes family-rendered lines land
  before/after different generic lines changes the bytes external parsers
  see. The golden tests pin this; the `http` family must be seeded first.
- **Registry seeding vs tests.** Registration is global; a test that
  registers a family must not perturb other tests' output. Built-ins seed
  once inside the `OnceLock` init; test families use unique names and the
  default (empty) rendering hooks.
- **The `llm_tokens_per_sec` histogram is a counter-shaped rate.** It is
  declared a Histogram (samples per request) even though its unit is tok/s —
  kind describes the *fold shape*, not the semantics.
- **Derived `*_failed` rates.** The runner derives `<family>_failed` from
  every histogram a step emits; those derived names are *not* declared in
  `defs()` (they are mechanically derived), so validation must not warn on
  them. Only the emitted names are checked.

## Alternatives

- **Keep the fold, add a static lint.** Catches nothing at runtime (pro
  steps are compiled into the agent, and their emissions depend on runtime
  responses).
- **Full OpenTelemetry metrics in core.** Heavyweight: OTel instruments the
  process; perfscale's store is per-run aggregates with a text contract.
  OTel export remains a possible sink *behind* families later.
- **Per-step metric declarations in YAML.** Rejected: metrics are a property
  of the action implementation (Rust), not of user config; users would only
  ever copy declarations from docs.

## Rollout

1. This RFC.
2. Phase 1 (core): module + store move + registry + built-in http/llm
   families + fold-time warnings + `perfscale metrics` + golden tests +
   docs. No output changes (golden-tested).
3. Phase 2 (pro): families for webrtc/fix/soap/llm/pubsub in the pro crates,
   webrtc `getStats` moved behind `start_samplers`, perfscaled registration.
4. Future work: WASM guest metrics (with RFC 009), controlplane catalog UI.

## Open questions

- Should `catalog()` expose the derived `*_failed` rate names explicitly
  (computed from histogram defs) so the CLI lists them? Leaning yes in a
  follow-up; Phase 1 lists declared defs only.
- Should `stats_extras` be capped (line length / field count) so a pro
  family cannot flood the `[stats]` line?

## Success metrics

- Golden tests green: `[stats]` and summary bytes identical before/after the
  refactor for http and llm runs.
- `perfscale metrics` lists the http and llm families with correct kinds.
- A scalar-emitted `llm_ttft_ms` produces the run-log warning and still
  folds.
- Phase 2 registers a pro family without touching `perfscale-core`.
