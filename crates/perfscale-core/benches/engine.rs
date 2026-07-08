//! Micro-benchmarks for the native engine's hot paths, complementing the
//! end-to-end suite in `scripts/bench.sh`:
//!
//! - YAML parsing (schema compile + validate + deserialize) — paid once per
//!   `perfscale run`/`lint` invocation.
//! - `${{ ... }}` interpolation — paid on every step of every iteration.
//! - Metrics recording and summary (percentile sort) — recording is per
//!   request under a mutex; the summary sorts the full sample vector once.
//!
//! Run with `cargo bench -p perfscale-core`.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use serde_json::json;

use perfscale_core::step::actions::HttpSample;
use perfscale_core::step::context::Context;
use perfscale_core::step::runner::Metrics;
use perfscale_core::yaml;

const TEST_YAML: &str = r#"
steps:
  - name: fetch
    use: std/http@v1
    with:
      method: GET
      url: https://api.example.com/health
      headers:
        x-api-key: secret
    check:
      status: 200
    outputs: resp
  - use: std/check@v1
    with: { on: resp, duration_ms_lt: 500 }
  - use: std/sleep@v1
    with: { ms: 100 }
  - use: std/log@v1
    with: { message: "status was ${{ resp.status }}" }
"#;

const CONFIG_YAML: &str = "vus: 10\nduration: 30s\nreport:\n  url: http://localhost:7999\n";

fn bench_yaml_parse(c: &mut Criterion) {
    c.bench_function("yaml_parse_test_file", |b| {
        b.iter(|| yaml::parse_test_file(std::hint::black_box(TEST_YAML)).unwrap())
    });
    c.bench_function("yaml_parse_config_file", |b| {
        b.iter(|| yaml::parse_config_file(std::hint::black_box(CONFIG_YAML)).unwrap())
    });
}

fn bench_interpolate(c: &mut Criterion) {
    let mut ctx = Context::new();
    ctx.set(
        "resp",
        json!({ "status": 200, "body": "hello world", "duration_ms": 1.5 }),
    );

    // Typical `with:` block of an http step referencing a previous output —
    // what `execute_action` interpolates on every single iteration.
    let params = json!({
        "method": "POST",
        "url": "https://api.example.com/items?prev=${{ resp.status }}",
        "headers": { "x-prev-duration": "${{ resp.duration_ms }}" },
        "body": { "note": "prev body was ${{ resp.body }}", "count": 3 },
    });

    c.bench_function("interpolate_with_block", |b| {
        b.iter(|| ctx.interpolate_value(std::hint::black_box(&params)))
    });

    c.bench_function("interpolate_plain_string_no_placeholder", |b| {
        b.iter(|| {
            ctx.interpolate(std::hint::black_box(
                "a plain log message without placeholders",
            ))
        })
    });
}

fn bench_metrics(c: &mut Criterion) {
    let sample = HttpSample {
        duration_ms: 1.234,
        status: 200,
        failed: false,
    };

    c.bench_function("metrics_record_1k", |b| {
        b.iter_batched(
            Metrics::default,
            |mut m| {
                for _ in 0..1_000 {
                    m.record(&sample);
                }
                m
            },
            BatchSize::SmallInput,
        )
    });

    // 100k samples ≈ a 30s run at ~3.3k RPS; with the HDR histogram the
    // summary cost is bucket iteration, independent of sample count.
    let mut filled = Metrics::default();
    for i in 0..100_000u64 {
        filled.record(&HttpSample {
            duration_ms: (i % 977) as f64 * 0.013,
            status: 200,
            failed: i % 100 == 0,
        });
    }
    c.bench_function("metrics_summary_100k", |b| {
        b.iter(|| filled.summary_lines(std::hint::black_box(30.0), 100_000, 10))
    });
}

criterion_group!(benches, bench_yaml_parse, bench_interpolate, bench_metrics);
criterion_main!(benches);
