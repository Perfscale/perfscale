# Рецепты

Готовые к запуску файлы-примеры для каждого движка лежат в [`examples/`](../../examples/).

## Smoke-тест API перед мёрджем

```yaml
# smoke.test.yaml
steps:
  - name: health
    use: std/http@v1
    with: { url: "https://staging.example.com/health" }
    check: { status: 200, duration_ms_lt: 300 }
```

```yaml
# smoke.config.yaml
vus: 5
duration: 30s
```

```sh
perfscale run -f smoke.test.yaml -c smoke.config.yaml
```

## Логин → авторизованный запрос (цепочка шагов)

```yaml
steps:
  - name: login
    use: std/http@v1
    with:
      method: POST
      url: https://api.example.com/login
      body: { user: demo, password: demo }
    check: { status: 200 }
    outputs: login

  - name: profile
    use: std/http@v1
    with:
      url: https://api.example.com/me
      headers:
        authorization: "Bearer ${{ login.body }}"
    check: { status: 200, body_contains: "demo" }

  - use: std/sleep@v1
    with: { ms: 500 }
```

## Нагрузочный тест WebSocket-эндпоинта

[`examples/websocket.test.yaml`](../../examples/websocket.test.yaml) показывает оба
стиля — живое соединение, удерживаемое между шагами, и разовую сессию. В живом
стиле соединение адресуется по id, который вернул `std/ws-connect@v1`:

```yaml
steps:
  - name: open feed
    use: std/ws-connect@v1
    with: { url: ws://127.0.0.1:9222 }
    outputs: feed

  - name: subscribe
    use: std/ws-send@v1
    with:
      id: "${{ feed.id }}"
      send: '{"op":"subscribe","id":"sub-${seq}"}'

  - name: await echo
    use: std/ws-recv@v1
    with: { id: "${{ feed.id }}", until_contains: "sub-1", timeout: 5000 }
    check: { message_contains: "subscribe" }

  - name: hang up
    use: std/ws-close@v1
    with: { id: "${{ feed.id }}" }
```

Запускайте против любого echo-сервера (`npx wscat --listen 9222`). Концепции — в
[руководстве по WebSocket](../core/websocket.md), а полный справочник параметров и
метрик — в [Actions → WebSocket](../core/actions.md#websocket-stdwsv1-and-the-stdws-v1-family).

## Нагрузочный тест gRPC-эндпоинта

[`examples/grpc.test.yaml`](../../examples/grpc.test.yaml) выполняет унарные вызовы
и двунаправленный (bidi) стрим через один живой канал. Без кодогенерации по protobuf:
схема загружается во время выполнения через server reflection (или base64
`descriptor_set`), а полезные данные — обычный JSON, преобразуемый по правилам
protobuf-JSON. Канал и его схема оплачиваются один раз за итерацию — вызовы и стримы
идут по тому же HTTP/2-соединению:

```yaml
steps:
  - name: open channel
    use: std/grpc-connect@v1
    with:
      url: grpc://127.0.0.1:50051
      reflection: true
    outputs: conn

  - name: unary echo
    use: std/grpc-call@v1
    with:
      id: "${{ conn.id }}"
      method: "perfscale.test.v1.Echo/Unary"
      payload: { message: "ping-${seq}" }
    check:
      duration_ms_lt: 250

  - name: open bidi stream
    use: std/grpc-stream-open@v1
    with:
      id: "${{ conn.id }}"
      method: "perfscale.test.v1.Echo/Bidi"
    outputs: stream

  - name: send events
    use: std/grpc-stream-send@v1
    with:
      id: "${{ stream.id }}"
      payload: { message: "evt-${seq}" }
      repeat: 5
      interval_ms: 20

  - name: await echoes
    use: std/grpc-stream-recv@v1
    with:
      id: "${{ stream.id }}"
      until_contains: "evt-5"
      timeout: 5000
    check:
      messages_count_gte: 5

  - name: close stream
    use: std/grpc-stream-close@v1
    with: { id: "${{ stream.id }}" }
```

Запускайте против встроенного echo-сервера
(`cargo run -p perfscale-core --example grpc_echo_server`). Для разовых проб
есть также one-shot-вариант `std/grpc@v1` (подключение → схема → вызов →
закрытие одним шагом). Концепции — в [руководстве по gRPC](../core/grpc.md),
а полный справочник параметров и метрик — в
[Actions → gRPC](../core/actions.md#grpc-stdgrpcv1-and-the-stdgrpc-v1-family).

## Нагрузочный тест базы данных

[`examples/db-sqlite.test.yaml`](../../examples/db-sqlite.test.yaml) работает
с in-memory SQLite — сервер не нужен. PostgreSQL и MySQL/MariaDB работают так же;
меняются только `driver` и `dsn`:

```yaml
steps:
  - name: open db
    use: std/db-connect@v1
    with:
      driver: postgres
      dsn: "${{ vars.db_dsn }}"        # keep the password out of the file
    outputs: db

  - name: write
    use: std/db-query@v1
    with:
      id: "${{ db.id }}"
      # SQL is never interpolated — values move through bound params.
      query: INSERT INTO hits (path, status) VALUES ($1, $2)
      params: ["/api/checkout", 200]

  - name: hang up
    use: std/db-close@v1
    with: { id: "${{ db.id }}" }
```

Транзакции (`std/db-tx-begin@v1` / `commit` / `rollback`) обрамляют группы
запросов на постоянном соединении; `mode: per-query`, наоборот, подключается
заново для каждого запроса (измеряя подключение + запрос). Метрики попадают в
`db_query_duration`, `db_rows` и классифицированные `db_errors`. Полный
справочник параметров и метрик — в
[Actions → Database](../core/actions.md#database-the-stddb-v1-family).

## Переиспользование существующего скрипта k6

```sh
perfscale run --k6 load-tests/checkout.js
```

Конфигурация нагрузки (VU, стадии, пороги) остаётся в блоке `options`
скрипта — perfscale стримит вывод и сохраняет семантику кодов выхода k6.

## Переиспользование существующего locustfile

```sh
perfscale run --locust locustfile.py --host https://target.example.com -c load.config.yaml
```

`vus`/`duration` из конфига мапятся на `--users`/`--spawn-rate`/`--run-time`.
После прогона CSV-статистика locust преобразуется в тот же summary-блок,
который выводят остальные движки.

## Сбор результатов с нескольких терминалов / машин

```sh
# terminal 1 — collector
perfscale serve --port 7999

# terminals 2..N — each run reports in
perfscale run -f test.yaml -c config.yaml --report http://collector-host:7999
```

Или зашейте коллектор в конфиг, чтобы флаг не был нужен:

```yaml
# config.yaml
vus: 10
duration: 5m
report:
  url: http://collector-host:7999
```

## CI (GitHub Actions)

[`Perfscale/github-action`](https://github.com/Perfscale/github-action)
устанавливает perfscale, запускает тест, выводит таблицу метрик в сводку джоба
и записывает машиночитаемую JSON-сводку:

```yaml
- uses: Perfscale/github-action@v1
  id: loadtest
  with:
    file: smoke.test.yaml
    config: smoke.config.yaml

- name: Gate on error rate and p95
  run: |
    jq -e '.summary.error_rate < 0.01 and .summary.p95_ms < 500' \
      "${{ steps.loadtest.outputs.summary-json }}"
```

Без action тот же гейт работает из любого CI через `--summary-export`:

```sh
perfscale run -f smoke.test.yaml -c smoke.config.yaml --summary-export result.json
jq -e '.summary.error_rate < 0.01' result.json
```

Сам прогон завершается с кодом `0`, даже когда проверки падают (см.
[семантику кодов выхода](commands.md#exit-code-semantics)) — если падения должны
ломать сборку, гейтите по экспортированной сводке, как показано выше.
