# Architecture

`perfscale-core` is a library crate: everything the CLI does is available to
any Rust program that wants to embed a load-testing engine.

```text
                        ┌─────────────────────────────────────┐
 CLI flags ──────────►  │            ExecutionPlan            │
                        │  K6Script | LocustScript | Native   │
                        └──────────────────┬──────────────────┘
                                           │ runner::execute(plan)
              ┌────────────────────────────┼────────────────────────────┐
              ▼                            ▼                            ▼
      runner::k6                   runner::locust               step::runner
   spawn `k6 run`,             spawn `locust --headless`,    N tokio tasks (VUs)
   stream stdout/err           stream + parse CSV stats      loop over steps
              │                            │                            │
              └────────────────────────────┴────────────────────────────┘
                                           │
                                           ▼
                          mpsc::Receiver<LogLine>
                     { source: stdout|stderr|system, text }
```

## The one abstraction that matters: `LogLine`

Every engine — external subprocess or in-process — reduces to the same output
type:

```rust
pub struct LogLine {
    pub source: LogSource,   // Stdout | Stderr | System
    pub text: String,
}
```

Consumers (the CLI, `perfscale serve`, a future TUI) never care which engine
produced a line. The stream closes when the run finishes — there is no
separate completion signal.

## Unified summary format

All three engines end their stream with the same k6-style summary block
(`http_req_duration`, `http_req_failed`, `http_reqs`, `vus`, `iterations`),
so downstream parsers are engine-agnostic:

- the native engine formats it from its own collected metrics
  (`step::runner::Metrics::summary_lines`)
- the locust runner builds it from locust's `--csv` stats file
- k6 prints it natively

## Module map

| Module | Responsibility |
|---|---|
| `runner` | `ExecutionPlan`, `execute()` dispatcher, `LogLine`/`LogSource` |
| `runner::k6` | k6 subprocess: temp-script handling, streaming, oneshot |
| `runner::locust` | locust subprocess: headless flags, CSV → summary conversion |
| `step` | Test model: `TestDef`, `Step`, `RunConfig`, duration parsing, presets |
| `step::runner` | Native VU scheduler and metrics collection |
| `step::actions` | Built-in action dispatch (`std/*`) + custom-action registry |
| `step::context` | Per-VU variable store + `${{ }}` interpolation |
| `step::resources` | Family handle types + gRPC reflection cache over the connection registries |
| `step::ws` / `step::grpc` / `step::db` | Live-connection protocol families |
| `step::thresholds` | `std/thresholds@v1` run-level SLO gates |
| `step::process` | Managed child processes (`std/child_process`, `std/kill_process`) |
| `yaml` | Schema-validated parsing of test/config files, `ConfigFile` |
| `schema` | JSON Schema generation (schemars) for both file formats |
| `models` | `RunResult` (oneshot subprocess result) |

Sibling workspace crate: **`perfscale-connection`** — the generic
named-connection registry (`Connection` trait + `ConnectionRegistry`) that
`step::resources` builds on. Zero dependencies; usable by any step engine
with the connect → park → use → close lifecycle.

## Native engine pipeline

A native run flows through these pieces in order:

```text
 test.yaml + config.yaml (step::yaml, schema-validated)
        │  steps, before/after, vars, run config (vus, duration)
        ▼
 step::runner::run_native ── before: steps once (outputs → ${{ config.* }})
        │
        │  spawn config.vus tokio tasks, each loops until duration expires:
        ▼
 step::context::Context     per VU: vars + ${{ }} interpolation,
        │                   per-VU generator, HTTP client shard
        ▼
 step::actions::execute_action   per step, strictly sequential:
        │   std/http·tcp·udp·ws*·grpc*·db-*·check·sleep·log·file-*·…
        ▼
 step::resources            live handles parked under Connection IDs
        │   (ws-1, grpc-1, grpcs-1, db-1) via perfscale-connection;
        │   drained after every iteration — nothing outlives it
        ▼
 step::runner::Metrics      HDR histograms (fixed memory), counters,
        │                   rates, threshold results
        ▼
 summary + thresholds       k6-compatible text summary streamed as
        │                   LogLines; NativeRunOutcome.thresholds
        ▼
 CLI exit code              non-zero when a severity:fail gate trips
```

Where things plug in:

- **k6 / locust** are sibling *engines*, not steps: `runner::execute`
  dispatches the `ExecutionPlan` to their subprocess runners, which reduce
  to the same `LogLine` stream (diagram above).
- **gRPC / WebSocket / DB families** plug in as `std/*` actions in
  `step::actions`, with their live handles parked in `step::resources`
  (backed by the `perfscale-connection` crate) — connect steps mint the
  `ws-1` / `grpc-1` / `db-1` ids users reference in later steps.
- **Thresholds** run as `std/thresholds@v1` steps in `after:`, evaluating
  gates over the run's collected metrics (`step::thresholds`); the combined
  result becomes the CLI exit code.
- **Custom actions** (e.g. downstream `pro/*`) register an `ActionHandler`
  via `step::actions::register_action` and can use `perfscale-connection`
  for their own parked handles.

Key files, one line each:

| File | Role |
|---|---|
| `crates/perfscale-core/src/runner/mod.rs` | Engine dispatch, `LogLine` |
| `crates/perfscale-core/src/step/runner.rs` | VU loop, HDR metrics, summary, exit-code source |
| `crates/perfscale-core/src/step/context.rs` | `${{ }}` interpolation, per-VU state |
| `crates/perfscale-core/src/step/actions.rs` | `std/*` action dispatch |
| `crates/perfscale-core/src/step/resources.rs` | Family handle types, registry glue |
| `crates/perfscale-connection/src/lib.rs` | The connection-registry pattern, documented |
| `crates/perfscale-cli/src/main.rs` | CLI: parse args, run plan, print stream, exit code |

## Embedding example

```rust
use perfscale_core::runner::{self, ExecutionPlan};
use perfscale_core::yaml;

let test = yaml::parse_test_file(&std::fs::read_to_string("test.yaml")?)?;
let config = yaml::parse_config_file(&std::fs::read_to_string("config.yaml")?)?;

let rx = runner::execute(ExecutionPlan::NativeSteps {
    test,
    config: config.run,
    before: config.before,
    after: config.after,
    variables: config.variables,
    quiet: false,
})
.await?;
while let Some(line) = rx.lines.recv().await {
    println!("[{:?}] {}", line.source, line.text);
}
```

## Design constraints

- **No proprietary integrations.** Everything here is generic; control-plane
  concerns (auth, metric push, fleet management) belong to downstream
  consumers of this crate.
- **External engines are subprocesses, not linked.** k6 and locust are found
  on `PATH` at run time; a missing binary is a friendly error, not a build
  dependency.
- **Bounded channels (512 lines).** Producers block when a consumer stalls —
  drain the receiver concurrently with the run (as `execute()` does), never
  after it.
