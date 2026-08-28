# JMeter load testing

perfscale runs your **existing JMeter test plans** (`.jmx`) headless and
folds the results into the same live log stream and k6-compatible summary
format as every other engine. There is nothing to rewrite: keep the plan
you already have, point perfscale at it, and get live output plus a
machine-readable summary you can diff against k6, locust, and native runs.

```sh
perfscale run --jmeter plan.jmx
```

## Prerequisites

A JMeter installation on the machine that runs the test — perfscale wraps
it, it does not embed it:

```sh
jmeter --version   # must be on PATH (requires a JRE)
```

If the binary is missing the run fails fast with
`jmeter not found in PATH — install from https://jmeter.apache.org/download_jmeter.cgi (requires a JRE)`.
On PerfScale Cloud agents the dev docker image already ships a JRE with
JMeter 5.6.3.

## How a run works

perfscale spawns the plan in non-GUI mode and streams everything live:

```text
jmeter -n -t <plan.jmx>
```

- **JMeter owns the load shape.** Thread groups, ramp-up, timers, and
  throughput all come from the plan. perfscale passes no `-J` properties,
  and perfscale run config (`vus`, `duration`, `stages:`) does not apply —
  the plan is the config.
- **Live output.** JMeter's own stdout/stderr stream through while the
  run is in progress, same as k6 or locust output.
- **Exit code.** JMeter exits non-zero on plan/startup errors; sample
  failures inside a plan do *not* change the exit code unless the plan
  itself gates on them. In CI the exit code is your plan-health gate.

## The translated summary

After the process exits, perfscale captures JMeter's final console line
(`summary = N in HH:MM:SS = R/s Avg: .. Min: .. Max: .. Err: .. (E%)`) and
translates it into the k6-compatible summary block every engine reports in:

```text
http_req_duration......: avg=4.00ms min=3.00ms max=72.00ms
http_req_failed........: 0.00%
http_reqs..............: 100 109.2/s
```

**No percentiles.** The console summary does not carry them, so
`http_req_duration` has `avg`/`min`/`max` only. If you need p95/p99, use
JMeter's own reporting (an HTML report or a Backend Listener) alongside the
run — see [Limits](#limits).

## Parameterizing plans

Since no `-J` properties are passed from outside, parameterize the plan
itself with JMeter's property functions and defaults:

```xml
<stringProp name="ThreadGroup.num_threads">${__P(vus,5)}</stringProp>
<stringProp name="LoopController.loops">${__P(loops,20)}</stringProp>
<stringProp name="HTTPSampler.domain">${__P(host,127.0.0.1)}</stringProp>
<stringProp name="HTTPSampler.port">${__P(port,8080)}</stringProp>
```

The defaults keep the plan runnable as-is; to vary the shape, wrap the
`jmeter` invocation in a script that passes `-Jvus=50 -Jloops=100`, or keep
several thin plan variants for different profiles.

## Running on an agent (PerfScale Cloud)

The same runner serves the platform: a JMeter-type test stores the plan
XML, the control plane dispatches it to an agent, and the agent executes it
via `POST /api/v1/run/jmeter` — streaming the log live and landing the
translated summary in the same dashboard and metrics pipeline as k6 runs.

## Limits

- **No percentiles** in the summary (avg/min/max only) — the JMeter
  console summary doesn't carry them.
- **No `std/thresholds@v1` gates** over JMeter runs — the exit code is the
  CI gate.
- **`.jtl` result files are not parsed** (throughput parsing of `.jtl` is
  future work).
- **No external parameterization** — perfscale config and `-J` properties
  are not applied; the plan owns the load shape.

Per-engine trade-offs at a glance live in the
[runners reference](runners.md#jmeter-runnerjmeter).
