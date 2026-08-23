# LLM load testing

perfscale drives **LLM inference endpoints** under load with the native step
engine: send chat-completion requests, stream the responses, and measure
time-to-first-token (TTFT), generation throughput (tokens/sec), and token
usage — alongside your HTTP/gRPC/WebSocket metrics.

One step = one completion request. Streaming is the default: the response is
read as server-sent events, text deltas are concatenated, and TTFT is the
arrival time of the first chunk that carries content.

This page is the guide. Per-step parameters and outputs live in the
[actions reference](actions.md#stdllmv1).

## Endpoints

| Endpoint | Wire format |
|---|---|
| `openai` (default) | OpenAI chat completions (`POST /v1/chat/completions`) — also every OpenAI-compatible server: **Ollama**, **vLLM**, LM Studio, Together, Groq, … The step sends `stream_options: { include_usage: true }` when streaming, so token counts arrive with the final chunk |
| `anthropic` | Anthropic messages API (`POST /v1/messages`). Sends `anthropic-version: 2023-06-01`; usage comes from the `message_start` / `message_delta` SSE events |
| `generic` | Bring-your-own format: the request body is the step's `params` object verbatim, and the response fields are pulled out with `extract` rules (dotted paths or regexes) — for endpoints that match neither API |

## Local model: Ollama (OpenAI-compatible)

The fastest way to build the test logic — no API key, no cloud bill. Ollama
serves an OpenAI-compatible endpoint on port 11434:

```yaml
steps:
  - name: llama completion
    use: std/llm@v1
    with:
      url: http://127.0.0.1:11434/v1/chat/completions
      model: llama3.1
      prompt: "Summarize the CAP theorem in two sentences."
      max_tokens: 128
    check:
      status: 200
```

`prompt` is sugar for a single `user` message; use `messages` for a real
conversation (a `system` prompt, few-shot examples, …). Prompts interpolate
like any other step — `${{ vars.* }}`, outputs of previous steps, `${uuid}` —
so every iteration can send a fresh prompt.

## OpenAI with a key from the environment

```yaml
steps:
  - name: gpt completion
    use: std/llm@v1
    with:
      url: https://api.openai.com/v1/chat/completions
      model: gpt-4o-mini
      api_key: ${{ env.OPENAI_API_KEY }}
      messages:
        - { role: system, content: "Answer in one word." }
        - { role: user, content: "Capital of France?" }
      params:
        temperature: 0
      timeout_ms: 30000
    check:
      status: 200
```

`api_key` goes out as `Authorization: Bearer …`. Extra body fields the API
understands (`temperature`, `top_p`, `presence_penalty`, …) ride in `params`,
merged into the request verbatim. `${{ env.* }}` reads the process
environment at run time — if the variable is unset the step fails with
`env var 'OPENAI_API_KEY' is not set` instead of sending an empty key (see
[Variables](../yaml-reference.md#variables-)).

## Anthropic streaming

```yaml
steps:
  - name: claude completion
    use: std/llm@v1
    with:
      endpoint: anthropic
      url: https://api.anthropic.com/v1/messages
      model: claude-sonnet-4-5
      api_key: ${{ env.ANTHROPIC_API_KEY }}
      prompt: "Explain backpressure in one paragraph."
      max_tokens: 512
```

The key is sent as `x-api-key` (with `anthropic-version: 2023-06-01`).
Streaming is on by default — set `stream: false` for a single JSON response
(no TTFT is measured then; tokens/sec falls back to the whole request time).

## Generic endpoint with extract

For a server that matches neither API — here a text-generation-inference
style `/generate` that returns one JSON document:

```yaml
steps:
  - name: tgi generate
    use: std/llm@v1
    with:
      endpoint: generic
      url: http://127.0.0.1:8080/generate
      params:                        # this object IS the request body
        inputs: "Tell me a joke."
        parameters: { max_new_tokens: 64 }
      extract:
        text: "$.generated_text"                 # dotted path…
        completion_tokens: '"generated_tokens": (\d+)'   # …or a one-group regex
    outputs: gen
```

Each `extract` value is either a dotted path (`$.usage.completion_tokens`,
`$.choices[0].text` — nested keys and `[N]` array indices) applied to the
response JSON, or a regex with exactly one capture group applied to the raw
response text. With `stream: true` the SSE `data:` payloads are glued back
together and `extract` applies to the last JSON payload / the joined text.

## Metrics and thresholds

Every request folds into the run summary as custom metrics:

- `llm_ttft_ms` — trend: request start → first content chunk (streamed
  requests only).
- `llm_tokens_per_sec` — trend: completion tokens / generation time (after
  the first token when streamed, over the whole request otherwise).
- `llm_prompt_tokens`, `llm_completion_tokens` — counters as reported by the
  server.
- `llm_chunks` — counter of SSE chunks received.

Gate a run on them with `std/thresholds@v1`:

```yaml
  - use: std/thresholds@v1
    with:
      llm_ttft_ms:
        - "p(95)<500"            # 95% of requests start streaming within 500 ms
      llm_tokens_per_sec:
        - "avg>20"               # fleet-average generation speed
      llm_ttft_ms_failed:
        - "rate<0.01"            # fewer than 1% failed requests
```

Step-level assertions work too — the output exposes `ttft_ms`,
`duration_ms`, `tokens_per_sec`, `prompt_tokens`, `completion_tokens`,
`text`, and more, so `check: { duration_ms_lt: 2000 }` or a `std/check@v1`
step over `outputs` covers per-iteration SLOs.

## Connection posture

Requests reuse the VU's pinned HTTP client shard (the same pooling as
`std/http@v1`), so a VU hammers one endpoint over a warm keep-alive
connection. `timeout_ms` (default 120 s) bounds the *whole* request —
connect through the last stream chunk — so a stalled generation fails the
step instead of hanging the VU. A non-2xx status fails the step with the
status and the first ~500 characters of the error body.

## Limits

- `text` in the step output is truncated to ~4 KiB; assert on substrings via
  `std/check@v1`, not on the whole completion.
- Token counts come from the server's `usage` report — a server that does
  not report usage yields no `llm_*_tokens` / `llm_tokens_per_sec` metrics.
- TTFT needs streaming; non-streamed requests only report total latency.
- Detailed per-token timing (ITL/TPOT percentiles) and cost accounting are a
  pro capability on the `register_llm_observer` seam; the OSS build reports
  the metrics above.

Against a **local** model server the GPU is the system under test — turn on
[GPU metrics](gpu.md) (`gpu.enabled: true` in the config) to chart
utilization/VRAM/temperature alongside TTFT and tokens/sec.
