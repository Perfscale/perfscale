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
| `step::actions` | Built-in actions (`std/http`, `std/check`, `std/sleep`, `std/log`) |
| `step::context` | Per-VU variable store + `${{ }}` interpolation |
| `yaml` | Schema-validated parsing of test/config files, `ConfigFile` |
| `schema` | JSON Schema generation (schemars) for both file formats |
| `models` | `RunResult` (oneshot subprocess result) |

## Embedding example

```rust
use perfscale_core::runner::{self, ExecutionPlan};
use perfscale_core::yaml;

let test = yaml::parse_test_file(&std::fs::read_to_string("test.yaml")?)?;
let config = yaml::parse_config_file(&std::fs::read_to_string("config.yaml")?)?;

let mut rx = runner::execute(ExecutionPlan::NativeSteps { test, config: config.run }).await?;
while let Some(line) = rx.recv().await {
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
