# CLI commands

```text
perfscale run          Run a load test with k6, locust, or the native step engine
perfscale serve        Start a local dev server that receives metrics from `run --report`
perfscale lint         Validate test/config YAML files without running them
perfscale self-update  Update perfscale to the latest release for this platform
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
  not a CLI error — mirroring k6's default behaviour without thresholds. This
  also covers engines that exit non-zero on failed *requests* (k6 with
  thresholds, locust) as long as they produced a results summary.
- `1` — the run could not execute: missing file, invalid YAML, engine binary
  not found, or the engine crashed before producing any results (non-zero
  exit with zero metrics — a script error or broken install, not test
  feedback): `error: engine exited with code 1 before producing any results`.
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

## Benchmarking

perfscale doesn't ship a `bench` subcommand — engine comparisons run through
[`scripts/bench.sh`](../../scripts/bench.sh), a [hyperfine](https://github.com/sharkdp/hyperfine)-based
script. See [Benchmarks](../benchmarks.md) for methodology and usage.

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

## `perfscale self-update`

Replaces the running binary with the latest
[GitHub release](https://github.com/Perfscale/perfscale/releases) for this
platform. The download's sha256 is verified against the release's
`sha256sums.txt` before the swap, and the swap itself is atomic (staged next
to the executable, then renamed) — a failed update never leaves a broken
binary behind.

```sh
perfscale self-update              # update to the latest release
perfscale self-update --check      # only check; exit 10 = update available
perfscale self-update --force      # reinstall even if already up to date
```

| Flag | Description |
|---|---|
| `--check` | Report whether an update exists without installing. Exit codes: `0` up to date, `10` update available — scriptable in cron/CI |
| `--force` | Reinstall the latest release even when versions match |

### The passive "update available" hint

Other commands (`run`, `serve`, `lint`) print a one-line stderr hint
when a newer release is known:

```text
perfscale v0.2.0 is available (you have 0.1.0) — run `perfscale self-update`
```

The check is deliberately unobtrusive: at most one network call per 24h
(cached in the user cache dir), only in interactive terminals (never in CI
logs or pipes), never delays a command by more than ~2s, and silent when
offline.

| Variable | Effect |
|---|---|
| `PERFSCALE_NO_UPDATE_CHECK=1` | Disable the passive check and hint entirely |
| `PERFSCALE_UPDATE_API_BASE` | Override the release API host (default `https://api.github.com`) — for mirrors/proxies |
| `PERFSCALE_UPDATE_DOWNLOAD_BASE` | Override the asset download host (default `https://github.com`) |

## Environment variables

| Name | Description |
|---|---|
| `RUST_LOG` | `tracing` filter, e.g. `RUST_LOG=debug perfscale run ...` |
| `PERFSCALE_NO_UPDATE_CHECK` | `1` disables the update-available hint |
