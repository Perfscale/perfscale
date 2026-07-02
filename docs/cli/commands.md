# CLI commands

```text
perfscale run    Run a load test with k6, locust, or the native step engine
perfscale serve  Start a local dev server that receives metrics from `run --report`
perfscale bench  Benchmark the engines against each other, markdown report
perfscale lint   Validate test/config YAML files without running them
```

## `perfscale run`

Exactly one engine flag is required: `--k6`, `--locust`, or `-f`.

| Flag | Value | Description |
|---|---|---|
| `--k6 <FILE>` | `.js` script | Run via the `k6` binary on `PATH` |
| `--locust <FILE>` | locustfile | Run via the `locust` binary on `PATH`, headless |
| `-f, --file <FILE>` | `test.yaml` | Run with the built-in native engine (requires `-c`) |
| `-c, --config <FILE>` | `config.yaml` | Load config: `vus`, `duration`, optional `report.url`. Required with `-f`, optional load hint for `--locust`, ignored by `--k6` |
| `--host <URL>` | base URL | Target host for `--locust` (locust's `--host`) |
| `--report <URL>` | base URL | POST the summary to a `perfscale serve` instance after the run; overrides `report.url` from the config file |

### Exit code semantics

- `0` — the run completed, **even if requests or checks failed**. Failed
  checks are load-test feedback (visible in `http_req_failed` and stderr),
  not a CLI error — mirroring k6's default behaviour without thresholds.
- `1` — the run could not execute: missing file, invalid YAML, engine binary
  not found, engine crashed.
- `2` — invalid command-line arguments.

### Output streams

- **stdout** — engine output and the final metric summary (machine-friendly)
- **stderr** — errors, failed checks, and `[system]` progress markers

### Engine availability errors

```text
error: k6 not found in PATH — install from https://k6.io/docs/get-started/installation/
error: locust not found in PATH — install with `pip install locust` (...)
```

## `perfscale serve`

| Flag | Default | Description |
|---|---|---|
| `--port <PORT>` | `7999` | Port to listen on; `0` picks a free port (printed at startup) |

Endpoints:

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Returns `ok` |
| `POST` | `/api/v1/metrics` | Accepts `{"lines": ["...", ...]}` and prints the batch |

This is a development stand-in, not a control-plane: no persistence, no auth,
no aggregation across runs. Anything that speaks these two endpoints can
replace it.

## `perfscale bench`

Runs the same tight `GET` loop through each selected engine — sequentially,
against an in-process loopback target — and prints a markdown report with the
host environment (OS, CPU, threads, RAM, swap), software versions (perfscale,
k6, locust), and per-engine results (requests, RPS, avg/p50/p90/p95/max,
failure rate). See [Benchmarks](../benchmarks.md) for methodology.

| Flag | Default | Description |
|---|---|---|
| `--vus <N>` | `10` | Virtual users per engine |
| `--duration <D>` | `15s` | Run length per engine (`"30s"`, `"1m"`, ...) |
| `--engines <LIST>` | `native,k6,locust` | Comma-separated subset to run |
| `--output <FILE>` | — | Also write the report to a file |

Engines missing from `PATH` are reported as skipped, not errors. The
canonical comparison runs on CI (`bench` workflow) to remove local-machine
variance.

## `perfscale lint`

Validate YAML files against the same schemas `run` uses — plus checks a
schema can't express — without executing anything. Made for editors, CI
gates, and pre-commit hooks.

```sh
perfscale lint test.yaml config.yaml
perfscale lint --schema config load.yaml
```

| Flag | Default | Description |
|---|---|---|
| `FILE...` | required | One or more YAML files |
| `--schema <auto\|test\|config>` | `auto` | `auto` detects per file: a top-level `steps:` key means test definition, anything else is a config |

What it checks:

1. **YAML syntax** — parse errors with an indentation/quoting hint
2. **JSON Schema** — required fields, types (same validation `run` performs)
3. **Unknown/typo'd fields** — with did-you-mean suggestions (`chek` → `check`),
   at every level: step fields, per-action `with:` parameters, `check:` keys,
   config fields, `report:` fields
4. **Unknown action IDs** — `std/htp@v1` → `did you mean 'std/http@v1'?`

Every finding shows *where* (`/steps/0/with`), *what*, and *what to use
instead*, and the output ends with a link to the
[YAML reference](../yaml-reference.md).

Exit code: `0` when every file is valid, `1` otherwise — safe to use as a CI
gate:

```yaml
- run: perfscale lint tests/*.yaml
```

## Environment variables

| Name | Description |
|---|---|
| `RUST_LOG` | `tracing` filter, e.g. `RUST_LOG=debug perfscale run ...` |
