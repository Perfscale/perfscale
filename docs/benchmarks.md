# Benchmarks

perfscale ships a built-in benchmark comparing five scenarios under an
identical workload:

```sh
perfscale bench
```

| Scenario | What it runs |
|---|---|
| `locust-native` | `locust` invoked directly — baseline |
| `k6-native` | `k6` invoked directly — baseline |
| `perfscale-k6` | the same k6 script, via `perfscale run --k6` |
| `perfscale-locust` | the same locustfile, via `perfscale run --locust` |
| `perfscale-yaml` | perfscale's own step engine (no external binary) |

The `*-native` rows exist specifically to isolate **perfscale's wrapping
overhead** from **the underlying tool's own performance**: `perfscale-k6` and
`k6-native` run the byte-identical generated script, so any gap between them
is temp-file handling, log piping, and summary translation — not k6. Same
pairing for `locust-native` / `perfscale-locust`.

## Methodology

- **Scenario**: `GET /` in a tight loop (no sleeps/wait_time), same VU count
  and duration for every scenario. Generated in code
  ([`commands/bench.rs`](../crates/perfscale-cli/src/commands/bench.rs)) so
  the native and perfscale-wrapped variants of each engine can't drift apart.
- **Target**: an in-process axum server on loopback — no network noise, the
  same target for every scenario within a run.
- **Sequential runs**: scenarios never compete with each other for CPU.
- **Report**: one markdown document per run with the host environment (OS,
  CPU, threads, RAM, swap) and software versions (perfscale, k6, locust) — so
  numbers are never compared blindly across machines.

The report has two tables: **Results** (throughput/latency) and **Resource
usage** (CPU/memory/disk IO) — speed alone doesn't tell you what a load
generator costs to run, and the two don't always agree (see below).

## Reading the numbers

The benchmark measures **overhead against a shared trivial target**, not
absolute engine limits: at loopback speeds the differentiator is how much CPU
the load generator (and perfscale's wrapper, where applicable) burns per
request. Compare scenarios within a single report; never compare across
machines or runs.

Resource usage is sampled by polling the engine's process every
~[`MINIMUM_CPU_UPDATE_INTERVAL`](https://docs.rs/sysinfo) (currently 200ms) —
CPU avg/max are computed from those samples, and peak memory / disk IO are
read from the same poll, so brief spikes between samples can be missed. A
scenario shorter than one interval shows no data. `perfscale-yaml` has no
child process to attribute cost to, so it measures perfscale's own process —
which also includes the in-process bench target serving its requests, a cost
every other scenario's requests pass through too, just paid by a separate
process there and so not counted against them.

Scenarios whose engine is missing from `PATH` are reported as skipped rather
than failing the run.

## Running on CI (canonical)

The [`bench` workflow](../.github/workflows/bench.yml) runs on
`ubuntu-latest` — a fixed runner class, which removes local-machine variance:

- **manually**: GitHub → Actions → `bench` → *Run workflow* (inputs: `vus`,
  `duration`)
- **weekly**: scheduled Monday 04:00 UTC as a perf-regression drift check

Results appear in the workflow's job summary and as a `bench-report` artifact
(kept 90 days).

## Running locally

```sh
cargo build --release
./target/release/perfscale bench                                    # all 5 scenarios, 10 VUs, 15s each
./target/release/perfscale bench --vus 50 --duration 30s
./target/release/perfscale bench --engines k6-native,perfscale-k6   # just the k6 comparison
./target/release/perfscale bench --output report.md                 # also write to a file
```

## Example report shape

```markdown
## Environment
| OS | Ubuntu 24.04.1 LTS (x86_64) |
| CPU | AMD EPYC 7763 |
| Threads | 4 |
| RAM | 15.6 GiB |
| Swap | 4.0 GiB |

## Software
| perfscale | 0.1.0 |
| k6 | k6 v1.5.0 |
| locust | locust 2.32 |

## Results
| Engine | Version | Requests | RPS | avg | p50 | p90 | p95 | max | Failed |
| locust (native) | locust 2.44.4 | 3961 | 3961.17/s | 0.60ms | 1.00ms | 1.00ms | 1.00ms | 6.00ms | 0.00% |
| k6 (native) | k6 v1.5.0 | 42428 | 21211.51/s | 0.12ms | 0.10ms | 0.18ms | 0.23ms | 5.90ms | 0.00% |
| perfscale (k6) | k6 v1.5.0 | 42435 | 21214.94/s | 0.12ms | 0.10ms | 0.17ms | 0.23ms | 18.18ms | 0.00% |
| perfscale (locust) | locust 2.44.4 | 1252 | 1251.96/s | 1.79ms | 1.00ms | 2.00ms | 3.00ms | 275.00ms | 0.00% |
| perfscale (yaml) | 0.2.0 | 17709 | 8853.47/s | 0.04ms | 0.00ms | 0.00ms | 0.00ms | 58.00ms | 0.00% |

## Resource usage
| Engine | CPU avg | CPU max | Peak memory | Disk read | Disk written |
| locust (native) | 35.4% (8.9% of 4 cores) | 99.3% (24.8% of 4 cores) | 54 MiB | 264 KiB | 4 KiB |
| k6 (native) | 3.7% (0.9% of 4 cores) | 5.0% (1.3% of 4 cores) | 68 MiB | 6 MiB | 0 B |
| perfscale (k6) | 3.9% (1.0% of 4 cores) | 5.1% (1.3% of 4 cores) | 72 MiB | 224 KiB | 0 B |
| perfscale (locust) | 35.6% (8.9% of 4 cores) | 98.6% (24.7% of 4 cores) | 54 MiB | 264 KiB | 8 KiB |
| perfscale (yaml) | 5.5% (1.4% of 4 cores) | 6.5% (1.6% of 4 cores) | 22 MiB | 52 KiB | 16 KiB |
```

The first CPU figure is the raw per-core percentage `sysinfo`/`top` report (a
multi-threaded process can exceed 100%); the parenthetical normalizes it
against every logical core on the host, for an at-a-glance "how much of the
whole machine" reading.

The `Version` column makes each row self-contained — no need to cross-reference
the Software section above to know exactly which k6/locust/perfscale build
produced a given number.

`k6-native` vs `perfscale-k6` above are near-identical on both tables
(perfscale's k6 wrapper is thin — just temp-file writing and log piping),
which is exactly what you want to see: wrapping k6 doesn't cost you anything
extra. `locust-native` vs `perfscale-locust` show a real throughput gap —
that's the cost of piping locust's stdout/stderr through an internal channel
and re-parsing its CSV output — but their resource cost is nearly identical,
so the gap is latency/overhead in the pipe, not extra CPU/memory burned.

Independent of the perfscale-vs-native comparison, the Resource usage table
also answers a different question: locust costs roughly **10x the CPU** of k6
for the same workload here (35% vs 3.7%) — a genuinely useful signal when
choosing an engine, that the Results table's RPS numbers alone don't surface.
