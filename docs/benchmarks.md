# Benchmarks

perfscale ships a suite-based benchmark script comparing engines under an
identical workload:

```sh
./scripts/bench.sh                       # all suites
SUITES="throughput startup" ./scripts/bench.sh
```

Every scenario hits the same `perfscale serve` instance on loopback, so any
gap between a `perfscale (*)` row and its native counterpart is perfscale's
wrapping overhead — not the underlying tool.

| Scenario | What it runs |
|---|---|
| `locust (native)` | `locust` invoked directly — baseline |
| `k6 (native)` | `k6` invoked directly — baseline |
| `perfscale (k6)` | the same k6 script, via `perfscale run --k6` |
| `perfscale (locust)` | the same locustfile, via `perfscale run --locust` |
| `perfscale (yaml)` | perfscale's own step engine (no external binary) |
| `perfscale (yaml quiet)` | the step engine with `--quiet` — per-request logging suppressed; the delta against `perfscale (yaml)` is the logging cost |

## Suites

| Suite | Question it answers |
|---|---|
| `overhead` | Does wrapping add wall time at a fixed duration? (hyperfine, statistically averaged) |
| `throughput` | How many requests / what latency does each scenario actually deliver? Plus CPU, CPU-per-request, peak RSS, IO ops from the same instrumented run |
| `startup` | What does the wrapper cost at startup? (1s runs, where startup isn't drowned by the test duration) |
| `scaling` | How do RPS / p95 / RSS grow with VUs? (default sweep: 10, 50, 200) |
| `saturation` | Approximate max RPS per engine at high VUs (default 256) |
| `yaml` | What does each native-engine feature cost? (GET baseline vs `--quiet` vs +check vs POST body vs multi-step interpolation) |
| `tls` | The TLS tax: same workload against `perfscale serve --tls` (self-signed HTTPS, verification skipped) |

Select suites with `SUITES="..."`. Scenarios whose engine (`k6`/`locust`)
isn't on `PATH` are skipped, not failed; `overhead`/`startup` need
[hyperfine](https://github.com/sharkdp/hyperfine), the rest don't.

Micro-benchmarks for the native engine's hot paths (YAML parse, `${{ ... }}`
interpolation, metrics recording / percentile summary) live in
`crates/perfscale-core/benches/` and run with `cargo bench -p perfscale-core`.

## Methodology

- **Target**: a `perfscale serve` instance on loopback (`GET /health`) — no
  network noise, the same target for every scenario within a run. The `tls`
  suite adds a second `serve --tls` instance.
- **Wall-time comparison** (`overhead`, `startup`): hyperfine repeats each
  scenario (`--runs`, with a `--warmup`) and reports mean, min/max, and
  standard deviation.
- **Throughput/latency/resources**: one instrumented run per scenario under
  `/usr/bin/time` (`-v` on Linux, `-l` on macOS), with requests, RPS, and
  latency percentiles parsed from the engine's own summary (k6 text summary,
  locust `--csv` stats, perfscale's uniform summary lines). Single-run:
  directional, not statistically tight.
- **Beware fixed-duration wall time**: every scenario runs the same
  duration, so overhead-suite wall times are all ≈ the duration itself.
  Relative numbers near 1.00 prove the wrapper adds nothing; *throughput*
  numbers are where engines actually differ.
- **Shared CPU**: the load generator and `perfscale serve` share one
  machine. Saturation ceilings include the target's cost; if two engines
  plateau at similar RPS, suspect the target/CPU, not the engine.

## Running locally

```sh
cargo build --release
./scripts/bench.sh                                   # all suites, 10 VUs, 15s, 5 runs
VUS=50 DURATION=30s ./scripts/bench.sh
SUITES="yaml" YAML_DURATION=5s ./scripts/bench.sh     # just the native-engine suite
OUTPUT=report.md RESULTS=results.json ./scripts/bench.sh
```

| Variable | Default | Description |
|---|---|---|
| `SUITES` | all | Space-separated suite list (see table above) |
| `VUS` | `10` | Virtual users per scenario |
| `DURATION` | `15s` | Run length for `overhead`/`throughput` |
| `WARMUP` | `1` | hyperfine warmup runs (discarded, not shown) |
| `RUNS` | `5` | hyperfine measured runs per scenario |
| `STARTUP_DURATION` / `STARTUP_RUNS` | `1s` / `5` | Startup-suite run length / samples |
| `SCALING_VUS` / `SCALING_DURATION` | `10 50 200` / `10s` | VU sweep points / per-point length |
| `SAT_VUS` / `SAT_DURATION` | `256` / `15s` | Saturation VUs / run length |
| `YAML_DURATION` / `TLS_DURATION` | `10s` / `10s` | Per-scenario length in those suites |
| `PORT` / `TLS_PORT` | `18999` / `18998` | Ports for the throwaway serve targets |
| `OUTPUT` | `bench-report.md` | Markdown report path |
| `RESULTS` | `bench-results.json` | Machine-readable results (regression tracking input) |
| `PERFSCALE_BIN` | `target/release/perfscale` | Binary under test; builds it if missing |

## Running on CI (canonical)

The [`bench` workflow](../.github/workflows/bench.yml) runs on
`ubuntu-latest` — a fixed runner class, which removes local-machine variance:

- **manually**: GitHub → Actions → `bench` → *Run workflow* (inputs: `vus`,
  `duration`, `runs`, `suites`)
- **weekly**: scheduled Monday 04:00 UTC as a perf-regression drift check

The job summary carries an Environment table (OS/CPU/threads/RAM/swap) and a
Software table (perfscale/k6/locust/hyperfine versions) ahead of the results.
The `bench-report` artifact (kept 90 days) holds the markdown report plus
`bench-results.json`.

### Regression tracking

Each CI run downloads the previous successful run's `bench-results.json` and
appends a delta table (`scripts/bench_compare.py`) to the summary: RPS,
latency, RSS, startup overhead, and criterion means, with changes beyond
±15% flagged. Runners are shared hardware, so regressions **warn** rather
than fail the job — treat a flag as "look here", confirmed by re-running.

## Reading the numbers

- `overhead`: near-1.00 relative wall time = the wrapper is free at that
  duration. Real engine differences are in `throughput`.
- `throughput`: compare RPS and percentiles; `CPU per req` normalizes CPU
  cost by work actually done (an engine using 2× CPU while serving 10× RPS
  is *more* efficient, not less).
- `startup`: `vs native` is the wrapper's own cost; `vs ideal` includes the
  engine's startup too.
- Never compare across machines or CI runs — only within one report (the
  delta table compares across runs *on the same runner class*, which is as
  close as it gets).

### Reading `IO ops` (`in` / `out`)

`N in / M out` is the filesystem operation count from the `/usr/bin/time`
pass:

- **`in`** — read operations: blocks fetched *from* the filesystem (loading
  scripts/configs, paging in binaries). `0 in` is normal on a warm run —
  everything is already in the page cache. A cold first run shows non-zero
  `in` (k6 paging its binary in, for example).
- **`out`** — write operations: blocks written *to* the filesystem (temp
  scripts, locust's `--csv` stats, logs).

IO ops units differ by OS (GNU time counts fs blocks on Linux; BSD time
counts block operations on macOS), so the same number does **not** mean the
same bytes across systems — only compare within one report.
