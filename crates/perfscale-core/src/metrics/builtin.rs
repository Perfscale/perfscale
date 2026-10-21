//! The built-in metric families, seeded into the registry before any
//! downstream registration: `http` (the request metrics every request-like
//! step feeds, plus the run-progress gauges) and `llm` (what `std/llm@v1`
//! emits — the step lives in core, so its defs live here too).

use super::registry::{MetricDef, MetricKind, MetricsFamily};
use super::store::Metrics;

/// The HTTP family: the `http_req_*` triple plus the run-progress gauges
/// that ride every during-run snapshot. Owns the summary's `http_req_*`
/// block — rendered here, after the generic lines, exactly where the
/// hardcoded runner block used to sit (the golden tests pin the bytes).
pub(crate) struct HttpFamily;

impl MetricsFamily for HttpFamily {
    fn family(&self) -> &'static str {
        "http"
    }

    fn defs(&self) -> &'static [MetricDef] {
        &[
            MetricDef {
                name: "http_req_duration",
                kind: MetricKind::Histogram,
                unit: "ms",
                description: "Request duration, recorded by every request-like step",
            },
            MetricDef {
                name: "http_req_failed",
                kind: MetricKind::Rate,
                unit: "ratio",
                description: "Share of failed requests (failed/total)",
            },
            MetricDef {
                name: "http_reqs",
                kind: MetricKind::Counter,
                unit: "count",
                description: "Total requests",
            },
            MetricDef {
                name: "perfscale_iterations_completed",
                kind: MetricKind::Gauge,
                unit: "count",
                description: "Iterations started since run start (during-run snapshots)",
            },
            MetricDef {
                name: "perfscale_vus",
                kind: MetricKind::Gauge,
                unit: "count",
                description: "Live VU count (configured count on fixed runs)",
            },
        ]
    }

    fn summary_lines(&self, store: &Metrics, wall_secs: f64) -> Vec<String> {
        if store.total == 0 {
            return Vec::new();
        }
        let h = &store.durations_micros;
        let pct = |q: f64| -> f64 { h.value_at_quantile(q) as f64 / 1000.0 };
        let rps = store.total as f64 / wall_secs.max(0.001);
        let err = store.failures as f64 / store.total as f64 * 100.0;
        vec![
            format!(
                "http_req_duration......: avg={avg:.2}ms p(50)={p50:.2}ms p(90)={p90:.2}ms p(95)={p95:.2}ms p(99)={p99:.2}ms min={min:.2}ms max={max:.2}ms",
                avg = h.mean() / 1000.0,
                p50 = pct(0.50),
                p90 = pct(0.90),
                p95 = pct(0.95),
                p99 = pct(0.99),
                min = h.min() as f64 / 1000.0,
                max = h.max() as f64 / 1000.0,
            ),
            format!("http_req_failed........: {err:.2}%"),
            format!("http_reqs..............: {total} {rps:.2}/s", total = store.total),
        ]
    }
}

/// The LLM family: what `std/llm@v1` reports. Owns the `[stats]` extras
/// (`llm_reqs`/`llm_tok_s`) — LLM steps never produce `HttpSample`s, so this
/// is the live signal on pure-LLM runs. Summary rendering stays generic
/// (default hook), matching the historical output.
pub(crate) struct LlmFamily;

impl MetricsFamily for LlmFamily {
    fn family(&self) -> &'static str {
        "llm"
    }

    fn defs(&self) -> &'static [MetricDef] {
        &[
            MetricDef {
                name: "llm_ttft_ms",
                kind: MetricKind::Histogram,
                unit: "ms",
                description: "Time to first token",
            },
            MetricDef {
                name: "llm_tokens_per_sec",
                kind: MetricKind::Histogram,
                unit: "tok/s",
                description: "Completion throughput per request",
            },
            MetricDef {
                name: "llm_prompt_tokens",
                kind: MetricKind::Counter,
                unit: "count",
                description: "Prompt tokens, summed across requests",
            },
            MetricDef {
                name: "llm_completion_tokens",
                kind: MetricKind::Counter,
                unit: "count",
                description: "Completion tokens, summed across requests",
            },
            MetricDef {
                name: "llm_chunks",
                kind: MetricKind::Counter,
                unit: "count",
                description: "Streamed chunks, summed across requests",
            },
        ]
    }

    fn stats_extras(&self, store: &Metrics) -> String {
        match store.hists.get("llm_tokens_per_sec") {
            // Samples are stored in microseconds (ms * 1000) like every
            // custom histogram; tok/s means are read back in plain units.
            Some(tps) if !tps.is_empty() => {
                format!(
                    " llm_reqs={} llm_tok_s={:.1}",
                    tps.len(),
                    tps.mean() / 1000.0
                )
            }
            _ => String::new(),
        }
    }
}

/// Engine self-observability (RFC 010 family for what the
/// `engine_metrics:` probe records — see [`super::engine`]). Rendering
/// stays generic: the lag histogram and the depth gauge need no
/// family-owned shapes.
pub(crate) struct EngineFamily;

impl MetricsFamily for EngineFamily {
    fn family(&self) -> &'static str {
        "engine"
    }

    fn defs(&self) -> &'static [MetricDef] {
        &[
            MetricDef {
                name: "engine_scheduler_lag_ms",
                kind: MetricKind::Histogram,
                unit: "ms",
                description: "Tokio scheduler lag: probe wake-up delay (actual minus expected)",
            },
            MetricDef {
                name: "engine_log_channel_depth_max",
                kind: MetricKind::Gauge,
                unit: "lines",
                description: "Peak pending lines in the bounded run-log channel",
            },
            MetricDef {
                name: "engine_arrival_queue_wait_ms",
                kind: MetricKind::Histogram,
                unit: "ms",
                description: "Arrival profile: scheduled-to-actual iteration start delay (coordinated omission)",
            },
            MetricDef {
                name: "engine_arrival_iteration_ms",
                kind: MetricKind::Histogram,
                unit: "ms",
                description: "Arrival profile: queue wait + iteration body, measured from the scheduled start",
            },
            MetricDef {
                name: "dropped_iterations",
                kind: MetricKind::Counter,
                unit: "count",
                description: "Arrival permits no worker could serve (pool saturated at arrival.max_vus)",
            },
            MetricDef {
                name: "engine_log_lines_dropped",
                kind: MetricKind::Counter,
                unit: "lines",
                description: "Per-step log lines dropped because the bounded run-log channel was full",
            },
        ]
    }
}

/// `std/pubsub@v1` metrics (emitted by the core step in `step/pubsub.rs` —
/// the pro packages only ship the broker drivers, so the defs live here).
pub(crate) struct PubsubFamily;

impl MetricsFamily for PubsubFamily {
    fn family(&self) -> &'static str {
        "pubsub"
    }

    fn defs(&self) -> &'static [MetricDef] {
        &[
            MetricDef {
                name: "pubsub_msgs_published",
                kind: MetricKind::Counter,
                unit: "count",
                description: "Messages accepted by the transport",
            },
            MetricDef {
                name: "pubsub_msgs_received",
                kind: MetricKind::Counter,
                unit: "count",
                description: "Messages counted toward the subscribe expectation",
            },
            MetricDef {
                name: "pubsub_publish_ms",
                kind: MetricKind::Histogram,
                unit: "ms",
                description: "Publish-call latency per message (nats, flush=message: the batch flush+ack folds into the last sample; flush=none: enqueue only)",
            },
            MetricDef {
                name: "pubsub_connect_ms",
                kind: MetricKind::Histogram,
                unit: "ms",
                description: "Broker connect time per exchange that opened a connection (scope=exchange: every exchange; scope=vu: first exchange of each VU)",
            },
            MetricDef {
                name: "pubsub_errors",
                kind: MetricKind::Counter,
                unit: "count",
                description: "Failed exchanges (connect/publish errors, subscribe shortfalls); 0 per success",
            },
            MetricDef {
                name: "pubsub_e2e_ms",
                kind: MetricKind::Histogram,
                unit: "ms",
                description: "End-to-end latency: publish-phase start to message receipt",
            },
        ]
    }
}
