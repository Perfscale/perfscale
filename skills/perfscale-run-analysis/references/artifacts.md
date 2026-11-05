# Run artifacts — what each one contains

## Final summary (stdout)

k6-compatible lines (`vus`, `iterations`, `http_req_duration` with
`avg/p(50)/p(90)/p(95)/p(99)/min/max`, `http_req_failed`, `http_reqs`, plus
every family that recorded samples). Percentiles come from an HDR histogram
(1 µs – 1 h, ≤1% quantile error). Arrival runs always print
`dropped_iterations`.

## `[stats]` lines (every 5 s while VUs run)

```text
[stats] ts=1720000000000 rps=246.80 err_pct=0.00 p50=1.20 p90=3.40 p95=4.10 p99=8.20 reqs=1234 iters=456
```

`rps` is the last 5 s window; `err_pct` and percentiles are cumulative;
staged/arrival runs add `vus=N`; LLM runs add `llm_reqs`, `llm_tok_s`.

## `--summary-export FILE` (JSON or Markdown)

```json
{
  "meta": { "perfscale_version": "…", "engine": "native", "vus": 10, "duration": "30s", "timestamp": "…" },
  "summary": { "avg_ms": 0.09, "p95_ms": 0.17, "p99_ms": 0.27, "error_rate": 0.0,
               "total_requests": 20872, "requests_per_sec": 20866.54 },
  "thresholds": { "status": "fail", "message": "…",
                  "violations": [{ "metric": "…", "expr": "p95<500", "actual": 612.0 }] }
}
```

`vus`/`duration` are `null` for k6/JMeter runs and for `stages`/`arrival`
profiles; `summary` is `null` with no HTTP samples. `--summary-format md` →
a table for `$GITHUB_STEP_SUMMARY`. A failed export write exits 1.

## `report@v1` (`--report json|junit|html:PATH`, `report.reporters`)

`schemaVersion`; `run` (engine-generated `run_id`, labels); `steps[]` —
count, failed, rate, duration quantiles **in seconds**, optional raw
samples (`max_samples`, `truncated`); `verdicts[]` —
`{ threshold, expression, pass, observed }`; `metrics` — every registered
family, `recorded: false` for silent ones. JUnit: one test case per
threshold. HTML: one self-contained offline page. With a serve target the
document is also POSTed to `<url>/api/v1/report`. Custom formats: write a
WASM reporter (`perfscale-library-sdk` skill).

## Streaming (during the run)

`report.reporters` with `during_run: true`: `std/report/prometheus-rw@v1`,
`std/report/otel@v1`, `std/report/serve@v1`; step-level `labels:` split
series per step. Reporter errors never fail the run; drops are counted in
`perfscale_report_dropped_snapshots_total` / `_dropped_series_total` /
`perfscale_report_errors_total` and summarized on stderr.

## Threshold expressions (`std/thresholds@v1`)

`<agg><op><number>` — agg ∈ `avg min max p50 p90 p95 p99 count rate`,
op ∈ `< <= > >= == !=`, plain number (ms for durations, no units).

- sample metrics (`*_duration`, `*_rtt`, `*_ms`): `avg/min/max/pNN/count`;
- counters (`*_errors`, `*_rows`, custom numbers): `count` only;
- failure rates (`*_failed`, derived per duration metric): `rate`, `count`.

An unknown metric or inapplicable aggregate is a hard error, never a
silent pass. `severity: fail` (exit 1) | `warn` | `info`.

## Exit codes

`0` — run completed (failed requests included); `1` — couldn't run, engine
crashed before results, or a `severity: fail` gate failed; `2` — bad CLI
arguments.
