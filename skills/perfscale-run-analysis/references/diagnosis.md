# Diagnosis playbook — symptom → cause → next step

## RPS plateaus while p95 climbs

The target (or something in front of it) is saturated. Confirm with an
arrival run at a rate just above the plateau: `dropped_iterations` > 0 and a
growing `engine_arrival_queue_wait_ms` mean capacity, not the tool. Check
`engine_scheduler_lag_ms` is flat — if it grows too, the generator is the
limit (move to a bigger machine or split load).

## RPS plateaus, p95 flat and low, errors 0

Classic closed-model ceiling: every VU is busy waiting. Add VUs (`stages`)
or switch to `arrival:` to drive a rate instead of a concurrency.

## Dropped iterations

`dropped_iterations` > 0 in an arrival run: an iteration was due and all
`max_vus` workers were busy. Either raise `max_vus` (if the target is
fine and latency × rate > pool size) or treat the rate where drops start as
the capacity. Since v0.29.0 drops only happen on a genuinely busy pool.

## p50 fine, p99/max huge

Tail latency: GC pauses, connection setup, queueing on the target. Look at
`[stats]` lines over time (cumulative percentiles converge; a sudden jump
marks the moment). For connection setup: one-shot steps (`std/grpc@v1`,
cold DB connects) pay a connect per call — compare with VU-scoped
connections (`scope: vu`, live channels) before blaming the target.

## Connection and port errors

`Can't assign requested address` / `EADDRNOTAVAIL`, connect timeouts with
the target idle: the generator ran out of ephemeral ports to one
endpoint (~28–64k per source IP). Options: keep connections alive
(`pool: dedicated` for "N VUs = N connections"), reuse connections
(`scope: vu`), or spread over several source addresses
(`std/http@v1` `source_ips: [...]`). Between connection-churning runs on
one machine, wait for TIME_WAIT sockets to drain (`ss -tan state
time-wait | wc -l`).

## Errors only under load

Run with `abort_on_fail: true` + `http_debug: error` on the gate: the
first failures are logged with VU, iteration, interpolated request and
response body (masked), and the run stops early instead of hammering a
broken target.

## Latency looks too good

Closed model hiding coordinated omission (see SKILL.md §2), or the step
isn't what you think: `http_req_duration` excludes gRPC/DB/WS-recv/sleep
steps by design. For multi-step flows look at the per-step aggregates in
`report@v1` (`steps[]`) or the step-level `labels:` in streaming metrics.

## Logging slows the run

`engine_log_channel_depth_max` near 512 or the run is CPU-bound on a chatty
scenario: add `--quiet` (keeps error lines and the summary). Under `--quiet`
successful HTTP/GraphQL requests don't even format their log line.

## Comparing runs

- **Same machine class.** Cloud CI runners differ by CPU generation; a
  uniform ~1.5× shift in metrics whose code did not change is hardware.
  Record the CPU model with the result.
- **Same profile and duration**, warm-up first (JIT/connection pools of
  the target), several alternating A/B rounds — single runs are noise.
- **Same generator posture**: `--quiet` vs not, keep-alive vs connect
  per call, open vs closed model.
- Diff `--summary-export` JSONs (`summary.p95_ms`, `requests_per_sec`,
  `error_rate`) or `report@v1` `steps[]` for per-step changes.
- CPU per request = (user + sys CPU of the generator) / requests — the
  fairest cross-tool efficiency number; peak RSS for memory.
