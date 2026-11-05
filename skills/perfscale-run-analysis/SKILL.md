---
name: perfscale-run-analysis
description: Analyze a perfscale load-test run — read the final summary, live [stats] lines, --summary-export JSON and report@v1 documents; judge latency under open vs closed models (coordinated omission, dropped_iterations, engine_arrival_* queue wait); tell target saturation from load-generator limits (engine_metrics, port exhaustion); debug failures with abort_on_fail + http_debug; gate CI with std/thresholds@v1; compare runs fairly. Use after a run, when results look wrong, or when turning results into an SLO verdict.
---

# Analyzing a perfscale run

Read the run in this order — each step tells you whether the next one is
worth looking at.

## 1. Did the run measure what you think?

```text
vus....................: 10 min=1 max=10
iterations..............: 4521 150.23/s
http_req_duration......: avg=0.42ms p(50)=0.31ms p(90)=0.88ms p(95)=1.02ms p(99)=1.90ms min=0.09ms max=3.10ms
http_req_failed........: 0.00%
http_reqs..............: 4521 150.23/s
```

- `vus` min/max — did the ramp reach the target concurrency?
- `iterations` vs `http_reqs` — requests per iteration as designed?
- For arrival runs: `dropped_iterations` must be `0`, else the offered rate
  was **not** delivered (see [diagnosis](references/diagnosis.md#dropped-iterations)).
- `http_req_*` lines appear only if some step recorded HTTP samples;
  gRPC, DB, pub/sub, LLM have their own families (`grpc_req_duration`,
  `db_query_duration`, `pubsub_publish_ms`, `llm_ttft_ms`, …) —
  `perfscale metrics` lists every metric with kind and unit.

## 2. Is the latency honest?

`http_req_duration` is **server time of requests that started**. In a closed
model (`vus` / `stages`) a slow target slows the load down and the
percentiles stay pretty. Use the arrival model (`arrival:`) to see the
user-side truth:

| Metric | Read it as |
|---|---|
| `engine_arrival_queue_wait_ms` | how late iterations started vs schedule (coordinated omission) |
| `engine_arrival_iteration_ms` | queue wait + body — end-to-end latency a user would see |
| `dropped_iterations` | iterations no worker could start — the offered rate exceeded capacity |

Judge an overloaded arrival run by all three together.

## 3. Who is the bottleneck — target or generator?

Percentiles from the target are only meaningful if the generator kept up.
Enable the engine's self-probe on suspicious runs:

```yaml
engine_metrics: { enabled: true, interval_ms: 100 }
```

`engine_scheduler_lag_ms` growing → the generator's runtime is starved (CPU);
`engine_log_channel_depth_max` near 512 → step logging back-pressures VUs
(run with `--quiet`). Transport errors like `Can't assign requested address`
are the machine's port budget, not the target — see
[diagnosis](references/diagnosis.md#connection-and-port-errors).

## 4. Why did requests fail?

`http_req_failed` counts status ≥ 400, transport errors and timeouts. To see
**which** requests failed and with what payload, gate the run and turn on
diagnostics:

```yaml
after:
  - name: slo gate
    use: std/thresholds@v1
    with:
      http_req_failed: ["rate<0.01"]
      http_req_duration: ["p95<500"]
    abort_on_fail: true     # re-evaluated every 250 ms; stops load early on violation
    http_debug: error       # one masked JSON line per failed action / check: VU, iteration, request, response
```

`abort_on_fail` stops load between steps; `after:` teardown, reporters and
the summary still run. Secrets are masked in `http_debug` lines.

## 5. Turn it into a verdict and an artifact

```console
$ perfscale run -f test.yaml -c config.yaml --quiet \
    --summary-export result.json \
    --summary-export "$GITHUB_STEP_SUMMARY" --summary-format md \
    --report html:report.html --report junit:junit.xml --report json:report.json
```

- Exit code `1` when a `severity: fail` gate fails — that is the CI signal.
  Failed requests alone exit `0` (load-test feedback, like k6).
- `--summary-export` → compact JSON (`meta`, `summary`, `thresholds`);
  `--report json` → full `report@v1` (per-step aggregates, verdicts, every
  metric family). Field reference: [artifacts.md](references/artifacts.md).
- Live during the run: `[stats]` lines every 5 s, `perfscale serve --ui`
  dashboard, or streaming reporters (Prometheus remote write, OTLP).

## 6. Comparing runs

Compare like with like — see [diagnosis](references/diagnosis.md#comparing-runs):
same machine class, same profile, alternate A/B, warm-up first, several
rounds. A uniform shift across unrelated metrics is the machine, not the
code.

Custom WASM steps/functions: the summary's `libraries:` line carries
`step_calls`, `step_errors`, `step_p95_ms` and token-call counters — the
cost of your extension per run (see the `perfscale-library-sdk` skill).
