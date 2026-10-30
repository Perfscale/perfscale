# Upcoming release

<!--
Release notes for the next release, written as features land.

- Append short, user-facing entries below this comment as you merge changes
  (what changed and why a user cares — not commit messages).
- On a `v*` tag, the release workflow publishes everything below the comment
  as the release body (with the auto-generated changelog appended), then
  resets this file back to the template.
- If this file has no entries at tag time, the release falls back to
  auto-generated notes and the workflow prints a warning.
-->
- Hot-path allocation and correctness improvements: built-in payload expansion copies literal spans instead of individual bytes, token arguments are borrowed instead of allocated per call, and non-ASCII payloads are preserved correctly. GraphQL schemas are shared through `Arc` instead of deep-cloned per execution, each query is parsed once, and owned JSON variables expand in place. Rust WASM guests advertise `settings_once`, so run settings cross the host/guest boundary once per instance instead of being copied on every call; older components keep the full-settings behavior.
- Custom WASM steps (RFC 009): a WASM library component can now export steps dispatchable in `use:` as `lib/<alias>/<step>` — the component receives the interpolated `with:` params plus the step context (VU id, iteration, run settings) and returns the standard action result (success / output / tagged logs / metrics / optional `http_sample` and `secret_output_paths`), so it folds into `check:`, thresholds, and the run summary like any built-in. `lib/` is a reserved namespace (never shadows `std/*`/`pro/*`), `perfscale lint` validates `lib/` references against each component's probed manifest, and the run summary's `libraries:` line gains `step_calls` / `step_errors` / `step_p95_ms` next to the token-call counters. A new `process` capability (triple-gated: the YAML grant, `allow_library_capabilities: true`, and `allow_process_actions: true`) gives step components host-mediated one-shot process execution with bounded output, timeout, and process-tree cleanup — never raw WASI process imports. New ABI: `perfscale:step@0.1.0` (`wit/step/step.wit`), additive on top of the untouched `perfscale:library@0.3.0`.
