# YAML reference

Two files drive a native run: the **test definition** (`-f`) describes *what*
to do; the **config** (`-c`) describes *how much load* to apply.

Both are validated against JSON Schemas before execution — errors point at
the offending field path, not a raw parser dump. The schemas live in
[`schema/`](../schema/) and can drive editor autocomplete via a modeline:

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/Perfscale/perfscale/main/schema/test.schema.json
```

## Test definition (`-f test.yaml`)

A single `steps` array. Each virtual user (VU) executes the whole list in a
loop until the configured duration expires.

```yaml
steps:
  - name: login                     # optional label used in log output
    use: std/http@v1                # required — action ID
    with:                           # action parameters (see below)
      method: POST
      url: https://api.example.com/login
      body:
        user: demo
    check:                          # optional inline assertions
      status: 200
      duration_ms_lt: 500
    outputs: login                  # optional — store output for later steps
```

### Step fields

| Field | Required | Description |
|---|---|---|
| `use` | yes | Action ID: `std/http@v1`, `std/check@v1`, `std/sleep@v1`, `std/log@v1` (short aliases `http`, `check`, `sleep`, `log` also work) |
| `name` | no | Human-readable label shown in log lines |
| `with` | no | Action parameters — see [Actions](core/actions.md) |
| `check` | no | Assertions on this step's output — same keys as `std/check@v1` |
| `outputs` | no | Variable name to store the step output under |

### Variable interpolation

Any string in `with`/`check` may reference previous step outputs with
`${{ name }}` or `${{ name.field }}`:

```yaml
steps:
  - use: std/http@v1
    with: { url: "https://api.example.com/token" }
    outputs: auth
  - use: std/http@v1
    with:
      url: "https://api.example.com/me"
      headers:
        authorization: "Bearer ${{ auth.body }}"
```

Missing variables resolve to an empty string. The output of the previous step
is always additionally available as `__last__` (used when `check` has no `on`).

## Config (`-c config.yaml`)

```yaml
vus: 10          # virtual users, default 1
duration: 5m     # "30s", "1m", "5m30s", "1h" — default "1m"

report:          # optional — forward the summary after the run
  url: http://localhost:7999
```

| Field | Default | Description |
|---|---|---|
| `vus` | `1` | Concurrent virtual users |
| `duration` | `1m` | Wall-clock run length; bare numbers are seconds |
| `report.url` | — | A `perfscale serve` base URL; the CLI `--report` flag overrides it |

With `--locust`, the same config maps to locust's `--users`/`--spawn-rate`/`--run-time`.
With `--k6`, load config lives in the script's own `options` block and the
config file is ignored.

## Validating without running: `perfscale lint`

Check files ahead of time — in CI, pre-commit hooks, or while writing them:

```sh
perfscale lint test.yaml config.yaml
```

Beyond schema validation, `lint` flags unknown and typo'd field names with
did-you-mean suggestions (`chek` → `check`, `vsu` → `vus`, `std/htp@v1` →
`std/http@v1`), including per-action `with:` parameters. See
[CLI commands → lint](cli/commands.md#perfscale-lint).

## Validation errors

perfscale validates before running. Examples of what you'll see:

```text
error: schema validation failed:
  /steps/0 — "use" is a required property
```

```text
error: invalid YAML: found unexpected end of stream
```

Regenerate the schemas after changing the Rust types:

```sh
cargo run -p perfscale-core --example gen_schema
```

(CI's `shipped_schemas_match_generated_ones` test fails if `schema/` goes stale.)
