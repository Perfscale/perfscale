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

- **`std/llm@v1` action** — LLM load testing: one chat-completion request
  per iteration against `openai` (also OpenAI-compatible servers: Ollama,
  vLLM, LM Studio, …), `anthropic`, or a `generic` endpoint whose response
  fields are pulled out with `extract` rules (dotted paths or one-group
  regexes). Streaming is the default and measures TTFT (time to first
  token); every request reports `llm_ttft_ms` and `llm_tokens_per_sec`
  trends plus `llm_prompt_tokens` / `llm_completion_tokens` / `llm_chunks`
  counters, gateable via `std/thresholds@v1`. A public observer seam
  (`register_llm_observer`, `LlmSample` with per-chunk arrival deltas) is
  where the pro build plugs in detailed ITL/TPOT and cost metrics. Guide:
  `docs/core/llm.md`.
