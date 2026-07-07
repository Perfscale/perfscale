//! k6-compatible summary parser.
//!
//! All three engines (k6, locust, native) emit — or are translated into — the
//! k6 text summary format:
//!
//! ```text
//! http_req_duration......: avg=0.42ms p(50)=0.31ms p(90)=0.88ms p(95)=1.02ms p(99)=1.90ms min=0.09ms max=3.10ms
//! http_req_failed........: 0.00%
//! http_reqs..............: 120 2.00/s
//! ```
//!
//! This module parses that format back into a structured [`RunSummary`], so
//! downstream consumers (dashboards, control planes, CI reporters) don't each
//! hand-roll their own line parser.

use serde::{Deserialize, Serialize};

/// Structured metrics extracted from a k6-compatible summary.
///
/// Latency fields are `None` when the corresponding token was absent from the
/// output (e.g. a sleep-only run emits no `http_req_duration` line at all).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    /// `avg=` — mean request duration, milliseconds.
    pub avg_ms: Option<f64>,
    /// `p(50)=` or k6's `med=` — median request duration, milliseconds.
    pub med_ms: Option<f64>,
    /// `p(90)=` — milliseconds.
    pub p90_ms: Option<f64>,
    /// `p(95)=` — milliseconds.
    pub p95_ms: Option<f64>,
    /// `p(99)=` — milliseconds. Real k6 omits this by default; the native
    /// engine always emits it.
    pub p99_ms: Option<f64>,
    /// `min=` — milliseconds.
    pub min_ms: Option<f64>,
    /// `max=` — milliseconds.
    pub max_ms: Option<f64>,
    /// `http_req_failed` as a fraction in `0.0..=1.0`.
    pub error_rate: f64,
    /// `http_reqs` count.
    pub total_requests: u64,
    /// `http_reqs` rate, requests per second.
    pub requests_per_sec: f64,
}

/// Parse a k6-compatible summary out of raw run output.
///
/// Scans every line, so the summary may be embedded in arbitrary log noise
/// (progress bars, warnings, `[err]` prefixes). Returns `None` when no
/// request metrics were found at all — callers treat that as "the run
/// produced no parseable summary" rather than a run with zero traffic.
pub fn parse_summary(output: &str) -> Option<RunSummary> {
    let mut s = RunSummary {
        avg_ms: None,
        med_ms: None,
        p90_ms: None,
        p95_ms: None,
        p99_ms: None,
        min_ms: None,
        max_ms: None,
        error_rate: 0.0,
        total_requests: 0,
        requests_per_sec: 0.0,
    };

    for line in output.lines() {
        let t = line.trim();

        // k6 prints a second `http_req_duration{expected_response:true}` line —
        // skip it so the unfiltered aggregate wins.
        if t.contains("http_req_duration") && !t.contains("expected_response") {
            s.avg_ms = extract_ms(t, "avg=").or(s.avg_ms);
            s.med_ms = extract_ms(t, "p(50)=")
                .or_else(|| extract_ms(t, "med="))
                .or(s.med_ms);
            s.p90_ms = extract_ms(t, "p(90)=").or(s.p90_ms);
            s.p95_ms = extract_ms(t, "p(95)=").or(s.p95_ms);
            s.p99_ms = extract_ms(t, "p(99)=").or(s.p99_ms);
            s.min_ms = extract_ms(t, "min=").or(s.min_ms);
            s.max_ms = extract_ms(t, "max=").or(s.max_ms);
        }

        // `http_reqs` but not `http_reqs_...` metric variants.
        if let Some(rest) = metric_value(t, "http_reqs") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if !parts.is_empty() {
                s.total_requests = parts[0].parse().unwrap_or(0);
            }
            if parts.len() >= 2 {
                s.requests_per_sec = parts[1].trim_end_matches("/s").parse().unwrap_or(0.0);
            }
        }

        if let Some(rest) = metric_value(t, "http_req_failed") {
            let chunk = rest.split_whitespace().next().unwrap_or("0%");
            s.error_rate = chunk.trim_end_matches('%').parse::<f64>().unwrap_or(0.0) / 100.0;
        }
    }

    if s.total_requests > 0 || s.requests_per_sec > 0.0 {
        Some(s)
    } else {
        None
    }
}

/// Match `name`, optional dot padding, `:`, and return the value part.
///
/// Handles both raw k6 (`http_reqs......................: 100 9.90/s`) and
/// the native engine's shorter padding. Rejects prefix collisions like
/// `http_reqs_failed` by requiring the name to be followed by `.`, `:` or
/// whitespace.
fn metric_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(name)?;
    let after = rest.trim_start_matches('.').trim_start();
    // The char right after `name` must be padding or the separator —
    // otherwise this is a different metric that merely shares the prefix.
    match rest.chars().next() {
        Some('.') | Some(':') | Some(' ') => {}
        _ => return None,
    }
    after.strip_prefix(':').map(str::trim_start)
}

/// Extract a duration token like `avg=1.42ms`, `p(95)=1.02s`, `min=980µs`
/// and normalise it to milliseconds.
fn extract_ms(line: &str, prefix: &str) -> Option<f64> {
    let start = line.find(prefix)? + prefix.len();
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace())
        .unwrap_or(rest.len());
    let token = &rest[..end];

    if let Some(v) = token.strip_suffix("ms") {
        return v.parse().ok();
    }
    if let Some(v) = token.strip_suffix("µs") {
        return v.parse::<f64>().ok().map(|x| x / 1000.0);
    }
    if let Some(v) = token.strip_suffix("us") {
        return v.parse::<f64>().ok().map(|x| x / 1000.0);
    }
    if let Some(v) = token.strip_suffix('m') {
        // minutes (k6 prints e.g. `1m30s` only in duration configs, but a
        // bare `Xm` can appear for very slow endpoints)
        return v.parse::<f64>().ok().map(|x| x * 60_000.0);
    }
    if let Some(v) = token.strip_suffix('s') {
        return v.parse::<f64>().ok().map(|x| x * 1000.0);
    }
    token.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Output shape produced by the native step engine
    /// (see `step::runner::Metrics::summary_lines`).
    const NATIVE_OUTPUT: &str = "\
[sys] Starting 2 VUs for 10s (10s)
vus....................: 2 min=1 max=2
iterations..............: 40 4.00/s
http_req_duration......: avg=0.42ms p(50)=0.31ms p(90)=0.88ms p(95)=1.02ms p(99)=1.90ms min=0.09ms max=3.10ms
http_req_failed........: 5.00%
http_reqs..............: 120 2.00/s
";

    /// Trimmed real `k6 run` summary (default end-of-test block).
    const K6_OUTPUT: &str = "\
     data_received..................: 1.2 MB 40 kB/s
     data_sent......................: 8.1 kB 270 B/s
     http_req_duration..............: avg=1.42ms min=980µs med=1.30ms max=12.51ms p(90)=1.80ms p(95)=2.10ms
       { expected_response:true }...: avg=1.40ms min=980µs med=1.29ms max=9.11ms p(90)=1.78ms p(95)=2.05ms
     http_req_failed................: 1.35%  ✓ 4    ✗ 292
     http_reqs......................: 296    9.86/s
     iteration_duration.............: avg=1.01s  min=1s  med=1.01s max=1.05s p(90)=1.01s p(95)=1.02s
     iterations.....................: 296    9.86/s
";

    #[test]
    fn parses_native_engine_summary() {
        let s = parse_summary(NATIVE_OUTPUT).unwrap();
        assert_eq!(s.avg_ms, Some(0.42));
        assert_eq!(s.med_ms, Some(0.31));
        assert_eq!(s.p90_ms, Some(0.88));
        assert_eq!(s.p95_ms, Some(1.02));
        assert_eq!(s.p99_ms, Some(1.90));
        assert_eq!(s.min_ms, Some(0.09));
        assert_eq!(s.max_ms, Some(3.10));
        assert!((s.error_rate - 0.05).abs() < 1e-9);
        assert_eq!(s.total_requests, 120);
        assert!((s.requests_per_sec - 2.0).abs() < 1e-9);
    }

    #[test]
    fn parses_real_k6_summary() {
        let s = parse_summary(K6_OUTPUT).unwrap();
        assert_eq!(s.avg_ms, Some(1.42));
        assert_eq!(s.med_ms, Some(1.30), "med= maps to med_ms");
        assert_eq!(s.min_ms, Some(0.98), "µs normalised to ms");
        assert_eq!(s.max_ms, Some(12.51));
        assert_eq!(s.p90_ms, Some(1.80));
        assert_eq!(s.p95_ms, Some(2.10));
        assert_eq!(s.p99_ms, None, "k6 default summary has no p(99)");
        assert!((s.error_rate - 0.0135).abs() < 1e-9);
        assert_eq!(s.total_requests, 296);
        assert!((s.requests_per_sec - 9.86).abs() < 1e-9);
    }

    #[test]
    fn expected_response_line_does_not_override_aggregate() {
        let s = parse_summary(K6_OUTPUT).unwrap();
        // The { expected_response:true } line has max=9.11ms; the aggregate
        // line's 12.51ms must win.
        assert_eq!(s.max_ms, Some(12.51));
    }

    #[test]
    fn no_metrics_returns_none() {
        assert!(parse_summary("").is_none());
        assert!(parse_summary("[sys] Starting 1 VU for 10s\nrandom noise\n").is_none());
    }

    #[test]
    fn sleep_only_run_with_zero_reqs_is_none() {
        // Native engine emits only vus/iterations lines when no HTTP happened.
        let out = "vus....................: 1 min=1 max=1\niterations..............: 10 1.00/s\n";
        assert!(parse_summary(out).is_none());
    }

    #[test]
    fn http_reqs_prefix_variants_do_not_collide() {
        let out = "http_reqs_custom.......: 999 9.99/s\n";
        assert!(parse_summary(out).is_none(), "http_reqs_custom must not match http_reqs");
    }

    #[test]
    fn seconds_and_micros_normalise_to_ms() {
        let out = "\
http_req_duration......: avg=1.5s p(50)=250µs p(95)=2s min=1ms max=1m
http_reqs..............: 10 1.00/s
";
        let s = parse_summary(out).unwrap();
        assert_eq!(s.avg_ms, Some(1500.0));
        assert_eq!(s.med_ms, Some(0.25));
        assert_eq!(s.p95_ms, Some(2000.0));
        assert_eq!(s.min_ms, Some(1.0));
        assert_eq!(s.max_ms, Some(60_000.0));
    }

    #[test]
    fn summary_serde_round_trip() {
        let s = parse_summary(NATIVE_OUTPUT).unwrap();
        let json = serde_json::to_string(&s).unwrap();
        let back: RunSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
