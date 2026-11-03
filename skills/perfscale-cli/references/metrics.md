# Metrics

Run `perfscale metrics` to list every metric the registered families declare:
name, kind (counter / gauge / histogram), unit, and description.

Core families:

- `http_req_duration` — histogram, ms. Per-request latency; the source of
  `p50/p95/p99` SLOs.
- `http_reqs` / iteration counters — throughput and totals.
- `errors` — counter with a derived `rate` for threshold expressions.
- `vus` — gauge of active virtual users over the run.
- `dropped_iterations` — arrival-mode permits dropped because the worker pool
  was saturated; a leading indicator that the target cannot hold the rate.
- `perfscale_report_*_total{reporter}` — per-run delivery counters of the
  streaming reporters themselves (batches, dropped snapshots/series).

Reading results:

- The terminal summary prints per-metric aggregates; `--summary-export
  result.json` writes the machine-readable form (including the `thresholds`
  verdict).
- The MCP `parse_summary` tool turns raw k6-compatible output into the same
  structured metrics.
- Latency distributions come from histograms — cross-worker quantiles are
  computed from buckets, not client-side sketches.
