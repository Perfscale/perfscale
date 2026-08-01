# Graph Report - perfscale  (2026-07-26)

## Corpus Check
- 68 files · ~100,954 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 1745 nodes · 3513 edges · 87 communities (75 shown, 12 thin omitted)
- Extraction: 97% EXTRACTED · 3% INFERRED · 0% AMBIGUOUS · INFERRED: 118 edges (avg confidence: 0.81)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `c1966ccd`
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
- [[_COMMUNITY_Community 57|Community 57]]
- [[_COMMUNITY_Community 58|Community 58]]
- [[_COMMUNITY_Community 59|Community 59]]
- [[_COMMUNITY_Community 60|Community 60]]
- [[_COMMUNITY_Community 61|Community 61]]
- [[_COMMUNITY_Community 62|Community 62]]
- [[_COMMUNITY_Community 63|Community 63]]
- [[_COMMUNITY_Community 64|Community 64]]
- [[_COMMUNITY_Community 65|Community 65]]
- [[_COMMUNITY_Community 66|Community 66]]
- [[_COMMUNITY_Community 67|Community 67]]
- [[_COMMUNITY_Community 68|Community 68]]
- [[_COMMUNITY_Community 69|Community 69]]
- [[_COMMUNITY_Community 70|Community 70]]
- [[_COMMUNITY_Community 71|Community 71]]
- [[_COMMUNITY_Community 72|Community 72]]
- [[_COMMUNITY_Community 73|Community 73]]
- [[_COMMUNITY_Community 74|Community 74]]
- [[_COMMUNITY_Community 75|Community 75]]
- [[_COMMUNITY_Community 76|Community 76]]
- [[_COMMUNITY_Community 77|Community 77]]
- [[_COMMUNITY_Community 78|Community 78]]
- [[_COMMUNITY_Community 79|Community 79]]
- [[_COMMUNITY_Community 80|Community 80]]
- [[_COMMUNITY_Community 81|Community 81]]
- [[_COMMUNITY_Community 82|Community 82]]
- [[_COMMUNITY_Community 83|Community 83]]
- [[_COMMUNITY_Community 84|Community 84]]
- [[_COMMUNITY_Community 85|Community 85]]
- [[_COMMUNITY_Community 86|Community 86]]

## God Nodes (most connected - your core abstractions)
1. `execute_action()` - 153 edges
2. `cmd()` - 24 edges
3. `lint()` - 24 edges
4. `ManagedProcess` - 23 edges
5. `ActionOutput` - 22 edges
6. `start_echo_server()` - 22 edges
7. `Value` - 21 edges
8. `String` - 21 edges
9. `run_native()` - 21 edges
10. `Context` - 20 edges

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
- 1-file cycle: `crates/perfscale-cli/src/commands/schema.rs -> crates/perfscale-cli/src/commands/schema.rs`
- 1-file cycle: `crates/perfscale-cli/src/commands/self_update.rs -> crates/perfscale-cli/src/commands/self_update.rs`
- 1-file cycle: `crates/perfscale-cli/src/commands/serve.rs -> crates/perfscale-cli/src/commands/serve.rs`
- 1-file cycle: `crates/perfscale-core/benches/engine.rs -> crates/perfscale-core/benches/engine.rs`
- 1-file cycle: `crates/perfscale-core/src/step/actions.rs -> crates/perfscale-core/src/step/actions.rs`
- 1-file cycle: `crates/perfscale-core/src/step/context.rs -> crates/perfscale-core/src/step/context.rs`
- 1-file cycle: `crates/perfscale-core/src/step/grpc.rs -> crates/perfscale-core/src/step/grpc.rs`
- 1-file cycle: `crates/perfscale-core/src/step/runner.rs -> crates/perfscale-core/src/step/runner.rs`
- 1-file cycle: `crates/perfscale-cli/tests/cli.rs -> crates/perfscale-cli/tests/cli.rs`
- 1-file cycle: `crates/perfscale-cli/tests/self_update.rs -> crates/perfscale-cli/tests/self_update.rs`
- 1-file cycle: `crates/perfscale-core/examples/grpc_echo_server.rs -> crates/perfscale-core/examples/grpc_echo_server.rs`
- 1-file cycle: `crates/perfscale-core/src/lint.rs -> crates/perfscale-core/src/lint.rs`
- 1-file cycle: `crates/perfscale-core/src/runner/k6.rs -> crates/perfscale-core/src/runner/k6.rs`
- 1-file cycle: `crates/perfscale-core/src/runner/locust.rs -> crates/perfscale-core/src/runner/locust.rs`
- 1-file cycle: `crates/perfscale-core/src/step/process.rs -> crates/perfscale-core/src/step/process.rs`
- 1-file cycle: `crates/perfscale-core/src/step/resources.rs -> crates/perfscale-core/src/step/resources.rs`

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

## Communities (87 total, 12 thin omitted)

### Community 0 - "Step Actions (http/check/log/sleep)"
Cohesion: 0.05
Nodes (80): check_action_bad_on_path_falls_back_to_last(), check_action_body_contains_pass_and_fail(), check_action_duration_ms_lt_handles_fractional_values(), check_action_duration_ms_lt_pass_and_fail(), check_action_message_contains_any_semantics(), check_action_message_matches_ws_strings_and_fix_objects(), check_action_messages_count_gte(), check_action_missing_target_fails_gracefully() (+72 more)

### Community 1 - "CLI Parser & Commands"
Cohesion: 0.22
Nodes (13): Atomic self-update binary swap pattern, self_update download, mock_release test fixture, replace_executable, self_update command handler, asset_url, current_artifact, fetch_latest_tag (+5 more)

### Community 2 - "Runner Output & LogLine Stream"
Cohesion: 0.05
Nodes (71): k6-compatible summary format, Unified LogLine output stream, Child, Error, PathBuf, Result, RunOutput, String (+63 more)

### Community 3 - "Docs, Examples & Schemas"
Cohesion: 0.08
Nodes (39): std/check@v1 action, std/http@v1 action, std/log@v1 action, std/sleep@v1 action, Benchmark methodology (hyperfine), ConfigFile schema, ReportConfig schema, External engines as subprocesses constraint (+31 more)

### Community 4 - "CLI Arg Parsing & Lint Tests"
Cohesion: 0.06
Nodes (38): Commands, Error, Option, PathBuf, Result, String, SummaryFormat, Vec (+30 more)

### Community 5 - "Runner Config & Output Structs"
Cohesion: 0.05
Nodes (111): Code, Codec, ActionOutput, Arc, Channel, Context, DescriptorPool, Duration (+103 more)

### Community 6 - "Step Runner Core"
Cohesion: 0.08
Nodes (66): AtomicBool, BTreeMap, Arc, Context, Default, HttpSample, LogLine, LogTag (+58 more)

### Community 7 - "Run Command Internals"
Cohesion: 0.11
Nodes (42): base_args(), build_export(), build_export_parses_summary_and_stamps_meta(), build_export_without_http_metrics_has_none_summary(), export_format(), is_summary_line(), load_config(), load_test_def() (+34 more)

### Community 8 - "Self-Update Version & Artifacts"
Cohesion: 0.10
Nodes (26): Duration, Option, PathBuf, Result, String, Duration, api_base(), artifact_for() (+18 more)

### Community 9 - "Lint Engine (did-you-mean)"
Cohesion: 0.08
Nodes (48): effective_kind(), kind_label(), lint_file(), print_issues(), run(), CliError, Path, Result (+40 more)

### Community 10 - "CLI Integration Tests"
Cohesion: 0.14
Nodes (25): Command, cmd(), errors_carry_hint_and_docs_sections(), help_flag_lists_all_commands(), k6_available(), lint_missing_file_is_a_cli_error_with_hint(), lint_missing_use_shows_fix_with_action_list(), lint_schema_override_forces_config_validation() (+17 more)

### Community 11 - "YAML Parsing"
Cohesion: 0.10
Nodes (35): Map, Option, Result, RunConfig, Step, String, TestDef, Value (+27 more)

### Community 12 - "Locust Runner Options"
Cohesion: 0.13
Nodes (8): Self, String, Value, default_duration(), default_vus(), run_config_default_is_one_vu_one_minute(), run_config_duration_secs_delegates_to_parser(), Step

### Community 13 - "E2E Workflow Tests"
Cohesion: 0.12
Nodes (27): BufReader, ChildStdout, Child, Self, String, Vec, Drop, NamedTempFile (+19 more)

### Community 14 - "Context Interpolation"
Cohesion: 0.13
Nodes (25): Arc, HashMap, LogLine, Option, PathBuf, ProcessRegistry, Self, Sender (+17 more)

### Community 15 - "CliError Formatting"
Cohesion: 0.19
Nodes (15): Formatter, Option, Result, Self, String, Display, Formatter, Into (+7 more)

### Community 16 - "Serve HTTP Endpoints"
Cohesion: 0.07
Nodes (44): bench_interpolate(), bench_metrics(), bench_ring_buf(), bench_wait_until(), bench_yaml_parse(), filled_capture(), run(), app() (+36 more)

### Community 17 - "Test Schema Definitions"
Cohesion: 0.12
Nodes (17): description, description, type, description, type, check, name, outputs (+9 more)

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
Cohesion: 0.07
Nodes (30): description, definitions, ReportConfig, Step, description, type, description, type (+22 more)

### Community 24 - "Schema Generation"
Cohesion: 0.18
Nodes (10): gen_schema example main, lint::lint, LintIssue, schema_issues, description, $schema, title, type (+2 more)

### Community 25 - "Schema Generation Tests"
Cohesion: 0.32
Nodes (7): Default, Option, PathBuf, LocustOpts::from_run_config, parse_duration_secs(), preset_config(), RunConfig

### Community 26 - "Config Schema Properties"
Cohesion: 0.06
Nodes (37): default, description, items, type, default, description, type, default (+29 more)

### Community 27 - "VUs Schema Property"
Cohesion: 0.20
Nodes (21): cmd_append(), cmd_criterion(), cmd_embed(), cmd_merge(), cmd_parse(), cmd_setobj(), cmd_startup(), coerce() (+13 more)

### Community 28 - "Steps Schema"
Cohesion: 0.40
Nodes (5): $ref, properties, steps, items, type

### Community 32 - "Lint Core Issues"
Cohesion: 0.16
Nodes (19): Option, String, expected_response_line_does_not_override_aggregate(), export_json_round_trips_and_is_self_describing(), export_markdown_renders_dash_for_missing_percentiles(), export_markdown_renders_metric_table(), export_markdown_without_metrics_says_no_traffic(), ExportMeta (+11 more)

### Community 33 - "Benchmark Script"
Cohesion: 0.18
Nodes (16): build_cmd(), cmd_k6_native(), cmd_k6_wrapped(), cmd_locust_native(), cmd_locust_wrapped(), cmd_yaml(), cmd_yaml_get(), cmd_yaml_get_quiet() (+8 more)

### Community 45 - "Community 45"
Cohesion: 0.18
Nodes (10): Commands, Environment variables, How it works, Install, License, Local development, perfscale, Release binaries (+2 more)

### Community 46 - "Community 46"
Cohesion: 0.22
Nodes (8): Benchmarks, Methodology, Reading `IO ops` (`in` / `out`), Reading the numbers, Regression tracking, Running locally, Running on CI (canonical), Suites

### Community 47 - "Community 47"
Cohesion: 0.20
Nodes (9): CI (GitHub Actions), Collect results from several terminals / machines, Load-test a gRPC endpoint, Load-test a WebSocket endpoint, Login → authenticated request (chained steps), Recipes, Reuse an existing k6 script, Reuse an existing locustfile (+1 more)

### Community 48 - "Community 48"
Cohesion: 0.05
Nodes (36): Adding a new action (contributors), Built-in actions, Channel profile, Connection profile, Custom actions from downstream crates, gRPC limits, gRPC: `std/grpc@v1` and the `std/grpc-*@v1` family, Interpolation rules (+28 more)

### Community 49 - "Community 49"
Cohesion: 0.18
Nodes (10): Config (`-c config.yaml`), Setup and variables, Step fields, Teardown (`after:`), Test definition (`-f test.yaml`), Validating without running: `perfscale lint`, Validation errors, Variable interpolation (+2 more)

### Community 50 - "Community 50"
Cohesion: 0.29
Nodes (6): Architecture, Design constraints, Embedding example, Module map, The one abstraction that matters: `LogLine`, Unified summary format

### Community 51 - "Community 51"
Cohesion: 0.40
Nodes (5): Engine availability errors, Exit code semantics, Output streams, `perfscale run`, Summary export

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

### Community 57 - "Community 57"
Cohesion: 0.22
Nodes (9): Benchmarking, CLI commands, Environment variables, npm installs, `perfscale lint`, `perfscale schema`, `perfscale self-update`, `perfscale serve` (+1 more)

### Community 58 - "Community 58"
Cohesion: 0.20
Nodes (14): run --report to serve reporting loop, RunArgs, CliError, CliError::from_engine, is_summary_line, load_config, load_test_def, print_line (+6 more)

### Community 59 - "Community 59"
Cohesion: 0.29
Nodes (7): Cli root parser, SelfUpdateArgs, ServeArgs, ServeProc test harness, main entrypoint, serve Router app, serve command handler

### Community 60 - "Community 60"
Cohesion: 0.50
Nodes (5): LintArgs, SchemaKind enum, lint_file, print_issues, lint command handler

### Community 61 - "Community 61"
Cohesion: 0.67
Nodes (3): verify_digest, digest_from_sums, sha256_hex

### Community 62 - "Community 62"
Cohesion: 0.50
Nodes (3): Added, Changed, Upcoming release

### Community 63 - "Community 63"
Cohesion: 0.14
Nodes (37): Context, Error, HttpSample, LogTag, Map, Option, PathBuf, Result (+29 more)

### Community 64 - "Community 64"
Cohesion: 0.10
Nodes (19): Alternatives considered, Benefits, Detailed design, Drawbacks, Execution order and lifecycle, Goals, Metrics isolation, Motivation (+11 more)

### Community 65 - "Community 65"
Cohesion: 0.67
Nodes (3): Client, shared_client(), shared_insecure_client()

### Community 66 - "Community 66"
Cohesion: 0.07
Nodes (62): c_int, Arc, Child, Command, Duration, HashMap, LogLine, LogSource (+54 more)

### Community 67 - "Community 67"
Cohesion: 0.11
Nodes (18): Action identity and resolution, Alternatives considered, Benefits, Detailed design, Drawbacks, Execution model (the hard part — options, not a decision), Goals, Motivation (+10 more)

### Community 68 - "Community 68"
Cohesion: 0.11
Nodes (18): Alternatives considered, Benefits, Detailed design, Drawbacks, Execution, Goals, Motivation, Non-goals (+10 more)

### Community 69 - "Community 69"
Cohesion: 0.11
Nodes (17): Alternatives considered, Benefits, Detailed design, Drawbacks, Goals, Layer 1 — the contract: test definition schema as the API, Layer 2 — Rust: stabilize a `perfscale` facade crate, Layer 3 — language SDKs: builders + drivers, not engines (+9 more)

### Community 70 - "Community 70"
Cohesion: 0.32
Nodes (8): Arc, RwLock, Send, action_registry(), ActionHandler, register_action(), registered_handler_serves_custom_action(), Sync

### Community 71 - "Community 71"
Cohesion: 0.40
Nodes (5): HashMap, Mutex, FileCacheEntry, FileCacheKey, file_cache()

### Community 72 - "Community 72"
Cohesion: 0.50
Nodes (3): perfscale RFCs, Process, Status values

### Community 73 - "Community 73"
Cohesion: 0.40
Nodes (3): common, TARGETS, [version, distDir, outDir]

### Community 74 - "Community 74"
Cohesion: 0.21
Nodes (16): BidiStream, Echo, EchoRequest, EchoResponse, Request, Response, Result, ServerStreamStream (+8 more)

### Community 75 - "Community 75"
Cohesion: 0.33
Nodes (5): Environment variables, MCP server, Notes, Setup, Tools

### Community 76 - "Community 76"
Cohesion: 0.08
Nodes (63): ActionOutput, Arc, ClientConfig, ActionOutput, Context, Gen, Instant, Option (+55 more)

### Community 77 - "Community 77"
Cohesion: 0.15
Nodes (23): Option, Self, String, Vec, choice_picks_one_option(), civil_from_millis(), double_brace_engine_placeholders_are_untouched(), Gen (+15 more)

### Community 78 - "Community 78"
Cohesion: 0.09
Nodes (35): Arc, Channel, DescriptorPool, DynamicMessage, Formatter, Gen, HashMap, Instant (+27 more)

### Community 79 - "Community 79"
Cohesion: 0.24
Nodes (14): BidiStream, Box, Echo, EchoRequest, EchoResponse, Error, Request, Response (+6 more)

### Community 80 - "Community 80"
Cohesion: 0.20
Nodes (10): ProcessRegistry, child_process_reports_output_shape_and_registers_by_step_name(), child_process_validates_params_before_spawning(), child_process_wait_until_failure_fails_the_step(), child_process_wait_until_timeout_continue_still_succeeds(), kill_process_by_name_stops_the_registered_process(), kill_process_by_name_targets_the_current_pid_after_restart(), kill_process_by_raw_pid_is_best_effort() (+2 more)

### Community 81 - "Community 81"
Cohesion: 0.22
Nodes (8): Asserting responses, Dynamic payloads, gRPC load testing, Limits, Live channel, Metrics, Schema sources, Two styles

### Community 82 - "Community 82"
Cohesion: 0.22
Nodes (8): Asserting messages, Dynamic messages, Limits, Live connection, Metrics, One-shot session, Two styles, WebSocket load testing

### Community 83 - "Community 83"
Cohesion: 0.33
Nodes (6): definitions, Step, anyOf, description, required, type

### Community 84 - "Community 84"
Cohesion: 0.40
Nodes (4): Box, Error, Result, main()

### Community 85 - "Community 85"
Cohesion: 0.40
Nodes (5): Path, confined_ctx(), file_actions_allowed_inside_fs_root(), file_read_rejects_path_traversal_escape_when_confined(), file_write_rejects_path_traversal_escape_when_confined()

## Knowledge Gaps
- **462 isolated node(s):** `PreToolUse`, `Commands`, `Commands`, `SchemaDumpKind`, `SchemaDumpKind` (+457 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **12 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Duration` connect `Self-Update Version & Artifacts` to `Step Actions (http/check/log/sleep)`, `Run Command Internals`, `CLI Integration Tests`, `Community 76`, `E2E Workflow Tests`, `Self-Update Integration Tests`, `Self-Update Download/Verify/Swap`?**
  _High betweenness centrality (0.142) - this node is a cross-community bridge._
- **Why does `execute_action()` connect `Step Actions (http/check/log/sleep)` to `Runner Config & Output Structs`, `Community 70`, `Step Runner Core`, `Community 76`, `Community 80`, `Community 85`, `Community 86`, `Schema Generation`, `Community 63`?**
  _High betweenness centrality (0.136) - this node is a cross-community bridge._
- **Why does `Sync` connect `Community 70` to `Step Actions (http/check/log/sleep)`, `Community 66`, `Community 78`, `Step Runner Core`?**
  _High betweenness centrality (0.086) - this node is a cross-community bridge._
- **Are the 51 inferred relationships involving `execute_action()` (e.g. with `lint::lint` and `bidi_send_recv_until_close()`) actually correct?**
  _`execute_action()` has 51 INFERRED edges - model-reasoned connections that need verification._
- **What connects `PreToolUse`, `Commands`, `Commands` to the rest of the system?**
  _474 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Step Actions (http/check/log/sleep)` be split into smaller, more focused modules?**
  _Cohesion score 0.051061388410786 - nodes in this community are weakly interconnected._
- **Should `Runner Output & LogLine Stream` be split into smaller, more focused modules?**
  _Cohesion score 0.05094905094905095 - nodes in this community are weakly interconnected._