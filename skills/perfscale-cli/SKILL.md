---
name: perfscale-cli
description: Author, lint, and run perfscale load tests (test.yaml + config.yaml pairs) with the native step engine, k6, locust, or JMeter. Use when creating or editing perfscale YAML, choosing load shapes, setting SLO thresholds, or picking reporters.
---

# perfscale CLI

perfscale runs load tests from one CLI. The native engine takes two files:

- `test.yaml` — the scenario: a `steps:` list of actions (`use: std/http@v1`, …).
- `config.yaml` — the run shape: `vus`, `duration`, `stages`, `arrival`, thresholds, reporters.

## Commands

```sh
perfscale lint test.yaml config.yaml        # validate without running (typo + action-ID checks)
perfscale run -f test.yaml -c config.yaml   # native engine run
perfscale schema test                       # JSON Schema for test YAML (IDE autocomplete)
perfscale schema config                     # JSON Schema for config YAML
perfscale metrics                           # every registered metric: name, kind, unit
```

The run exits 0 even when checks fail (that is load-test feedback); it exits 1
when the run itself cannot execute. A `std/thresholds@v1` step with
`severity: fail` gates the exit code — violated thresholds make the run exit
non-zero.

## Minimal example

```yaml
# test.yaml
steps:
  - name: homepage
    use: std/http@v1
    with:
      method: GET
      url: https://api.example.com/health
    check:
      status: 200
    outputs: resp
```

```yaml
# config.yaml
vus: 1
duration: 10s
```

Run: `perfscale run -f test.yaml -c config.yaml`

## References

- [references/yaml-reference.md](references/yaml-reference.md) — full test/config field reference
- [references/actions.md](references/actions.md) — the `std/*` action catalog
- [references/load-shapes.md](references/load-shapes.md) — `vus`/`duration`, `stages`, `arrival`
- [references/thresholds.md](references/thresholds.md) — SLO gates with `std/thresholds@v1`
- [references/metrics.md](references/metrics.md) — metric names, kinds, units
- [references/reporters.md](references/reporters.md) — end-of-run and streaming reporters
- [references/protocols.md](references/protocols.md) — HTTP, GraphQL, gRPC, WebSocket, SQL, pub/sub, TCP/UDP

## Rules for agents

- Default every verification run to **1 VU and a short duration** (e.g.
  `vus: 1`, `duration: 5s`). Never run full load without an explicit user
  request or the threshold workflow.
- Lint after every write: a test that fails `perfscale lint` must not be run.
