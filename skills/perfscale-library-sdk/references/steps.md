# Custom steps (RFC 009, `perfscale:step@0.1.0`)

A step component exports actions dispatched from `use: lib/<alias>/<step>`.
The engine calls it with the interpolated `with:` block and a context; the
return value folds into `outputs:`, `check:`, thresholds and the summary
exactly like a built-in step.

## Minimal step (TypeScript)

```ts
import { defineSteps, StepError } from "@perfscale/library-sdk";

export default defineSteps({
  name: "echo",
  version: "0.1.0",
  steps: {
    echo: {
      description: "Echo the `message` param into outputs and the log",
      run(params, ctx) {
        const message = String(params.message ?? "");
        if (!message) throw new StepError("echo: `message` is required");
        return {
          output: { message, vu: Number(ctx.vuId), iteration: Number(ctx.iterationSeq) },
          logs: [{ tag: "out", line: `${ctx.stepName}: ${message}` }],
          metrics: { echo_len: message.length },
        };
      },
    },
  },
  functions: {
    shout: { description: "Upper-case the argument", call: (argv) => String(argv[0] ?? "").toUpperCase() },
  },
});
```

```console
$ npx perfscale-library-build echo.ts -o echo.wasm --kind steps
```

```yaml
# config.yaml
libraries:
  - use: ./echo.wasm
    capabilities: []
before:                       # steps also run in before:/after: (vu = iteration = 0)
  - name: setup
    use: lib/echo/echo
    with: { message: starting }
    outputs: setup            # → ${{ config.setup.message }} in test steps

# test.yaml
steps:
  - name: say
    use: lib/echo/echo
    with:
      message: "hello from vu ${{ __vu__ }} — ${echo.shout(echo)}"
    outputs: said
    check:
      message_contains: hello
```

## What `run(params, ctx)` gets

| `ctx` field | Meaning |
|---|---|
| `vuId`, `iterationSeq` | bigint; `0` in `before:` / `after:` |
| `timeMs` | wall clock, unix ms — the only time source a step has |
| `stepName` | the YAML step's `name:` |
| `settings` | run settings frozen at start (vus, duration, seed, stages/arrival, variables, `extensions`) |
| `exec(req)` | host process spawn — only when built with `--process` (see below) |

`params` is the `with:` block after `${{ }}` interpolation and `${…}` token
expansion.

## What it returns (`StepResult`)

| Field | Effect |
|---|---|
| `success` (default `true`) | `false` (or a thrown error) = a failed step in every report, like a failed `std/http` |
| `output` | stored under the step's `outputs:`; `check:` asserts on it |
| `logs` | `[{ tag: "out" \| "err" \| "sys", line }]`; `--quiet` keeps only `err` |
| `metrics` | `name → number` = counter, `name → [ms, …]` = duration samples; a `<name>_duration` / `<name>_rtt` histogram also mints `<name>_failed` (rate) |
| `httpSample` | `{ durationMs, status, failed }` → counted in `http_req_duration` / `http_req_failed` |
| `secretOutputPaths` | output keys the engine masks in logs (tokens, session ids) |

Metric names: lowercase `[a-z0-9_]`, no collision with engine-owned series.
They are usable in `std/thresholds@v1` like any built-in metric:

```yaml
after:
  - use: std/thresholds@v1
    with:
      echo_len: ["count>0"]        # a counter: only `count` applies
      sign_duration: ["p95<5"]     # a sample metric, ms
      sign_failed: ["rate<0.01"]   # derived from sign_duration
```

## Process steps

For trusted local tools (a runtime, a CLI, a generator binary) build with
`--process` and call `ctx.exec()`:

```ts
run(params, ctx) {
  const r = ctx.exec({
    executable: "/usr/bin/python3",
    argv: ["-c", String(params.code)],
    env: { LANG: "C.UTF-8" },      // replacement env — host vars are NOT inherited
    timeoutMs: 10_000,
  });
  return { success: r.exitCode === 0, output: { stdout: r.stdout, code: r.exitCode } };
}
```

Triple opt-in, fail-closed — missing any one and the component won't load:

```yaml
allow_library_capabilities: true
allow_process_actions: true
libraries:
  - use: ./mytool.wasm
    capabilities: [process]
```

No shell is involved (direct argv), output is bounded, the timeout is host
capped and the process tree is cleaned up. A spawn is a real process: use
process steps for setup, fixtures and orchestration, not for a hot path at
thousands of RPS. Reference implementation: `Perfscale/nodejs`
(`lib/nodejs/install` → checksum-verified download, `exec`, `install-exec`).

## Testing

```ts
import { StepCtx, testStep } from "@perfscale/library-sdk";
import def from "./echo.ts";

const out = testStep(def, new StepCtx(), "echo", { message: "hi" }); // no WASM
// out.success === true, out.output.message === "hi"
```

For process steps pass a fake host: `new StepCtx((req) => ({ exitCode: 0,
stdout: "ok", stderr: "" }))` — the fake receives the raw WIT request
(`env` as `[key, value]` pairs).

## Rust

The engine fixtures `crates/perfscale-library-sdk/examples/steps` and
`stepexec` show the same contract with raw `wit-bindgen` against
`wit/step/step.wit`; the `info()` manifest lists `steps: [{name,
description}]` and must agree with the exported ABI or the component fails
to load.

## Cost visibility

The run summary's `libraries:` line reports per component `step_calls`,
`step_errors`, `step_p95_ms` next to token-call counters — check it after a
run to see what the step costs per iteration.
