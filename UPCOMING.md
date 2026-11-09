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

## k6 → native YAML converter

`perfscale convert k6 script.js` writes a schema-validated `test.yaml` +
`config.yaml` pair from a real JavaScript AST. It converts static HTTP calls,
payloads/headers/environment templates, status/body checks, sleep and groups,
fixed/staged/ramping-arrival load, and thresholds (including `abortOnFail`).
Dynamic JavaScript, xk6 modules and options without an exact native equivalent
fail with a source line instead of being silently dropped. Diagnostics now also
show the column and source snippet, underline the unsupported construct, and
suggest a concrete fix (`if:` alternatives, static values, `before:`/`after:`,
or keeping the script on the k6 runner). Existing outputs are protected unless
`--force` is passed; scripts outside the static subset continue to run through
`perfscale run --k6`.

- Benchmarks: gcannon — HttpArena's io_uring HTTP/1.1 load generator —
  joins the throughput and saturation tables as a competitor engine (`APPS`
  default now includes `gcannon`; Linux only, built from pinned tags against
  liburing 2.9 by the bench workflow). Same fairness rules as the other
  rows: one keep-alive connection per VU, no pipelining.

## JMeter: JTL metrics, `-J` properties, thresholds gates

`perfscale run --jmeter` now writes each run's samples to a private CSV
`results.jtl` and parses it incrementally into the shared metrics store:
the summary gains full `http_req_duration` percentiles (p50/p90/p95/p99),
per-JMeter-`label` series (bounded cardinality), and
`http_req_latency`/`http_req_connect`/`data_received`/`data_sent`. Plans are
parameterizable from the CLI with repeatable `-Jkey=value` /
`--jmeter-property key=value`, and `-c config.yaml` now applies `report:`
(end-of-run and during-run reporters) plus `after:` `std/thresholds@v1`
gates — a violated `severity: fail` gate exits non-zero. A zero-sample plan
still falls back to the console `summary =` translation; a malformed results
file fails the run instead of silently passing. Native-only config (load
shape, `before:`, other `after:` actions, `abort_on_fail`) is rejected with
a clear error rather than silently ignored.
