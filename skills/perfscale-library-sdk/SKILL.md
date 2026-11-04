---
name: perfscale-library-sdk
description: Extend perfscale with the official SDK (@perfscale/library-sdk for TypeScript, perfscale-library-sdk for Rust) — value generators for ${alias.fn(...)} tokens, custom steps for use: lib/<alias>/<step> with their own metrics and checks, trusted process steps, config extensions, and end-of-run reporters. Use when a std/* action or built-in token does not cover the protocol, data, metric or report you need.
---

# perfscale SDK — extending the engine

One WASM component can carry four kinds of extension. Pick by what the
scenario needs:

| Need | Build | Used from YAML as | Reference |
|---|---|---|---|
| A computed value inside a payload (ID, signature, fake data) | `defineLibrary` — `functions` | `${alias.fn(args)}` in any string | [libraries.md](references/libraries.md) |
| A whole step with its own logic, outputs, metrics and checks | `defineSteps` — `steps` | `use: lib/<alias>/<step>` | [steps.md](references/steps.md) |
| Running a local tool/runtime (node, python, CLI) as a step | `defineSteps` + `ctx.exec()`, build `--process` | `use: lib/<alias>/<step>` + `capabilities: [process]` | [steps.md](references/steps.md#process-steps) |
| Library-owned settings in `config.yaml` | `configSchema` on the definition | a top-level `config.yaml` key | [libraries.md](references/libraries.md#config-extensions) |
| A custom end-of-run report/file/integration | `defineReporter` | `report.reporters: [{ use: wasm:./x.wasm }]` | [reporters.md](references/reporters.md) |

Value functions and steps live in the **same** component: a signing function
`${sig.token(...)}` and the login step `lib/sig/login` that uses it ship as
one `.wasm`, one build, one `libraries:` entry.

## Golden path (TypeScript)

```console
$ npm install @perfscale/library-sdk
$ npx perfscale-library-build mylib.ts -o mylib.wasm --kind steps   # library | steps | reporter
$ perfscale lint test.yaml config.yaml                             # validates lib/ refs + config block
$ perfscale run -f test.yaml -c config.yaml
```

```yaml
# config.yaml
libraries:
  - use: ./mylib.wasm
    capabilities: []        # required key; [] = no grants
    as: mylib               # optional alias (default: the library's name)
```

Unit-test without WASM under `node:test`: `testCall(def, new Ctx(), "fn",
[args])` for functions and `testStep(def, new StepCtx(), "step", params)`
for steps.

## Rules that bite

- Libraries are declared in `libraries:` (config file). `import:` is YAML
  document composition, **not** library loading.
- Steps are addressed `lib/<alias>/<step>` — `lib/` is reserved and never
  shadows `std/*` / `pro/*`; `perfscale lint` checks every reference against
  the component's manifest before the run.
- Steps get **no network egress**. Protocol I/O stays with `std/*` / `pro/*`
  steps; a WASM step transforms, signs, correlates, asserts and emits
  metrics — or runs a host process through the `process` capability.
- Any non-empty `capabilities:` grant needs `allow_library_capabilities:
  true` in the config; `process` additionally `allow_process_actions: true`.
- Metric names a step emits are `[a-z0-9_]` and must not collide with
  engine series (`http_req_duration`, registered families) — a collision
  fails the step naming the metric.
- `${{ … }}` interpolation (secrets resolved and log-masked) happens
  **before** the component is called; the component sees plain values.
- Randomness: use the seeded `ctx.rng()` — never `Math.random()` — so a
  `seed:` run reproduces bit-for-bit.

## Distribution

`perfscale install` fetches remote components into the content-addressed
cache and pins digests in `perfscale.lock`; it also pre-compiles (burns)
them so runs deserialize native code instead of recompiling. See
[distribution.md](references/distribution.md). Templates to start from:
`Perfscale/library-template` (functions), `Perfscale/faker` (Rust data
generator), `Perfscale/nodejs` (process steps).

After a run, analyze what your extension did with the
`perfscale-run-analysis` skill — custom step cost shows in the summary's
`libraries:` line (`step_calls`, `step_errors`, `step_p95_ms`).
