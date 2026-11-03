# Load shapes

Exactly one load shape per `config.yaml`.

## Closed model: `vus` + `duration`

A fixed pool of VUs loops the scenario for the duration. Each VU starts the
next iteration only after the previous one finishes, so a slow system lowers
throughput.

```yaml
vus: 10
duration: 5m
```

Use `iterations: N` instead of (or with) `duration` to cap total work.

## Ramping VUs: `stages`

```yaml
stages:
  - { duration: 30s, target: 0 }    # start point
  - { duration: 2m,  target: 50 }   # ramp to 50 VUs
  - { duration: 5m,  target: 50 }   # hold
  - { duration: 30s, target: 0 }    # ramp down
```

## Open model: `arrival`

Hold a target iterations/sec rate; a worker pool grows lazily up to
`max_vus` to keep up even when the system slows down. Permits arriving while
all workers are busy are dropped and counted in the `dropped_iterations`
summary metric.

```yaml
arrival:
  max_vus: 100
  pre_allocated_vus: 2
  stages:
    - { duration: 15s, rate: 10 }   # ramp 0→10 iterations/sec
    - { duration: 30s, rate: 10 }   # hold
    - { duration: 15s, rate: 0 }    # ramp down
```

Arrival rate is the right shape for "will we hold 500 rps?" questions: the
offered load stays constant regardless of server health, so queueing shows up
as latency, not as reduced load.

## Choosing

- Smoke / correctness: `vus: 1`, `duration: 5–10s`.
- Steady-state capacity: `arrival` at the target rate.
- Spike / soak: `stages` with sharp ramps or long holds.
