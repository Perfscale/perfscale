# YAML reference

perfscale native tests are a `test.yaml` (scenario) plus a `config.yaml` (run
shape). Validate any file with `perfscale lint <file>`; print the exact schema
with `perfscale schema test|config`.

## test.yaml

Top-level keys:

| Key | Required | Meaning |
|---|---|---|
| `steps` | yes | Ordered list of steps |
| `imports` | no | Library imports (local path, https, git) resolved via `perfscale.lock` |
| `variables` | no | Run-level variables for `${{ variables.* }}` interpolation |
| `setup` | no | Steps run once before the load starts |
| `after` | no | Steps run once after the load (home of `std/thresholds@v1` gates) |

Step fields:

| Field | Required | Meaning |
|---|---|---|
| `use` | yes | Action ID, e.g. `std/http@v1` (short alias `http` works; `uses:` is an alias) |
| `name` | no | Label for logs and per-step metrics |
| `with` | no | Action inputs (per-action schema) |
| `check` | no | Assertions on the step result, e.g. `status: 200`, `duration_ms_lt: 500` |
| `outputs` | no | Variable name the step result binds to, referenced as `${{ name.* }}` |
| `severity` | no | `std/thresholds@v1` only: `fail` (default), `warn`, `info` |
| `skip` | no | Skip the step/test, optionally with a reason string |

Interpolation: `${{ outputs.* }}`, `${{ variables.* }}`, `${{ inputs.* }}`,
`${{ env.* }}` resolve before the step runs.

## config.yaml

| Key | Meaning |
|---|---|
| `vus` | Number of concurrent virtual users (closed model) |
| `duration` | Run length, e.g. `30s`, `10m` |
| `stages` | Ramping VU profile: `- { duration: 30s, target: 20 }` |
| `arrival` | Open model: `max_vus`, `pre_allocated_vus`, `co_correct` (queue instead of drop — CO-corrected latency), `stages: [{ duration, rate }]` (iterations/sec) |
| `report` | `url` (perfscale serve) and/or `reporters` (streaming sinks) |
| `iterations` | Cap total iterations instead of/alongside duration |

Exactly one load shape per config: `vus`+`duration`, `stages`, or `arrival`.

Run: `perfscale run -f test.yaml -c config.yaml`. Add
`--report json:report.json` / `junit:junit.xml` / `html:report.html` for
end-of-run artifacts, `--summary-export result.json` for the machine-readable
summary.
