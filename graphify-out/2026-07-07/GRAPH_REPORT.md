# Graph Report - perfscale  (2026-07-07)

## Corpus Check
- 49 files · ~37,913 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 891 nodes · 1451 edges · 57 communities (46 shown, 11 thin omitted)
- Extraction: 98% EXTRACTED · 2% INFERRED · 0% AMBIGUOUS · INFERRED: 24 edges (avg confidence: 0.83)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `264c5c01`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- [[_COMMUNITY_Step Actions (httpchecklogsleep)|Step Actions (http/check/log/sleep)]]
- [[_COMMUNITY_CLI Parser & Commands|CLI Parser & Commands]]
- [[_COMMUNITY_Runner Output & LogLine Stream|Runner Output & LogLine Stream]]
- [[_COMMUNITY_Docs, Examples & Schemas|Docs, Examples & Schemas]]
- [[_COMMUNITY_CLI Arg Parsing & Lint Tests|CLI Arg Parsing & Lint Tests]]
- [[_COMMUNITY_Runner Config & Output Structs|Runner Config & Output Structs]]
- [[_COMMUNITY_Step Runner Core|Step Runner Core]]
- [[_COMMUNITY_Run Command Internals|Run Command Internals]]
- [[_COMMUNITY_Self-Update Version & Artifacts|Self-Update Version & Artifacts]]
- [[_COMMUNITY_Lint Engine (did-you-mean)|Lint Engine (did-you-mean)]]
- [[_COMMUNITY_CLI Integration Tests|CLI Integration Tests]]
- [[_COMMUNITY_YAML Parsing|YAML Parsing]]
- [[_COMMUNITY_Locust Runner Options|Locust Runner Options]]
- [[_COMMUNITY_E2E Workflow Tests|E2E Workflow Tests]]
- [[_COMMUNITY_Context Interpolation|Context Interpolation]]
- [[_COMMUNITY_CliError Formatting|CliError Formatting]]
- [[_COMMUNITY_Serve HTTP Endpoints|Serve HTTP Endpoints]]
- [[_COMMUNITY_Test Schema Definitions|Test Schema Definitions]]
- [[_COMMUNITY_Self-Update Integration Tests|Self-Update Integration Tests]]
- [[_COMMUNITY_Self-Update DownloadVerifySwap|Self-Update Download/Verify/Swap]]
- [[_COMMUNITY_Lint File Processing|Lint File Processing]]
- [[_COMMUNITY_End-to-End Tests|End-to-End Tests]]
- [[_COMMUNITY_SchemaYAML Integration Tests|Schema/YAML Integration Tests]]
- [[_COMMUNITY_ReportConfig Schema|ReportConfig Schema]]
- [[_COMMUNITY_Schema Generation|Schema Generation]]
- [[_COMMUNITY_Schema Generation Tests|Schema Generation Tests]]
- [[_COMMUNITY_Config Schema Properties|Config Schema Properties]]
- [[_COMMUNITY_VUs Schema Property|VUs Schema Property]]
- [[_COMMUNITY_Steps Schema|Steps Schema]]
- [[_COMMUNITY_Models RunResult|Models RunResult]]
- [[_COMMUNITY_Locust Example|Locust Example]]
- [[_COMMUNITY_Claude Settings Hooks|Claude Settings Hooks]]
- [[_COMMUNITY_Lint Core Issues|Lint Core Issues]]
- [[_COMMUNITY_Benchmark Script|Benchmark Script]]
- [[_COMMUNITY_k6 Example|k6 Example]]
- [[_COMMUNITY_Edit-Distance Suggest|Edit-Distance Suggest]]
- [[_COMMUNITY_Graphify Hook & Skill|Graphify Hook & Skill]]
- [[_COMMUNITY_Repo Commit Rules|Repo Commit Rules]]
- [[_COMMUNITY_No-Proprietary Constraint|No-Proprietary Constraint]]
- [[_COMMUNITY_runnerexecute Re-export|runner::execute Re-export]]
- [[_COMMUNITY_detect_kind|detect_kind]]
- [[_COMMUNITY_Community 45|Community 45]]
- [[_COMMUNITY_Community 46|Community 46]]
- [[_COMMUNITY_Community 47|Community 47]]
- [[_COMMUNITY_Community 48|Community 48]]
- [[_COMMUNITY_Community 49|Community 49]]
- [[_COMMUNITY_Community 50|Community 50]]
- [[_COMMUNITY_Community 51|Community 51]]
- [[_COMMUNITY_Community 52|Community 52]]
- [[_COMMUNITY_Community 53|Community 53]]
- [[_COMMUNITY_Community 54|Community 54]]
- [[_COMMUNITY_Community 55|Community 55]]
- [[_COMMUNITY_Community 56|Community 56]]

## God Nodes (most connected - your core abstractions)
1. `execute_action()` - 38 edges
2. `cmd()` - 24 edges
3. `lint()` - 17 edges
4. `run_steps()` - 17 edges
5. `What You Must Do When Invoked` - 16 edges
6. `run_streaming()` - 15 edges
7. `/graphify` - 15 edges
8. `execute()` - 14 edges
9. `execute_step()` - 14 edges
10. `run_streaming()` - 13 edges

## Surprising Connections (you probably didn't know these)
- `hello.k6.js example script` --references--> `k6 runner`  [EXTRACTED]
  examples/hello.k6.js → docs/core/runners.md
- `hello.locust.py example (HelloUser)` --references--> `locust runner`  [EXTRACTED]
  examples/hello.locust.py → docs/core/runners.md
- `Test definition (test.yaml)` --shares_data_with--> `TestDef schema`  [EXTRACTED]
  docs/yaml-reference.md → schema/test.schema.json
- `hello.config.yaml example` --shares_data_with--> `ConfigFile schema`  [EXTRACTED]
  examples/hello.config.yaml → schema/config.schema.json
- `Config (config.yaml)` --shares_data_with--> `ConfigFile schema`  [EXTRACTED]
  docs/yaml-reference.md → schema/config.schema.json

## Import Cycles
- 1-file cycle: `crates/perfscale-cli/src/cli.rs -> crates/perfscale-cli/src/cli.rs`
- 1-file cycle: `crates/perfscale-cli/src/commands/lint.rs -> crates/perfscale-cli/src/commands/lint.rs`
- 1-file cycle: `crates/perfscale-cli/src/commands/run.rs -> crates/perfscale-cli/src/commands/run.rs`
- 1-file cycle: `crates/perfscale-cli/src/update.rs -> crates/perfscale-cli/src/update.rs`
- 1-file cycle: `crates/perfscale-cli/src/commands/self_update.rs -> crates/perfscale-cli/src/commands/self_update.rs`
- 1-file cycle: `crates/perfscale-cli/src/commands/serve.rs -> crates/perfscale-cli/src/commands/serve.rs`
- 1-file cycle: `crates/perfscale-core/benches/engine.rs -> crates/perfscale-core/benches/engine.rs`
- 1-file cycle: `crates/perfscale-core/src/step/actions.rs -> crates/perfscale-core/src/step/actions.rs`
- 1-file cycle: `crates/perfscale-core/src/step/context.rs -> crates/perfscale-core/src/step/context.rs`
- 1-file cycle: `crates/perfscale-core/src/step/runner.rs -> crates/perfscale-core/src/step/runner.rs`
- 1-file cycle: `crates/perfscale-cli/tests/cli.rs -> crates/perfscale-cli/tests/cli.rs`
- 1-file cycle: `crates/perfscale-cli/tests/self_update.rs -> crates/perfscale-cli/tests/self_update.rs`
- 1-file cycle: `crates/perfscale-core/src/lint.rs -> crates/perfscale-core/src/lint.rs`
- 1-file cycle: `crates/perfscale-core/src/runner/k6.rs -> crates/perfscale-core/src/runner/k6.rs`
- 1-file cycle: `crates/perfscale-core/src/runner/locust.rs -> crates/perfscale-core/src/runner/locust.rs`

## Hyperedges (group relationships)
- **Three engines, one LogLine interface** — k6_runner, locust_runner, native_step_engine, log_line, unified_summary [EXTRACTED 1.00]
- **Native engine built-in action set** — action_std_http, action_std_check, action_std_sleep, action_std_log, native_step_engine [EXTRACTED 1.00]
- **Benchmark comparison flow** — scripts_bench_sh, workflows_bench_yml, benchmarks_methodology, wrapping_overhead, serve_health_endpoint [EXTRACTED 0.85]
- **run to serve metric reporting loop** — run_reportsummary, serve_ingest, serve_metricspayload, run_issummaryline [INFERRED 0.85]
- **self-update download-verify-swap pipeline** — self_update_selfupdate, self_update_download, self_update_verifydigest, self_update_replaceexecutable [EXTRACTED 0.75]
- **run command engine plan dispatch** — run_run, run_resolveplan, cli_runargs [EXTRACTED 0.75]
- **Built-in std step actions dispatched by execute_action** — step_actions_http_action, step_actions_check_action, step_actions_sleep_action, step_actions_log_action, step_actions_execute_action [EXTRACTED 1.00]
- **Three load-test engines unified behind execute** — runner_k6_run_streaming, runner_locust_run_streaming, step_runner_run_steps, runner_mod_execute [INFERRED 0.85]
- **YAML parse + schema validation + lint flow** — yaml_parse_with_schema, schema_test_schema, schema_config_schema, lint_lint [INFERRED 0.85]

## Communities (57 total, 11 thin omitted)

### Community 0 - "Step Actions (http/check/log/sleep)"
Cohesion: 0.08
Nodes (50): Client, Context, Error, HttpSample, LogTag, Option, String, Value (+42 more)

### Community 1 - "CLI Parser & Commands"
Cohesion: 0.05
Nodes (53): Atomic self-update binary swap pattern, Cli root parser, Benchmarking, CLI commands, Engine availability errors, Environment variables, Exit code semantics, Output streams (+45 more)

### Community 2 - "Runner Output & LogLine Stream"
Cohesion: 0.10
Nodes (36): Unified LogLine output stream, Child, Error, PathBuf, Result, RunOutput, String, Option (+28 more)

### Community 3 - "Docs, Examples & Schemas"
Cohesion: 0.08
Nodes (39): std/check@v1 action, std/http@v1 action, std/log@v1 action, std/sleep@v1 action, Benchmark methodology (hyperfine), ConfigFile schema, ReportConfig schema, External engines as subprocesses constraint (+31 more)

### Community 4 - "CLI Arg Parsing & Lint Tests"
Cohesion: 0.08
Nodes (28): Commands, Error, Option, PathBuf, Result, String, Vec, Cli (+20 more)

### Community 5 - "Runner Config & Output Structs"
Cohesion: 0.12
Nodes (31): Child, Default, Error, Option, Path, PathBuf, Result, RunConfig (+23 more)

### Community 6 - "Step Runner Core"
Cohesion: 0.12
Nodes (28): Arc, k6-compatible summary format, Context, HttpSample, LogLine, LogTag, RunConfig, Self (+20 more)

### Community 7 - "Run Command Internals"
Cohesion: 0.16
Nodes (29): base_args(), is_summary_line(), load_config(), load_test_def(), print_line(), report_summary(), resolve_plan(), resolve_plan_locust_with_config_maps_vus_to_users() (+21 more)

### Community 8 - "Self-Update Version & Artifacts"
Cohesion: 0.11
Nodes (23): Option, PathBuf, Result, String, Duration, api_base(), artifact_for(), asset_url() (+15 more)

### Community 9 - "Lint Engine (did-you-mean)"
Cohesion: 0.10
Nodes (39): effective_kind(), kind_label(), lint_file(), print_issues(), run(), CliError, Path, Result (+31 more)

### Community 10 - "CLI Integration Tests"
Cohesion: 0.14
Nodes (25): Command, cmd(), errors_carry_hint_and_docs_sections(), help_flag_lists_all_commands(), k6_available(), lint_missing_file_is_a_cli_error_with_hint(), lint_missing_use_shows_fix_with_action_list(), lint_schema_override_forces_config_validation() (+17 more)

### Community 11 - "YAML Parsing"
Cohesion: 0.14
Nodes (23): Option, Result, RunConfig, String, TestDef, Value, ConfigFile, empty_config_file_uses_run_config_defaults() (+15 more)

### Community 12 - "Locust Runner Options"
Cohesion: 0.12
Nodes (9): Option, Self, String, Value, default_duration(), default_vus(), preset_config(), run_config_default_is_one_vu_one_minute() (+1 more)

### Community 13 - "E2E Workflow Tests"
Cohesion: 0.14
Nodes (20): BufReader, ChildStdout, Child, Self, String, Vec, Drop, NamedTempFile (+12 more)

### Community 14 - "Context Interpolation"
Cohesion: 0.19
Nodes (16): Self, String, Value, HashMap, Context, interpolate_field(), interpolate_missing_is_empty(), interpolate_multiple() (+8 more)

### Community 15 - "CliError Formatting"
Cohesion: 0.20
Nodes (14): Option, Result, Self, String, Display, Formatter, Into, CliError (+6 more)

### Community 16 - "Serve HTTP Endpoints"
Cohesion: 0.10
Nodes (29): bench_interpolate(), bench_metrics(), bench_yaml_parse(), app(), health_route_rejects_post(), health_route_returns_ok(), ingest(), metrics_route_accepts_empty_lines() (+21 more)

### Community 17 - "Test Schema Definitions"
Cohesion: 0.11
Nodes (19): description, definitions, Step, description, type, description, type, check (+11 more)

### Community 18 - "Self-Update Integration Tests"
Cohesion: 0.27
Nodes (17): Command, PathBuf, String, MockServer, TempDir, binary_copy(), mock_release(), platform_artifact() (+9 more)

### Community 19 - "Self-Update Download/Verify/Swap"
Cohesion: 0.28
Nodes (15): download(), replace_executable(), replace_executable_swaps_contents_atomically(), self_update(), staged_path(), staged_path_is_next_to_exe(), verify_digest(), verify_digest_accepts_matching_and_rejects_mismatched() (+7 more)

### Community 20 - "Lint File Processing"
Cohesion: 0.06
Nodes (35): For --cluster-only, For git commit hook, For /graphify add, For /graphify explain, For /graphify path, For /graphify query, For native CLAUDE.md integration, For --update (incremental re-extraction) (+27 more)

### Community 21 - "End-to-End Tests"
Cohesion: 0.23
Nodes (10): LogLine, RunOutput, String, Vec, collect(), failing_backend_shows_up_in_error_rate_and_check_failures(), k6_script_against_backend_reports_success(), stdout_text() (+2 more)

### Community 22 - "Schema/YAML Integration Tests"
Cohesion: 0.19
Nodes (12): Step, Vec, end_to_end integration tests, description, required, $schema, title, type (+4 more)

### Community 23 - "ReportConfig Schema"
Cohesion: 0.22
Nodes (9): definitions, ReportConfig, url, description, properties, required, type, description (+1 more)

### Community 24 - "Schema Generation"
Cohesion: 0.22
Nodes (8): gen_schema example main, lint::lint, LintIssue, schema_issues, description, $schema, title, type

### Community 25 - "Schema Generation Tests"
Cohesion: 0.52
Nodes (6): Value, both_schemas_compile_as_valid_json_schema(), config_schema(), config_schema_describes_vus_and_duration_with_defaults(), test_schema(), test_schema_describes_steps_array()

### Community 26 - "Config Schema Properties"
Cohesion: 0.15
Nodes (13): default, description, type, properties, duration, report, vus, anyOf (+5 more)

### Community 27 - "VUs Schema Property"
Cohesion: 0.20
Nodes (21): cmd_append(), cmd_criterion(), cmd_embed(), cmd_merge(), cmd_parse(), cmd_setobj(), cmd_startup(), coerce() (+13 more)

### Community 28 - "Steps Schema"
Cohesion: 0.40
Nodes (5): $ref, properties, steps, items, type

### Community 32 - "Lint Core Issues"
Cohesion: 0.26
Nodes (10): Option, expected_response_line_does_not_override_aggregate(), extract_ms(), metric_value(), parse_summary(), parses_native_engine_summary(), parses_real_k6_summary(), RunSummary (+2 more)

### Community 33 - "Benchmark Script"
Cohesion: 0.18
Nodes (15): build_cmd(), cmd_k6_native(), cmd_k6_wrapped(), cmd_locust_native(), cmd_locust_wrapped(), cmd_yaml(), cmd_yaml_get(), has_suite() (+7 more)

### Community 45 - "Community 45"
Cohesion: 0.20
Nodes (9): Commands, Environment variables, How it works, License, Local development, perfscale, Release binaries, Repository layout (+1 more)

### Community 46 - "Community 46"
Cohesion: 0.22
Nodes (8): Benchmarks, Methodology, Reading `IO ops` (`in` / `out`), Reading the numbers, Regression tracking, Running locally, Running on CI (canonical), Suites

### Community 47 - "Community 47"
Cohesion: 0.25
Nodes (7): CI (GitHub Actions), Collect results from several terminals / machines, Login → authenticated request (chained steps), Recipes, Reuse an existing k6 script, Reuse an existing locustfile, Smoke-test an API before merging

### Community 48 - "Community 48"
Cohesion: 0.25
Nodes (7): Adding a new action (contributors), Built-in actions, Interpolation rules, `std/check@v1`, `std/http@v1`, `std/log@v1`, `std/sleep@v1`

### Community 49 - "Community 49"
Cohesion: 0.25
Nodes (7): Config (`-c config.yaml`), Step fields, Test definition (`-f test.yaml`), Validating without running: `perfscale lint`, Validation errors, Variable interpolation, YAML reference

### Community 50 - "Community 50"
Cohesion: 0.29
Nodes (6): Architecture, Design constraints, Embedding example, Module map, The one abstraction that matters: `LogLine`, Unified summary format

### Community 51 - "Community 51"
Cohesion: 0.33
Nodes (6): Default, LocustOpts::from_run_config, parse_duration_secs(), RunConfig, ConfigFile, ReportConfig

### Community 52 - "Community 52"
Cohesion: 0.29
Nodes (6): Collecting results from multiple runs, First run (no external tools needed), Getting started, Install, Next steps, Running k6 or locust scripts

### Community 53 - "Community 53"
Cohesion: 0.33
Nodes (5): Choosing an engine, k6 (`runner::k6`), locust (`runner::locust`), Native step engine (`step::runner`), Runners

### Community 54 - "Community 54"
Cohesion: 0.33
Nodes (5): CLI (`perfscale` binary), Core (`perfscale-core` library), For contributors, perfscale documentation, Start here

### Community 55 - "Community 55"
Cohesion: 0.50
Nodes (3): Commit messages, graphify, perfscale — opensource repo rules

## Knowledge Gaps
- **233 isolated node(s):** `PreToolUse`, `Commands`, `Commands`, `SelfUpdateArgs`, `Option` (+228 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **11 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Duration` connect `Self-Update Version & Artifacts` to `Step Actions (http/check/log/sleep)`, `Run Command Internals`, `CLI Integration Tests`, `E2E Workflow Tests`, `Self-Update Integration Tests`, `Self-Update Download/Verify/Swap`?**
  _High betweenness centrality (0.154) - this node is a cross-community bridge._
- **Why does `run_steps()` connect `Step Runner Core` to `Runner Output & LogLine Stream`, `Locust Runner Options`, `Runner Config & Output Structs`, `Context Interpolation`?**
  _High betweenness centrality (0.098) - this node is a cross-community bridge._
- **Why does `Json` connect `Serve HTTP Endpoints` to `Step Actions (http/check/log/sleep)`, `VUs Schema Property`, `Context Interpolation`, `Step Runner Core`?**
  _High betweenness centrality (0.087) - this node is a cross-community bridge._
- **Are the 2 inferred relationships involving `run_steps()` (e.g. with `run_streaming()` and `run_streaming()`) actually correct?**
  _`run_steps()` has 2 INFERRED edges - model-reasoned connections that need verification._
- **What connects `PreToolUse`, `Commands`, `Commands` to the rest of the system?**
  _245 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Step Actions (http/check/log/sleep)` be split into smaller, more focused modules?**
  _Cohesion score 0.0792156862745098 - nodes in this community are weakly interconnected._
- **Should `CLI Parser & Commands` be split into smaller, more focused modules?**
  _Cohesion score 0.050314465408805034 - nodes in this community are weakly interconnected._