//! Micro-benchmarks for the native engine's hot paths, complementing the
//! end-to-end suite in `scripts/bench.sh`:
//!
//! - YAML parsing (schema compile + validate + deserialize) — paid once per
//!   `perfscale run`/`lint` invocation.
//! - `${{ ... }}` interpolation — paid on every step of every iteration.
//! - Metrics recording and summary (percentile sort) — recording is per
//!   request under a mutex; the summary sorts the full sample vector once.
//! - `RingBuf` capture — paid per output line of every managed child process
//!   (`std/child_process@v1`).
//! - `waitUntil` matcher evaluation — polled against the captured buffers
//!   while a child process boots.
//!
//! Run with `cargo bench -p perfscale-core`.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use serde_json::json;

use perfscale_core::step::actions::HttpSample;
use perfscale_core::step::context::Context;
use perfscale_core::step::process::{RingBuf, WaitUntil};
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

/// A config exercising the process actions: `before` spawns a managed child
/// with a `waitUntil` gate, `after` stops it by registry name.
const PROCESS_CONFIG_YAML: &str = r#"
vus: 5
duration: 30s
allow_process_actions: true
before:
  - name: web
    uses: std/child_process@v1
    with:
      command: python3
      args: ["-m", "http.server", "8080"]
      port: 8080
      waitUntil:
        port_open: 8080
        timeout: 10s
      restart: on-failure
    outputs: web
after:
  - name: stop web
    uses: std/kill_process@v1
    with:
      name: web
      signal: TERM
"#;

fn bench_yaml_parse(c: &mut Criterion) {
    c.bench_function("yaml_parse_test_file", |b| {
        b.iter(|| yaml::parse_test_file(std::hint::black_box(TEST_YAML)).unwrap())
    });
    c.bench_function("yaml_parse_config_file", |b| {
        b.iter(|| yaml::parse_config_file(std::hint::black_box(CONFIG_YAML)).unwrap())
    });
    c.bench_function("yaml_parse_config_file_with_processes", |b| {
        b.iter(|| yaml::parse_config_file(std::hint::black_box(PROCESS_CONFIG_YAML)).unwrap())
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

/// Access-log-shaped line, typical of a managed web server's stdout.
const LOG_LINE: &str = "web: 127.0.0.1 - - [26/Jul/2026 13:09:18] \"GET / HTTP/1.1\" 200 -\n";

fn bench_ring_buf(c: &mut Criterion) {
    // Steady state: appending small lines to a buffer with room to spare —
    // what every reader task pays per output line.
    c.bench_function("ring_buf_push_small_lines", |b| {
        b.iter_batched(
            || RingBuf::new(64 * 1024),
            |mut buf| {
                for _ in 0..1_000 {
                    buf.push(LOG_LINE);
                }
                buf
            },
            BatchSize::SmallInput,
        )
    });

    // Buffer permanently full (a chatty long-running server): every push
    // also evicts from the front on a char boundary.
    c.bench_function("ring_buf_push_evicting", |b| {
        b.iter_batched(
            || {
                let mut buf = RingBuf::new(1024);
                while buf.as_str().len() < 1024 {
                    buf.push(LOG_LINE);
                }
                buf
            },
            |mut buf| {
                for _ in 0..1_000 {
                    buf.push(LOG_LINE);
                }
                buf
            },
            BatchSize::SmallInput,
        )
    });
}

/// Build a filled 64 KiB capture where the readiness marker sits in the last
/// line — the realistic "waited a while, then it appeared" scan shape.
fn filled_capture(marker_line: &str) -> String {
    let mut s = String::with_capacity(64 * 1024 + LOG_LINE.len() + marker_line.len());
    while s.len() < 64 * 1024 {
        s.push_str(LOG_LINE);
    }
    s.push_str(marker_line);
    s
}

fn bench_wait_until(c: &mut Criterion) {
    let stdout = filled_capture("READY marker 12345\n");
    let stderr = String::new();

    // Substring matcher against a 64 KiB capture — the marker is near the
    // end, so this is close to a full scan.
    let contains = WaitUntil::parse(&json!({ "stdout_contains": "READY marker" })).unwrap();
    c.bench_function("wait_until_stdout_contains", |b| {
        b.iter(|| {
            contains.matches_buffers(std::hint::black_box(&stdout), std::hint::black_box(&stderr))
        })
    });

    // Compiled-regex matcher over the same capture.
    let matches = WaitUntil::parse(&json!({ "stdout_matches": "READY marker \\d+" })).unwrap();
    c.bench_function("wait_until_stdout_matches_regex", |b| {
        b.iter(|| {
            matches.matches_buffers(std::hint::black_box(&stdout), std::hint::black_box(&stderr))
        })
    });

    // The string form is parsed once per step — the miniature expression
    // parser behind 'contains(stdout, "...")'.
    let string_form = json!("contains(stdout, \"Serving HTTP on 0.0.0.0 port 8080\")");
    c.bench_function("wait_until_parse_string_form", |b| {
        b.iter(|| WaitUntil::parse(std::hint::black_box(&string_form)).unwrap())
    });
}

criterion_group!(
    benches,
    bench_yaml_parse,
    bench_interpolate,
    bench_metrics,
    bench_ring_buf,
    bench_wait_until
);
criterion_main!(benches);
