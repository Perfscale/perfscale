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
| `jmeter (native)` | `jmeter -n -t` invoked directly — baseline (overhead/startup suites only) |
| `perfscale (k6)` | the same k6 script, via `perfscale run --k6` |
| `perfscale (locust)` | the same locustfile, via `perfscale run --locust` |
| `perfscale (jmeter)` | the same `.jmx` plan, via `perfscale run --jmeter` (overhead/startup suites only) |
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
| `ws` | WebSocket message throughput and RTT: same-connection echo round-trips against `serve`'s `/ws` endpoint (`perfscale (yaml)`, `k6 (native)`, `perfscale (k6)` — locust has no built-in WebSocket support and is skipped) |
| `boundary` | Where does the engine break? Climbs a ladder of VU ceilings (`BOUNDARY_LADDER`, default `5000 15000 30000`): each rung ramps 0→ceiling over `BOUNDARY_DURATION`; the boundary is the VU level / time / RPS where the cumulative error rate first reaches `BOUNDARY_ERR_PCT` percent (default 1%) — or the rung where the process dies (OOM/crash counts as the boundary). An engine that crossed stops climbing |
| `tls` | The TLS tax: same workload against `perfscale serve --tls` (self-signed HTTPS, verification skipped) |
| `library` | WASM value-generator cost: per-run compile (cold) vs burn-cache deserialize vs the `@std/random` builtin |
| `grpc` | `std/grpc@v1` unary echo against the repo's `grpc_echo_server` example |
| `graphql` | `std/graphql@v1` query against the repo's `graphql_server` example |
| `db` | SQL: cold connect+query vs warm persistent pool vs transactions (`std/db-*@v1`, PostgreSQL + in-process SQLite), plus the warm shape from locust (psycopg2), JMeter (JDBC) and k6 — the k6 row is a **custom binary** built with the `xk6-sql` extension + postgres driver (stock k6 has no SQL module), used only by this row |

JMeter joins only the hyperfine suites (`overhead`/`startup`): its non-GUI
console summary has no percentiles, so it stays out of the `throughput`
table. Parsing its `.jtl` result files for full throughput metrics is future
work.

Select suites with `SUITES="..."`. Scenarios whose engine (`k6`/`locust`/
`jmeter`) isn't on `PATH` are skipped, not failed; `overhead`/`startup` need
[hyperfine](https://github.com/sharkdp/hyperfine), the rest don't.

The `boundary` suite detects the crossing differently per engine: k6 aborts
itself via an `abortOnFail` threshold (the peak-VU progress line is the
boundary), locust is read from its `--csv-full-history` Aggregated rows,
jmeter from accumulated `summary +` deltas (run with
`-Jsummariser.interval=2`, heap raised via `BOUNDARY_JMETER_HEAP`), and the
native engine from its periodic `[stats]` lines — which is why the native
boundary row runs without `--quiet` and includes the per-request logging
cost. A non-zero engine exit without a detected crossing (OOM, crash) is
recorded as the boundary at that rung with a "process died" note. Wrapped
locust/jmeter rows are omitted: the wrapper is a pass-through, the boundary
is an engine property. `BOUNDARY_MAX_VUS`, when set, replaces the ladder with
a single rung (backwards-compatible override). Between runs the suite probes
the target with 50 fresh connections and waits out any loopback TIME_WAIT
drain left by the previous engine (the CI runner also gets a widened
`ip_local_port_range` + `tcp_tw_reuse`) — a boundary must be the engine's,
not the machine's exhausted ephemeral ports.

The `db` suite separates the cold path (`perfscale (cold)`,
`perfscale (cold, params)`: connect + one
query per iteration, connection dropped at iteration end) from the warm path
(`perfscale (warm pool, …)`: one persistent pool per iteration,
`DB_WARM_QUERIES` queries,
`db-close`; `perfscale (warm tx, …)`: `DB_TX_CYCLES` × begin+query+commit) and
adds an
in-process SQLite floor (`perfscale (sqlite, …)`, no server needed).
The locust row (`pipx inject locust psycopg2-binary`), the JMeter row
(PostgreSQL JDBC jar in JMeter's `lib/`, `BENCH_PG_JDBC_JAR` to point
elsewhere), and the k6 row repeat the warm shape. The `k6 (xk6-sql)` row is
**not stock k6**: stock k6 has no SQL module, so the bench workflow builds a
separate binary with `xk6 build` (xk6-sql extension + postgres driver, all
versions pinned, binary cached; `K6_SQL_BIN`, default
`target/xk6-sql/k6`). Its k6 core version may differ from the stock one — the
CI environment summary prints both. The custom binary serves only this row;
every other k6 scenario keeps using the stock binary.

Micro-benchmarks for the native engine's hot paths (YAML parse, `${{ ... }}`
interpolation, metrics recording / percentile summary, `RingBuf` process-output
capture, `waitUntil` readiness matchers) live in
`crates/perfscale-core/benches/` and run with `cargo bench -p perfscale-core`.

## GPU inference (local only)

A separate suite for benchmarking **GPU-backed LLM inference** (Ollama /
vLLM via `std/llm@v1`, with `gpu:` run metrics — TTFT, tokens/sec, GPU
utilization/VRAM) lives in [`bench/gpu/`](../bench/gpu/README.md). It is
deliberately not part of the CI workflow above: hosted runners have no GPU,
so the suite runs on a local machine with an NVIDIA card.

## Methodology

- **Target**: a `perfscale serve` instance on loopback (`GET /health`; the
  `ws` suite uses its `GET /ws` WebSocket echo endpoint) — no network noise,
  the same target for every scenario within a run. The `tls`
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
| `LIB_DURATION` / `GRPC_DURATION` / `GQL_DURATION` | `10s` each | Per-scenario length in the library / grpc / graphql suites |
| `WS_DURATION` / `WS_ROUNDS` | `10s` / `10` | ws-suite run length / echo round-trips per connection |
| `BOUNDARY_DURATION` / `BOUNDARY_LADDER` / `BOUNDARY_ERR_PCT` | `30s` / `5000 15000 30000` / `1` | boundary-suite per-rung ramp length / VU ceilings / error-rate threshold in percent |
| `BOUNDARY_MAX_VUS` | unset | Single-rung override for `BOUNDARY_LADDER` (legacy spelling) |
| `BOUNDARY_JMETER_HEAP` | `-Xms512m -Xmx4g` | JMeter heap on the high-VU boundary rungs |
| `DB_DURATION` / `DB_WARM_QUERIES` / `DB_TX_CYCLES` | `10s` / `10` / `5` | db-suite per-scenario length / queries per warm iteration / tx cycles per iteration |
| `BENCH_PG_DSN` | `postgres://postgres:perfscale@127.0.0.1:5432/postgres` | PostgreSQL DSN for the db suite |
| `BENCH_PG_JDBC_JAR` | autodetected from JMeter's `lib/` | PostgreSQL JDBC driver jar for the JMeter db row |
| `K6_SQL_BIN` | `target/xk6-sql/k6` | Custom k6 with the xk6-sql extension for the db-suite k6 row (built by the bench workflow) |
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
Software table (perfscale/k6/locust/jmeter/java/hyperfine versions) ahead of
the results.
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
