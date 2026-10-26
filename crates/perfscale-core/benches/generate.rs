//! Baseline for the per-message generator hot path (`generate::Gen`): a
//! Criterion timing benchmark plus a no-dependency allocation report, both in
//! this one `harness = false` binary so the allocation numbers and the timings
//! describe the exact same deterministic workloads.
//!
//! Every operation is `Gen::new(SEED)` + `begin_message()` + one expansion, so
//! outputs are fully deterministic (no `now*` tokens). Cases:
//!
//! - `static_ascii` — plain ASCII, no tokens (fast-path `contains("${")`).
//! - `mixed_non_ascii` — non-ASCII literal mixed with `${seq}` and an unknown
//!   token (left verbatim).
//! - `builtins_heavy` — `${rand(1,1000000)}`, `${randf(1.0,2.0,3)}`,
//!   `${choice(a|b|c)}`, `${uuid}`, plus an unknown token.
//! - `json_tokens` / `json_static` — `expand_tokens` over a nested JSON value
//!   with two tokens / token-free (the token-free case exercises the full
//!   clone the current `&Value -> Value` API performs).
//! - `json_tokens_in_place` / `json_static_in_place` — the same shapes through
//!   `expand_tokens_in_place(&mut Value)`: the owned `Value` is created outside
//!   the measured region (Criterion `iter_batched` setup / a prebuilt vector
//!   before the counter snapshot), so the numbers isolate the eliminated deep
//!   clone.
//!
//! Run: `cargo bench -p perfscale-core --bench generate`
//! (the allocation report prints after Criterion's summary).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use criterion::{black_box, Criterion};
use serde_json::{json, Value};

use perfscale_core::generate::{expand_tokens, expand_tokens_in_place, Gen};

// ---------------------------------------------------------------------------
// Allocation counter
// ---------------------------------------------------------------------------

/// `#[global_allocator]` wrapper over the system allocator. Counts only
/// `alloc` and `realloc` calls (plus their bytes) — `dealloc` is deliberately
/// NOT counted, so the numbers read as "allocations per operation", not a
/// mix of allocation and free traffic. Counting is a single relaxed
/// `fetch_add` per counter; the wrapper never allocates, logs, or locks, so
/// it cannot recurse into itself.
struct CountingAlloc;

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // A realloc is one allocation event for accounting purposes (the old
        // block may be freed, but that is dealloc traffic, not an allocation).
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

#[derive(Clone, Copy)]
struct AllocSnapshot {
    calls: u64,
    bytes: u64,
}

fn alloc_snapshot() -> AllocSnapshot {
    AllocSnapshot {
        calls: ALLOC_CALLS.load(Ordering::Relaxed),
        bytes: ALLOC_BYTES.load(Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------
// Deterministic workloads
// ---------------------------------------------------------------------------

const SEED: u64 = 42;

const STATIC_ASCII: &str = "EURUSD 1.0850 limit order submit venue=X liquidty pool alpha";

const MIXED_NON_ASCII: &str = "Заявка №${seq} — інструмент €/$, статус: ${unknown_tag} ✓";

const BUILTINS_HEAVY: &str = concat!(
    "id=${rand(1,1000000)} price=${randf(1.0,2.0,3)} side=${choice(a|b|c)} ",
    "ref=${uuid} tag=${custom.tag}"
);

fn json_tokens() -> Value {
    json!({
        "order": {
            "id": "${rand(1,1000000)}",
            "symbol": "EURUSD",
            "price": 1.0850,
            "legs": [
                {"qty": 100, "venue": "X"},
                {"qty": 25, "venue": "Y", "note": "msg-${seq}"}
            ],
            "meta": {"tags": ["alpha", "beta"], "retries": 0}
        }
    })
}

fn json_static() -> Value {
    json!({
        "order": {
            "id": "123456",
            "symbol": "EURUSD",
            "price": 1.0850,
            "legs": [
                {"qty": 100, "venue": "X"},
                {"qty": 25, "venue": "Y", "note": "static"}
            ],
            "meta": {"tags": ["alpha", "beta"], "retries": 0}
        }
    })
}

/// One operation: fresh generator (fixed seed), `begin_message`, expand.
/// Building the generator per op matches the engine's per-message usage and
/// keeps every op identical, which is what the before/after comparison needs.
fn expand_once(template: &str) -> String {
    let mut gen = Gen::new(SEED);
    gen.begin_message();
    gen.expand(template).expect("built-in tokens never fail")
}

/// Same shape for the JSON path: expand every string leaf of `v`.
fn expand_json_once(v: &Value) -> Value {
    let mut gen = Gen::new(SEED);
    gen.begin_message();
    expand_tokens(v, &mut gen).expect("built-in tokens never fail")
}

/// In-place variant: the caller owns the value, so no second tree is built.
fn expand_json_once_in_place(v: &mut Value) {
    let mut gen = Gen::new(SEED);
    gen.begin_message();
    expand_tokens_in_place(v, &mut gen).expect("built-in tokens never fail")
}

// ---------------------------------------------------------------------------
// Criterion benchmarks
// ---------------------------------------------------------------------------

fn bench_generate(c: &mut Criterion) {
    let mut group = c.benchmark_group("generate");
    group.bench_function("static_ascii", |b| {
        b.iter(|| black_box(expand_once(black_box(STATIC_ASCII))))
    });
    group.bench_function("mixed_non_ascii", |b| {
        b.iter(|| black_box(expand_once(black_box(MIXED_NON_ASCII))))
    });
    group.bench_function("builtins_heavy", |b| {
        b.iter(|| black_box(expand_once(black_box(BUILTINS_HEAVY))))
    });
    let with_tokens = json_tokens();
    group.bench_function("json_tokens", |b| {
        b.iter(|| black_box(expand_json_once(black_box(&with_tokens))))
    });
    let without_tokens = json_static();
    group.bench_function("json_static", |b| {
        b.iter(|| black_box(expand_json_once(black_box(&without_tokens))))
    });
    // In-place variants: `iter_batched` builds a fresh owned `Value` in the
    // setup phase (unmeasured), so the timed routine measures expansion only —
    // the elimination of the second deep clone, not JSON construction.
    // Same shapes and seed as the `&Value` cases above.
    group.bench_function("json_tokens_in_place", |b| {
        b.iter_batched(
            json_tokens,
            |mut v| {
                expand_json_once_in_place(&mut v);
                black_box(v)
            },
            criterion::BatchSize::SmallInput,
        )
    });
    group.bench_function("json_static_in_place", |b| {
        b.iter_batched(
            json_static,
            |mut v| {
                expand_json_once_in_place(&mut v);
                black_box(v)
            },
            criterion::BatchSize::SmallInput,
        )
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Allocation report (same cases, counted outside the Criterion timing loop)
// ---------------------------------------------------------------------------

/// Expand `op` `iters` times between counter snapshots and return the
/// per-operation allocation delta. Nothing is printed or formatted inside the
/// counted region; the results are accumulated and dropped only after the
/// second snapshot so result-`Drop` deallocs (uncounted anyway) cannot smear
/// the byte totals of the next case.
fn count_allocs(iters: u64, mut op: impl FnMut() -> String) -> (f64, f64) {
    // Warm-up outside the counted region (allocator arenas, lazy init).
    for _ in 0..iters.min(1024) {
        black_box(op());
    }
    let before = alloc_snapshot();
    for _ in 0..iters {
        black_box(op());
    }
    let after = alloc_snapshot();
    let n = iters as f64;
    (
        (after.calls - before.calls) as f64 / n,
        (after.bytes - before.bytes) as f64 / n,
    )
}

fn count_allocs_json(iters: u64, v: &Value) -> (f64, f64) {
    for _ in 0..iters.min(1024) {
        black_box(expand_json_once(black_box(v)));
    }
    let before = alloc_snapshot();
    for _ in 0..iters {
        black_box(expand_json_once(black_box(v)));
    }
    let after = alloc_snapshot();
    let n = iters as f64;
    (
        (after.calls - before.calls) as f64 / n,
        (after.bytes - before.bytes) as f64 / n,
    )
}

/// In-place variant: the owned `Value`s are prebuilt BEFORE the snapshot (a
/// bounded vector, not one clone per op), so setup allocations never enter the
/// counted region and the delta is expansion-only — directly comparable to
/// `count_allocs_json`, which includes the full tree clone per op.
fn count_allocs_json_in_place(iters: u64, proto: &Value) -> (f64, f64) {
    let mut warm: Vec<Value> = (0..iters.min(1024)).map(|_| proto.clone()).collect();
    for v in &mut warm {
        expand_json_once_in_place(black_box(v));
    }
    let mut values: Vec<Value> = (0..iters).map(|_| proto.clone()).collect();
    let before = alloc_snapshot();
    for v in &mut values {
        expand_json_once_in_place(black_box(v));
    }
    let after = alloc_snapshot();
    let n = iters as f64;
    (
        (after.calls - before.calls) as f64 / n,
        (after.bytes - before.bytes) as f64 / n,
    )
}

fn report_allocations() {
    const ITERS: u64 = 100_000;
    // In-place cases prebuild one owned `Value` per op; 10k keeps the
    // prebuilt vector small while the per-op accounting stays exact.
    const ITERS_IN_PLACE: u64 = 10_000;
    let cases: [(&str, (f64, f64)); 7] = [
        (
            "static_ascii",
            count_allocs(ITERS, || expand_once(STATIC_ASCII)),
        ),
        (
            "mixed_non_ascii",
            count_allocs(ITERS, || expand_once(MIXED_NON_ASCII)),
        ),
        (
            "builtins_heavy",
            count_allocs(ITERS, || expand_once(BUILTINS_HEAVY)),
        ),
        ("json_tokens", count_allocs_json(ITERS, &json_tokens())),
        ("json_static", count_allocs_json(ITERS, &json_static())),
        (
            "json_tokens_in_place",
            count_allocs_json_in_place(ITERS_IN_PLACE, &json_tokens()),
        ),
        (
            "json_static_in_place",
            count_allocs_json_in_place(ITERS_IN_PLACE, &json_static()),
        ),
    ];
    println!();
    println!("allocations per operation (alloc+realloc calls; dealloc not counted; {ITERS} iters, seed {SEED}):");
    for (name, (calls, bytes)) in cases {
        println!("  {name:<16} {calls:>8.2} allocs/op  {bytes:>9.1} bytes/op");
    }
}

// ---------------------------------------------------------------------------
// Entry point: Criterion with its default config, then the alloc report
// ---------------------------------------------------------------------------

fn main() {
    let mut criterion = Criterion::default().configure_from_args();
    bench_generate(&mut criterion);
    criterion.final_summary();
    report_allocations();
}
