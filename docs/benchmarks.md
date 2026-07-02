# Benchmarks

perfscale ships a [hyperfine](https://github.com/sharkdp/hyperfine)-based
benchmark script comparing up to five scenarios under an identical workload:

```sh
./scripts/bench.sh
```

| Scenario | What it runs |
|---|---|
| `locust (native)` | `locust` invoked directly — baseline |
| `k6 (native)` | `k6` invoked directly — baseline |
| `perfscale (k6)` | the same k6 script, via `perfscale run --k6` |
| `perfscale (locust)` | the same locustfile, via `perfscale run --locust` |
| `perfscale (yaml)` | perfscale's own step engine (no external binary) |

The `*-native` scenarios exist specifically to isolate **perfscale's wrapping
overhead** from **the underlying tool's own performance**: `perfscale (k6)`
and `k6 (native)` run the byte-identical generated script, so any gap between
them is temp-file handling, log piping, and summary translation — not k6.
Same pairing for `locust (native)` / `perfscale (locust)`.

## Methodology

- **Target**: a `perfscale serve` instance on loopback (`GET /health`) — no
  network noise, the same target for every scenario within a run.
- **Workload**: each scenario runs the configured VUs for the configured
  duration, same for every scenario.
- **Measurement**: [hyperfine](https://github.com/sharkdp/hyperfine) repeats
  each scenario (`--runs`, with a `--warmup`) and reports wall-time mean,
  min/max, and standard deviation — a statistically sound comparison instead
  of a single noisy sample.
- Scenarios whose engine (`k6`/`locust`) isn't on `PATH` are skipped, not
  failed.

## Reading the numbers

Since every scenario runs the same fixed-duration workload, the wall time is
mostly that duration plus fixed overhead — process startup, engine
initialization, and (for the `perfscale (*)` rows) perfscale's wrapping.
Compare scenarios within a single report; never compare across machines or
CI runs.

## Running locally

```sh
cargo build --release
./scripts/bench.sh                                   # all available scenarios, 10 VUs, 15s each, 5 runs
VUS=50 DURATION=30s ./scripts/bench.sh
RUNS=10 WARMUP=2 ./scripts/bench.sh                   # more hyperfine samples
OUTPUT=report.md ./scripts/bench.sh                   # write to a specific file
```

| Variable | Default | Description |
|---|---|---|
| `VUS` | `10` | Virtual users per scenario |
| `DURATION` | `15s` | Run length per scenario (`k6`/`locust` duration syntax) |
| `WARMUP` | `1` | hyperfine warmup runs (discarded, not shown) |
| `RUNS` | `5` | hyperfine measured runs per scenario |
| `PORT` | `18999` | Port for the throwaway `perfscale serve` target |
| `OUTPUT` | `bench-report.md` | Markdown report path |
| `PERFSCALE_BIN` | `target/release/perfscale` | Binary under test; builds it if missing |

## Running on CI (canonical)

The [`bench` workflow](../.github/workflows/bench.yml) runs on
`ubuntu-latest` — a fixed runner class, which removes local-machine variance:

- **manually**: GitHub → Actions → `bench` → *Run workflow* (inputs: `vus`,
  `duration`, `runs`)
- **weekly**: scheduled Monday 04:00 UTC as a perf-regression drift check

The job summary carries an Environment table (OS/CPU/threads/RAM/swap) and a
Software table (perfscale/k6/locust/hyperfine versions) ahead of the results,
so numbers are never read without knowing what produced them. The
`bench-report` artifact (kept 90 days) holds just the hyperfine table.

## Example report shape

```markdown
| Command | Mean [s] | Min [s] | Max [s] | Relative |
|:---|---:|---:|---:|---:|
| `locust (native)` | 2.358 ± 0.019 | 2.344 | 2.372 | 1.17 ± 0.01 |
| `perfscale (locust)` | 2.310 ± 0.031 | 2.288 | 2.332 | 1.15 ± 0.02 |
| `k6 (native)` | 2.751 ± 0.060 | 2.709 | 2.794 | 1.37 ± 0.03 |
| `perfscale (k6)` | 2.695 ± 0.097 | 2.626 | 2.764 | 1.34 ± 0.05 |
| `perfscale (yaml)` | 2.014 ± 0.007 | 2.009 | 2.019 | 1.00 |
```

`k6 (native)` vs `perfscale (k6)` above are close (perfscale's k6 wrapper is
thin — just temp-file writing and log piping), which is exactly what you
want to see: wrapping k6 doesn't cost you much extra. Watch the
`locust (native)` / `perfscale (locust)` pair the same way for locust's
wrapping cost.
