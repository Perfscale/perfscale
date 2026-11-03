---
name: perfscale-migration-k6
description: Migrate k6 scripts and dashboards to perfscale — maps k6 concepts (vus/duration/stages, checks, thresholds, http metrics) to perfscale YAML and explains metric-name equivalences. Use when converting an existing k6 suite.
---

# Migrating from k6 to perfscale

perfscale can run existing k6 scripts as-is (`perfscale run --k6 script.js`),
so migration is incremental: keep running scripts with k6 while porting
scenarios to native YAML one at a time.

## Concept mapping

| k6 | perfscale |
|---|---|
| `options.vus` + `options.duration` | `vus:` + `duration:` in `config.yaml` |
| `options.stages` | `stages:` in `config.yaml` (`{ duration, target }`) |
| `options.scenarios` with `ramping-arrival-rate` | `arrival:` block (`max_vus`, `stages: [{ duration, rate }]`) |
| `http.get(url)` / `http.request(...)` | `use: std/http@v1` step with `with: { method, url, headers, body }` |
| `check(res, { 'status is 200': ... })` | step-level `check: { status: 200 }` |
| `sleep(1)` | `use: std/sleep@v1` with `{ ms: 1000 }` |
| `options.thresholds` | `std/thresholds@v1` step in `after:` with `severity: fail` |
| `group(name, fn)` | step `name:` labels (per-step metrics) |
| env vars (`__ENV.X`) | `${{ env.X }}` interpolation |
| k6 extensions (xk6) | WASM libraries via `imports:` (see perfscale-library-sdk) |

## Metric mapping

| k6 metric | perfscale metric |
|---|---|
| `http_req_duration` (p95 etc.) | `http_req_duration.p95` |
| `http_req_failed` rate | `errors.rate` |
| `http_reqs` | iteration/request counters in the summary |
| `vus` | `vus` gauge |
| `dropped_iterations` | `dropped_iterations` (arrival mode) |

`parse_summary` (MCP) and `--summary-export` understand the k6-compatible
output format, so existing CI parsers keep working.

## Porting workflow

1. Keep the k6 script running: `perfscale run --k6 script.js` — same binary,
   same summaries.
2. Port one scenario to `test.yaml` + `config.yaml`; model it on
   `tests/seed.yaml` if the repo was set up with `perfscale init-agents`.
3. Verify with a 1-VU short run: `perfscale run -f test.yaml -c config.yaml`.
4. Move `options.thresholds` into an `after: [std/thresholds@v1]` gate —
   same SLOs, now enforced by the exit code.
5. Delete the k6 script when the YAML twin holds the same SLOs.
