# Benchmarks

perfscale ships a built-in benchmark comparing its three engines — the native
step engine, k6, and locust — under an identical scenario:

```sh
perfscale bench
```

## Methodology

- **Scenario**: `GET /` in a tight loop (no sleeps/wait_time), same VU count
  and duration for every engine. Scenarios are generated in code
  ([`commands/bench.rs`](../crates/perfscale-cli/src/commands/bench.rs)) so
  they cannot drift apart between engines.
- **Target**: an in-process axum server on loopback — no network noise, the
  same target for every engine within a run.
- **Sequential runs**: engines never compete with each other for CPU.
- **Report**: one markdown document per run with the host environment (OS,
  CPU, threads, RAM, swap) and software versions (perfscale, k6, locust) — so
  numbers are never compared blindly across machines.

## Reading the numbers

The benchmark measures **engine overhead against a shared trivial target**,
not absolute engine limits: at loopback speeds the differentiator is how much
CPU the load generator itself burns per request. Compare engines within a
single report; never compare across machines or runs.

Engines missing from `PATH` are reported as skipped rather than failing the
run.

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
./target/release/perfscale bench                          # all engines, 10 VUs, 15s each
./target/release/perfscale bench --vus 50 --duration 30s
./target/release/perfscale bench --engines native,k6      # skip locust
./target/release/perfscale bench --output report.md       # also write to a file
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
| Engine | Requests | RPS | avg | p50 | p90 | p95 | max | Failed |
| perfscale (native) | 377287 | 37728.05/s | 0.04ms | 0.00ms | 0.00ms | 0.00ms | 9.00ms | 0.00% |
| k6 | 477025 | 47701.58/s | 0.18ms | 0.15ms | 0.28ms | 0.37ms | 27.90ms | 0.00% |
| locust | 34674 | 3849.21/s | 1.86ms | 1.00ms | 3.00ms | 3.00ms | 40.00ms | 0.00% |
```
