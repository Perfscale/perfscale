# Нагрузочное тестирование WebSocket

perfscale нагружает **WebSocket**-эндпоинты с помощью нативного движка шагов:
открывает соединения, стримит сообщения из шаблонов, ждёт подходящих ответов
и измеряет как handshake, так и полный круг (round trip) сообщения на уровне
приложения — наряду с метриками HTTP/TCP/UDP.

Эта страница — руководство: из каких частей всё состоит и когда какую
использовать. Параметры, выходные данные и семантика ошибок каждого шага
описаны в [справочнике действий](actions.md#websocket-stdwsv1-and-the-stdws-v1-family);
готовый к запуску сценарий лежит в
[`examples/websocket.test.yaml`](../../examples/websocket.test.yaml).

## Два стиля

- **Разовая сессия** (`std/ws@v1`) — подключиться, обменяться сообщениями,
  закрыть соединение за один шаг. Вся сессия измеряется как один сэмпл
  задержки, как FIX-сессия. Самый простой вариант; подходит для обменов
  вида «запрос/ответ».
- **Живое соединение** (`std/ws-connect@v1` и компания) — соединение,
  удерживаемое между шагами внутри одной итерации и адресуемое по id,
  который возвращает шаг подключения. Позволяет чередовать WS-трафик с
  другими шагами: подписаться по WS, инициировать событие через REST,
  проверить, что push пришёл.

Оба стиля свободно смешиваются в одном сценарии.

## Разовая сессия

```yaml
steps:
  - name: subscribe and await first trade
    use: std/ws@v1
    with:
      url: wss://stream.example.com/feed
      messages:
        - send: '{"op":"subscribe","channel":"trades","id":"sub-${seq}"}'
          until_json: { type: trade }
    check:
      message_matches: { type: trade }
```

Каждый элемент `messages` отправляет payload; элемент с правилом `until_*`
ждёт подходящий ответ перед переходом к следующему элементу — и даёт один
сэмпл **RTT сообщения**. Шаг падает при ошибках handshake/транспорта или
когда правило любого элемента не сработало вовремя. Полный список параметров:
[`std/ws@v1`](actions.md#stdwsv1--one-shot-session).

## Живое соединение

```yaml
# config.yaml — 25 concurrent VUs, each holding its own connection per iteration
vus: 25
duration: 5m
```

```yaml
# test.yaml
steps:
  - name: open feed
    use: std/ws-connect@v1
    with: { url: "wss://stream.example.com/feed" }
    outputs: feed

  - name: subscribe
    use: std/ws-send@v1
    with:
      id: "${{ feed.id }}"
      send: '{"op":"subscribe","id":"sub-${seq}"}'

  - name: await confirmation
    use: std/ws-recv@v1
    with:
      id: "${{ feed.id }}"
      until_json: { type: subscribed }
    outputs: got

  - name: hang up
    use: std/ws-close@v1
    with: { id: "${{ feed.id }}" }
```

Пять шагов делят одно соединение:

- [`std/ws-connect@v1`](actions.md#stdws-connectv1) — открывает его и
  возвращает `{ id, subprotocol, … }`.
- [`std/ws-send@v1`](actions.md#stdws-sendv1) — отправляет текстовый шаблон
  (токены `${…}` подставляются при каждой отправке) или бинарные данные через
  `send_base64`; `repeat` + `interval_ms` стримят N сообщений из одного
  шаблона.
- [`std/ws-recv@v1`](actions.md#stdws-recvv1) — читает, пока не сработает
  **правило остановки**: `until_contains` (подстрока), `until_json`
  (совпадение по подмножеству JSON) или просто `count`. Если правило не
  сработало за `timeout`, шаг падает.
- [`std/ws-ping@v1`](actions.md#stdws-pingv1) — транспортный ping→pong; RTT
  попадает в `duration_ms` шага.
- [`std/ws-close@v1`](actions.md#stdws-closev1) — корректный close handshake.

Идентификаторы соединений выдаются на каждого виртуального пользователя (VU)
(`ws-1`, `ws-2`, …) и действуют только внутри текущей итерации этого VU —
id никогда не переходит в следующую итерацию или к другому VU. Всё, что
сценарий оставил открытым, сбрасывается в конце итерации (аварийно — для
чистого завершения используйте `ws-close`), а `ws-connect` в `before:`-настройке
конфига бесполезен: контекст настройки и его сокеты исчезают до старта VU.

## Динамические сообщения

Текстовые payload могут содержать токены `${…}` в одинарных фигурных скобках
(`${seq}`, `${uuid}`, `${now}`/`${now_ms}`/`${now_iso}`, `${rand(a,b)}`,
`${randf(a,b[,dp])}`, `${choice(x|y|z)}`), которые подставляются заново при
каждой отправке — в отличие от `${{ … }}`, который вычисляется один раз до
запуска действия:

```yaml
- use: std/ws-send@v1
  with:
    id: "${{ feed.id }}"
    send: '{"op":"order","id":"ord-${seq}","px":${randf(1.05,1.15,5)}}'
    repeat: 100
    interval_ms: 50
```

Полная таблица токенов — в
[справочнике `std/ws-send@v1`](actions.md#stdws-sendv1).

## Проверка сообщений

Шаги приёма отдают список `messages`; `std/check@v1` проверяет его с
квантором **any** (хотя бы одно сообщение совпало — потоки несут heartbeat'ы
и посторонние события):

```yaml
check:
  message_contains: "trade"              # some message contains the substring
  message_matches: { type: trade }       # some message JSON-subset-matches
  messages_count_gte: 5                  # at least 5 messages arrived
```

Для детерминированных обменов адресуйте конкретное сообщение по индексу:
`check: { on: got.messages.0, message_matches: { type: welcome } }`.

## Метрики

- **Handshake'и** и **разовые сессии** попадают в общую гистограмму задержек
  (`http_req_duration`) — сопоставимы с перцентилями HTTP/TCP/UDP;
  неудачные handshake'и учитываются в `http_req_failed`.
- **`ws_msg_rtt`** — RTT сообщения на уровне приложения: время от отправки
  до первого ответа, подходящего под until-правило, агрегируется как
  гистограмма (p50 / p95 / max в сводке).
- **`ws_msgs_sent` / `ws_msgs_received`** — счётчики пропускной способности
  по сообщениям.
- Ожидание server-push потока намеренно *не* считается задержкой — то,
  сколько сервер решит ждать перед отправкой, не является задержкой цели и
  только отравило бы общие перцентили.

## Ограничения

Входящие протокольные лимиты берутся из настроек WebSocket-библиотеки по
умолчанию: сообщения до 64 МиБ, отдельные фреймы до 16 МиБ — бо́льшее
входящее сообщение приводит к ошибке соединения (а значит, и шага, который
его читает). Значения таймаутов, количества `repeat` и список `messages`
встроенных ограничений не имеют.
