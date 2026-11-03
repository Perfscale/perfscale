# Reporters

perfscale has end-of-run reporters (CLI flags) and streaming reporters
(config `report.reporters`).

## End-of-run

```sh
perfscale run -f test.yaml -c config.yaml \
  --report json:report.json \    # canonical report@v1 artifact
  --report junit:junit.xml \     # CI test report
  --report html:report.html \    # self-contained dark HTML page
  --report http://localhost:7999 # POST to `perfscale serve`
```

`--summary-export result.json` writes the k6-compatible summary;
`--summary-format md` renders markdown (e.g. `$GITHUB_STEP_SUMMARY`).

## Streaming (`report.reporters` in config.yaml)

Live, cumulative metrics during the run:

- `std/report/serve@v1` — the historical `report.url` path as a reporter.
- `std/report/prometheus-rw@v1` — Prometheus remote write (Snappy + protobuf;
  classic `_bucket{le}` histograms by default, native opt-in via
  `with.histogram: native`).
- `std/report/otel@v1` — OTLP/gRPC with cumulative exponential histograms.

Engine-owned keys sit next to `use:`: `during_run`, `interval_ms`, `labels`,
`max_series` (default 10000). Labels merge step > reporter > run and
interpolate `${{ inputs.* }}` / `${{ env.* }}` once before the run.

Delivery guarantees: independent bounded queue (`with.max_pending`), batched
sends (`with.batch_size`), retry with backoff (`with.retry`, `Retry-After`
honored), and a synchronous final flush bounded by `with.final_timeout`
(default 30s). Overflow drops the oldest snapshot; a slow backend can never
block VUs or fail the run.

`during_run: true` on a final-only reporter (json/junit/html/wasm) is a
lint/run error.
