# Нагрузочное тестирование LLM

perfscale нагружает **эндпоинты LLM-инференса** с помощью нативного движка
шагов: отправляет запросы chat-completion, читает потоковые ответы и измеряет
время до первого токена (TTFT), скорость генерации (токены/с) и расход
токенов — вместе с вашими метриками HTTP/gRPC/WebSocket.

Один шаг = один запрос на completion. Стриминг включён по умолчанию: ответ
читается как server-sent events, текстовые дельты склеиваются, а TTFT — это
момент прихода первого чанка, содержащего контент.

Эта страница — руководство. Параметры и выходные данные шага описаны в
[справочнике actions](actions.md#stdllmv1).

## Эндпоинты

| Эндпоинт | Формат на проводе |
|---|---|
| `openai` (по умолчанию) | OpenAI chat completions (`POST /v1/chat/completions`) — а также любой OpenAI-совместимый сервер: **Ollama**, **vLLM**, LM Studio, Together, Groq, … При стриминге шаг отправляет `stream_options: { include_usage: true }`, поэтому счётчики токенов приходят с финальным чанком |
| `anthropic` | Anthropic messages API (`POST /v1/messages`). Отправляет `anthropic-version: 2023-06-01`; usage берётся из SSE-событий `message_start` / `message_delta` |
| `generic` | Свой собственный формат: тело запроса — это объект `params` шага как есть, а поля ответа извлекаются правилами `extract` (пути через точку или регулярные выражения) — для эндпоинтов, не соответствующих ни одному из API |

## Локальная модель: Ollama (OpenAI-совместимая)

Самый быстрый способ собрать логику теста — без API-ключа и счетов за облако.
Ollama обслуживает OpenAI-совместимый эндпоинт на порту 11434:

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

`prompt` — это сокращение для одиночного сообщения `user`; для настоящего
диалога (`system`-промпт, few-shot примеры, …) используйте `messages`.
Промпты интерполируются, как и любой другой шаг — `${{ vars.* }}`, выходные
данные предыдущих шагов, `${uuid}` — так что каждая итерация может отправлять
свежий промпт.

## OpenAI с ключом из окружения

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

`api_key` уходит в заголовке `Authorization: Bearer …`. Дополнительные поля
тела, которые понимает API (`temperature`, `top_p`, `presence_penalty`, …),
передаются в `params` и вставляются в запрос как есть. `${{ env.* }}` читает
окружение процесса во время выполнения — если переменная не задана, шаг падает
с ошибкой `env var 'OPENAI_API_KEY' is not set` вместо отправки пустого ключа
(см. [Переменные](../yaml-reference.md#variables-)).

## Стриминг Anthropic

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

Ключ отправляется в заголовке `x-api-key` (вместе с
`anthropic-version: 2023-06-01`). Стриминг включён по умолчанию — задайте
`stream: false` для единственного JSON-ответа (тогда TTFT не измеряется, а
токены/с считаются по полному времени запроса).

## Generic-эндпоинт с extract

Для сервера, не соответствующего ни одному из API, — например, эндпоинт
`/generate` в стиле text-generation-inference, возвращающий один JSON-документ:

```yaml
steps:
  - name: tgi generate
    use: std/llm@v1
    with:
      endpoint: generic
      url: http://127.0.0.1:8080/generate
      params:                        # этот объект и ЕСТЬ тело запроса
        inputs: "Tell me a joke."
        parameters: { max_new_tokens: 64 }
      extract:
        text: "$.generated_text"                 # путь через точку…
        completion_tokens: '"generated_tokens": (\d+)'   # …или regex с одной группой
    outputs: gen
```

Каждое значение `extract` — это либо путь через точку
(`$.usage.completion_tokens`, `$.choices[0].text` — вложенные ключи и индексы
массивов `[N]`), применяемый к JSON ответа, либо регулярное выражение ровно с
одной группой захвата, применяемое к сырому тексту ответа. При `stream: true`
полезные данные SSE `data:` склеиваются обратно, и `extract` применяется к
последнему JSON-пayload / склеенному тексту.

## Метрики и пороги

Каждый запрос складывается в сводку прогона как пользовательские метрики:

- `llm_ttft_ms` — HDR-гистограмма в мс (процентили в сводке); от начала
  запроса до первого чанка с контентом (время до первого токена). Только
  потоковые запросы.
- `llm_tokens_per_sec` — HDR-гистограмма; completion-токены / время
  генерации (после первого токена при стриминге, за весь запрос в остальных
  случаях). Только когда сервер сообщает число токенов завершения.
- `llm_prompt_tokens`, `llm_completion_tokens` — счётчики, как их сообщает
  сервер; отсутствуют, когда сервер не сообщает usage.
- `llm_chunks` — счётчик полученных SSE-чанков (0 для непотоковых
  запросов).
- `llm_ttft_ms_failed` / `llm_tokens_per_sec_failed` — производные доли
  ошибок: один сэмпл 0/1 на вызов, породивший соответствующий сэмпл
  (1 = шаг упал — не-2xx, таймаут, транспортная ошибка). См.
  [metrics.md](metrics.md#метрики-доли-ошибок-family_failed).

Ограничьте прогон по ним с помощью `std/thresholds@v1`:

```yaml
  - use: std/thresholds@v1
    with:
      llm_ttft_ms:
        - "p(95)<500"            # 95% запросов начинают стримить в течение 500 мс
      llm_tokens_per_sec:
        - "avg>20"               # средняя по флоту скорость генерации
      llm_ttft_ms_failed:
        - "rate<0.01"            # менее 1% неуспешных запросов
```

Работают и проверки на уровне шага — выходные данные содержат `ttft_ms`,
`duration_ms`, `tokens_per_sec`, `prompt_tokens`, `completion_tokens`, `text`
и другое, так что `check: { duration_ms_lt: 2000 }` или шаг `std/check@v1`
по `outputs` покрывают SLO на каждую итерацию.

## Работа с соединениями

Запросы переиспользуют закреплённый за VU шард HTTP-клиента (тот же пулинг,
что и у `std/http@v1`), поэтому VU бьёт в один эндпоинт по тёплому
keep-alive соединению. `timeout_ms` (по умолчанию 120 с) ограничивает *весь*
запрос — от соединения до последнего чанка стрима — так что зависшая
генерация роняет шаг, а не вешает VU. Статус, отличный от 2xx, роняет шаг с
указанием статуса и первых ~500 символов тела ошибки.

## Ограничения

- `text` в выходных данных шага обрезается до ~4 КиБ; проверяйте подстроки
  через `std/check@v1`, а не весь completion целиком.
- Счётчики токенов берутся из отчёта `usage` сервера — сервер, который не
  сообщает usage, не даёт метрик `llm_*_tokens` / `llm_tokens_per_sec`.
- TTFT требует стриминга; непотоковые запросы сообщают только общую задержку.
- Детальные пометоковые тайминги (перцентили ITL/TPOT) и учёт стоимости —
  это pro-capability на точке расширения `register_llm_observer`; OSS-сборка
  сообщает метрики, перечисленные выше.

При тестировании **локального** сервера модели GPU — это тестируемая система,
поэтому включите [GPU-метрики](gpu.md) (`gpu.enabled: true` в конфиге), чтобы
строить графики утилизации/VRAM/температуры рядом с TTFT и токенами/с. На
Mac с Apple Silicon используйте `gpu.source: powermetrics` для встроенного
GPU (истинная активная резидентность) плюс мощность линий ANE/CPU
(`ane_power_w`, `cpu_power_w`).
