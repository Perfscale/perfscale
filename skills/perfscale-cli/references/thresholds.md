# Thresholds (SLO gates)

`std/thresholds@v1` is a run-level gate placed in the `after:` section of the
test file. It evaluates expressions over the run's metrics after the load
finishes.

```yaml
after:
  - use: std/thresholds@v1
    with:
      thresholds:
        - http_req_duration.p95 < 300        # latency SLO in ms
        - errors.rate < 0.01                 # error-rate SLO
    severity: fail                            # fail | warn | info
    message: checkout SLO
```

- `severity: fail` (default) — a violated gate makes the run exit non-zero.
- `warn` / `info` — reported but non-gating.
- `message` — custom label appended to the violation summary (interpolated).

Expressions compare an aggregated metric (`<metric>.<stat>`) against a
constant. Stats include `p50`, `p90`, `p95`, `p99`, `avg`, `min`, `max`,
`count`, and `rate` for counters. See `perfscale metrics` for the exact
metric names a run produces.

The run summary JSON carries a `thresholds` field with per-expression
pass/fail and observed values — parse it with the MCP `parse_summary` tool or
`--summary-export result.json`.

## Rules

- Thresholds are SLOs: treat them as user-owned. Never loosen a threshold to
  make a run pass.
- Keep gates in `after:` so they evaluate over the whole run, not per-step.
