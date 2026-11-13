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

## No model yet: the bundled mock

`examples/llm-mock.py` (Python standard library only) speaks both the
OpenAI and the Anthropic wire format, streamed or not, with model-like
timing — build and debug the test before pointing it at a GPU or a paid API:

```bash
python3 examples/llm-mock.py                 # :8088, ttft 150 ms, 40 tok/s
perfscale run -f examples/llm.test.yaml -c examples/llm.config.yaml
```

`TTFT_MS`, `TOK_S` and `FAIL_PCT` (injected 503s) shape the latency and the
error rate; `--port` moves it. Switching to a real server is a change of
`vars.host` / `vars.model` in `examples/llm.config.yaml`.

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

## Prompt pools

One fixed prompt lets the server's prefix/KV cache answer from memory and
flatters TTFT. `prompts` sends one entry per request, chosen at random, as
the single `user` message:

```yaml
    with:
      url: http://127.0.0.1:11434/v1/chat/completions
      model: llama3.1
      prompts:
        - "Summarize the CAP theorem in two sentences."
        - "Ticket ${uuid}: why does p95 matter more than the average?"
        - "List three causes of tail latency in an inference server."
```

`prompts` excludes `prompt` and `messages`. For a large pool — or one with a
system prompt per entry — keep it in a JSON file and pick per iteration with
[`std/pick@v1`](actions.md#stdpickv1) (needs `allow_file_actions: true`):

```yaml
steps:
  - use: std/pick@v1
    with: { file: ./prompts.json }     # ["prompt one", "prompt two", …]
    outputs: p
  - use: std/llm@v1
    with:
      url: http://127.0.0.1:11434/v1/chat/completions
      model: llama3.1
      messages:
        - { role: system, content: "You are a concise support assistant." }
        - { role: user, content: "${{ p }}" }
```

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

- `llm_ttft_ms` — HDR histogram in ms (percentiles in the summary); request
  start → first content chunk (time to first token). Streamed requests
  only.
- `llm_tokens_per_sec` — HDR histogram; completion tokens / generation time
  (after the first token when streamed, over the whole request otherwise).
  Only when the server reports completion tokens.
- `llm_prompt_tokens`, `llm_completion_tokens` — counters as reported by
  the server; absent when the server does not report usage.
- `llm_chunks` — counter of SSE chunks received (0 for non-streamed
  requests).
- `llm_ttft_ms_failed` / `llm_tokens_per_sec_failed` — derived failure
  rates: one 0/1 sample per invocation that produced the corresponding
  sample (1 = the step failed — non-2xx, timeout, transport error). See
  [metrics.md](metrics.md#failure-rate-metrics-family_failed).

Gate a run on them with `std/thresholds@v1` in the config's `after:` block
— it is evaluated once, after every VU has stopped (inside `steps:` it would
re-run on every iteration). `p(95)` and `p95` are the same aggregate:

```yaml
after:
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

## Reading the output

The live `[stats]` line every 5 s carries the LLM signal even on runs with no
HTTP traffic (`reqs`/`rps` stay 0 there):

```text
[stats] ts=1794553711570 rps=0.00 reqs=0 iters=20 llm_reqs=20 llm_tok_s=35.7 llm_ttft_p95=159.0
```

The end-of-run summary closes with one digest line:

```text
llm....................: reqs=20 2.05/s failed=0 ttft_p50=156ms ttft_p95=159ms tok_s_avg=35.8 tok_s_p50=35.8 tokens_in=196 tokens_out=1280
```

`--summary-export run.md` / `run.json` carry the same numbers (`LLM
requests`, `TTFT p50 / p95`, `tok/s avg / p50`, `Tokens in / out`; JSON key
`llm`). `--quiet` drops the per-request `LLM openai … → 200` lines under
load.

## When a request fails

A non-2xx status, a refused connection, or a timeout fails the step with the
server's message and, for the usual first-test mistakes, a `hint:`:

| Symptom | Hint |
|---|---|
| 404/405 and the url lacks `/v1/chat/completions` (`/v1/messages` for anthropic) | the path to add |
| 404/400 `model … not found` | the model is not on this server (`ollama pull <model>`) |
| 401/403 without `api_key` | `api_key: ${{ env.OPENAI_API_KEY }}` (or `ANTHROPIC_API_KEY`) |
| 429 | rate limited — the `Retry-After` value, lower VUs / arrival rate |
| 503 | server overloaded or the model is still loading |
| connection refused / timeout | nothing listening at `url` / raise `timeout_ms` |

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
utilization/VRAM/temperature alongside TTFT and tokens/sec. On Apple Silicon
Macs use `gpu.source: powermetrics` for the integrated GPU (true active
residency) plus ANE/CPU rail power (`ane_power_w`, `cpu_power_w`).
