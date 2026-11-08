# JMeter load testing

perfscale runs your **existing JMeter test plans** (`.jmx`) headless and
folds the results into the same live log stream, k6-compatible summary,
thresholds gates, and reports as every other engine. There is nothing to
rewrite: keep the plan you already have, point perfscale at it, and get live
output plus machine-readable metrics you can diff against k6, locust, and
native runs.

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

perfscale spawns the plan in non-GUI mode with a private results file and
streams everything live:

```text
jmeter -n -t <plan.jmx> -j <temp>/jmeter.log -l <temp>/results.jtl -J…
```

- **JMeter owns the load shape.** Thread groups, ramp-up, timers, and
  throughput all come from the plan. perfscale run config `vus`, `duration`,
  `stages:`, `arrival:` do not apply (they are rejected with a clear error,
  not silently ignored) — parameterize the plan with [`-J`
  properties](#parameterizing-plans) instead.
- **Deterministic CSV results.** Every run writes samples to a private
  `results.jtl` in CSV format with a fixed field set (`timeStamp`, `elapsed`,
  `label`, `success`, plus `Latency`/`Connect`/`bytes`/`sentBytes` when the
  plan produces them). The forced saveservice `-J` properties are appended
  after yours, so a user property can never break the parser. Both temp
  files are removed when the run ends.
- **Live output.** JMeter's own stdout/stderr stream through while the run
  is in progress, same as k6 or locust output.
- **Exit code.** JMeter exits non-zero on plan/startup errors; sample
  failures inside a plan do *not* change the exit code unless the plan
  itself gates on them. perfscale adds two failure sources: a results file
  with no trustworthy samples (invalid header or every row malformed) and a
  violated `severity: fail` thresholds gate both exit 1.
  In CI the exit code is your plan-health + SLO gate.

## The JTL-derived summary

The results file is parsed incrementally (bounded memory — samples fold into
HDR histograms, never a list) into the same metrics store the native engine
uses. The summary carries full percentiles:

```text
http_req_duration......: avg=4.12ms p(50)=3.00ms p(90)=6.00ms p(95)=8.00ms p(99)=21.00ms min=1.00ms max=72.00ms
http_req_failed........: 0.00%
http_reqs..............: 100 10.83/s
http_req_latency: avg=2.01ms p(50)=2.00ms … count=100
http_req_connect: avg=0.83ms p(50)=1.00ms … count=100
data_received: 102400 1109.00/s
data_sent: 51200 554.50/s
```

Metrics are aggregated **per JMeter `label`** as well as globally — the
label series land in the [`report@v1` document](#reports) and in during-run
streaming snapshots. Distinct labels are capped (64 by default); beyond the
cap, samples fold into a `__jmeter_other__` series and a `system` line says
how many — cardinality never grows silently.

A plan with zero samples (e.g. an empty thread group) falls back to
translating JMeter's final console `summary =` line (`avg`/`min`/`max`
only). Malformed rows are skipped with a `system` warning while trustworthy
samples exist; a file with an invalid header or *only* malformed rows is a
run failure, never silently passed.

## Parameterizing plans

Pass JMeter properties straight from the CLI — repeatable, values may
contain `=`:

```sh
perfscale run --jmeter plan.jmx -Jvus=50 -Jloops=100 --jmeter-property host=staging.example.com
```

Reference them in the plan with property functions and defaults:

```xml
<stringProp name="ThreadGroup.num_threads">${__P(vus,5)}</stringProp>
<stringProp name="LoopController.loops">${__P(loops,20)}</stringProp>
<stringProp name="HTTPSampler.domain">${__P(host,127.0.0.1)}</stringProp>
```

Property values are never printed to logs. The runner's own saveservice
settings always win over user properties.

## Thresholds and config

`perfscale run --jmeter plan.jmx -c config.yaml` applies the config's
`report:` block and `after:` `std/thresholds@v1` gates:

```yaml
report:
  url: http://localhost:7999
  during_run: true            # stream live snapshots off the .jtl
  reporters:
    - use: std/report/junit@v1
      with: { path: junit.xml }
after:
  - use: std/thresholds@v1
    severity: fail            # fail (default) | warn | info
    message: checkout SLO
    with:
      http_req_duration: ["p95<500", "p99<1500"]
      http_req_failed: ["rate<0.01"]
```

Gates evaluate over the JTL metrics (including `http_req_latency`,
`http_req_connect`, `data_received`, `data_sent`) after the run; a violated
`severity: fail` gate exits non-zero, exactly like the native engine.
`abort_on_fail` is **not** supported for JMeter (jmeter owns the run, so the
gate can only evaluate afterwards) and is rejected with a clear error, as
are other `after:` actions, `before:` steps, and native-only config blocks.

## Reports

End-of-run reporters work for JMeter runs — the runner emits the canonical
`report@v1` document built from the real metrics, so JSON/JUnit/HTML reports
include percentiles, per-label series, and threshold verdicts:

```sh
perfscale run --jmeter plan.jmx --report json:report.json --report junit:junit.xml --label testid=smoke
```

With `report.during_run: true` (and a `report.url` or streaming reporters),
cumulative metric snapshots stream off the `.jtl` while the run is in
progress — the same pipeline the native engine feeds.

## Running on an agent (PerfScale Cloud)

The same runner serves the platform: a JMeter-type test stores the plan
XML, the control plane dispatches it to an agent, and the agent executes it
via `POST /api/v1/run/jmeter` — streaming the log live and landing the
summary in the same dashboard and metrics pipeline as k6 runs.

## Limits

- **Load shape lives in the plan.** perfscale `vus`/`duration`/`stages:`/
  `arrival:` do not apply (rejected, not ignored); use `-J` properties.
- **No `abort_on_fail`** mid-run abort — gates evaluate after jmeter exits.
- Labels containing embedded newlines can confuse the line-oriented live
  tail; the final parse still counts them via CSV quoting rules.

Per-engine trade-offs at a glance live in the
[runners reference](runners.md#jmeter-runnerjmeter).
