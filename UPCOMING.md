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

### LLM load testing DX

- **Readable LLM output.** The run summary ends with one `llm....` digest line (requests, TTFT p50/p95, tok/s, tokens in/out), `llm_tokens_per_sec` is printed in `tok/s` instead of `ms`, and the live `[stats]` line adds `llm_ttft_p95`. `--summary-export` `.md`/`.json` now include an LLM block instead of `Requests: 0 (no HTTP metrics)`.
- **Prompt pools.** `std/llm@v1` takes `prompts: [...]` — one random entry per request — so a fixed prompt no longer flatters TTFT through the server's prefix cache.
- **Error hints.** Failed LLM requests explain the usual first-test mistakes: missing `/v1/chat/completions` path, unknown model (`ollama pull`), missing/rejected `api_key`, 429 with `Retry-After`, 503, connection refused, timeout.
- **Mock server and examples.** `examples/llm-mock.py` (stdlib-only OpenAI + Anthropic SSE mock with `TTFT_MS`/`TOK_S`/`FAIL_PCT`) plus `examples/llm.test.yaml`, `llm-anthropic.test.yaml` and `llm.config.yaml` with an SLO gate.
- **Thresholds accept k6's `p(95)`** as an alias of `p95` — the spelling the LLM guide used to show now works.

### Quiet console by default

- **Native runs (`-f`) print only what matters**: `std/log@v1` messages, errors, failing checks, `[stats]`, `[system]` markers and the summary. Per-request lines and passing checks are off — and not even formatted, so the default is also cheaper under load. `-v` / `--verbose` brings them back for debugging. `std/log@v1` output now survives `--quiet` too (it used to be dropped with the request lines). `--quiet` keeps filtering k6/locust/jmeter output.
