# Нагрузочное тестирование gRPC

perfscale нагружает **gRPC**-эндпоинты с помощью встроенного движка шагов:
открывает HTTP/2-каналы, выполняет унарные вызовы из JSON-шаблонов, запускает
клиентские/серверные/двунаправленные стримы и измеряет как полный круговой путь
запроса, так и RTT сообщений на уровне приложения — наряду с метриками
HTTP/TCP/UDP/WebSocket.

Вызовы **динамические**: генерация кода по protobuf не требуется. Схема
приходит во время выполнения (из дескриптор-сета или через серверный
reflection), запросы и ответы представлены в JSON, а perfscale преобразует их
по правилам protobuf-JSON.

Эта страница — руководство: из чего всё состоит и когда что использовать.
Параметры, выходы и семантика ошибок каждого шага описаны в
[справочнике actions](actions.md#grpc-stdgrpcv1-and-the-stdgrpc-v1-family);
готовый к запуску сценарий поставляется как
[`examples/grpc.test.yaml`](../../examples/grpc.test.yaml) вместе с локальным
эхо-сервером для проверки
(`cargo run -p perfscale-core --example grpc_echo_server`).

## Два стиля

- **Разовый вызов** (`std/grpc@v1`) — подключиться, загрузить схему, сделать
  один унарный вызов и закрыть соединение в одном шаге. Самый простой вариант
  для редких проб.
- **Живой канал** (`std/grpc-connect@v1` и сопутствующие шаги) — канал,
  удерживаемый между шагами внутри итерации и адресуемый по id, который
  возвращает шаг подключения. Унарные вызовы и стримы идут по одному
  HTTP/2-соединению: подключение и загрузка схемы оплачиваются один раз на
  итерацию, а не на вызов. Используйте этот стиль для любой серьёзной
  нагрузки.

Оба стиля свободно сочетаются в одном сценарии.

## Источники схемы

Динамическим вызовам нужна protobuf-схема во время выполнения. Оба шага с
подключением (`std/grpc@v1`, `std/grpc-connect@v1`) принимают ровно один
источник — они взаимоисключающие:

- **`descriptor_set`** — base64 от сериализованного `FileDescriptorSet`.
  Сгенерируйте его из своих proto-файлов:

  ```bash
  protoc --descriptor_set_out=echo.pb --include_imports echo.proto
  base64 -i echo.pb   # macOS; on Linux: base64 -w0 echo.pb
  ```

  Или получите его по HTTP на более раннем шаге и передайте бинарное тело
  напрямую — шаг `std/http@v1` возвращает бинарные ответы как `body_base64`:

  ```yaml
  steps:
    - name: fetch schema
      use: std/http@v1
      with: { url: "https://schema.example.com/echo.pb" }
      outputs: fetch

    - name: unary probe
      use: std/grpc@v1
      with:
        url: grpcs://api.example.com:443
        descriptor_set: "${{ fetch.body_base64 }}"
        method: "echo.v1.Echo/Unary"
        payload: { message: "ping ${seq}" }
      check:
        duration_ms_lt: 500
  ```

- **`reflection: true`** — получить схему через сервис reflection сервера
  (протокол v1); на сервере reflection должен быть включён. Полученный пул
  кэшируется по URL до конца прогона, так что повторные подключения к одному
  серверу тратят всего один reflection-запрос.

Некорректный `descriptor_set` завершается с ошибкой сразу, до любого сетевого
ввода-вывода. Методы именуются как `"package.Service/Method"`; при опечатке
ошибка содержит подсказку «возможно, вы имели в виду», если известный метод
достаточно похож.

## Живой канал

```yaml
# config.yaml — 25 concurrent VUs, each holding its own channel per iteration
vus: 25
duration: 5m
```

```yaml
# test.yaml
steps:
  - name: open channel
    use: std/grpc-connect@v1
    with:
      url: grpcs://api.example.com:443
      reflection: true
      metadata: { authorization: "Bearer ${{ vars.token }}" }
    outputs: conn

  - name: unary echo
    use: std/grpc-call@v1
    with:
      id: "${{ conn.id }}"
      method: "echo.v1.Echo/Unary"
      payload: { message: "ping-${seq}" }
    check:
      duration_ms_lt: 250

  - name: open bidi stream
    use: std/grpc-stream-open@v1
    with:
      id: "${{ conn.id }}"
      method: "echo.v1.Echo/Bidi"
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

Семь шагов, каждый с коротким псевдонимом:

- [`std/grpc-connect@v1`](actions.md#stdgrpc-connectv1) — открывает канал и
  загружает схему, возвращает `{ id, connected, duration_ms }`.
- [`std/grpc-call@v1`](actions.md#stdgrpc-callv1) — один унарный вызов по
  каналу. Неудачный RPC проваливает шаг, но канал остаётся рабочим:
  HTTP/2-каналы восстанавливаются, в отличие от мёртвого WebSocket.
- [`std/grpc@v1`](actions.md#stdgrpcv1--one-shot-call) — разовый вариант
  (подключение → схема → вызов → закрытие).
- [`std/grpc-stream-open@v1`](actions.md#stdgrpc-stream-openv1) — запускает
  клиентский стриминг, двунаправленный или серверный стриминг и возвращает id
  стрима. Для серверного стриминга единственный запрос уходит при открытии;
  ошибка на стороне сервера (UNIMPLEMENTED, аутентификация, …) проявится при
  первом recv/close, а не при open.
- [`std/grpc-stream-send@v1`](actions.md#stdgrpc-stream-sendv1) — отправляет
  `payload` (JSON, токены `${…}` подставляются при каждой отправке) или
  `payload_base64`; `repeat` + `interval_ms` отправляют N сообщений из одного
  шаблона.
- [`std/grpc-stream-recv@v1`](actions.md#stdgrpc-stream-recvv1) — читает до
  срабатывания **правила остановки**: `until_contains`, `until_json` или
  просто `count`.
- [`std/grpc-stream-close@v1`](actions.md#stdgrpc-stream-closev1) —
  полузакрывает сторону запросов и дочитывает сторону сервера до финального
  статуса.

Id каналов выдаются на каждого VU (`grpc-1`, `grpc-2`, …), id стримов —
аналогично (`grpcs-1`, …); оба действительны только внутри текущей итерации
этого VU. Канал никогда не переживает свою итерацию: всё, что сценарий
оставил открытым, сбрасывается в конце итерации (стримы отменяются — для
чистого завершения с проверкой статуса используйте `grpc-stream-close`), а
`grpc-connect` в блоке `before:` конфига бесполезен: контекст настройки и его
каналы исчезают до старта VU.

## Динамические payload

Запросы принимают `payload` (JSON → динамическое protobuf-сообщение) или
`payload_base64` (сериализованные protobuf-байты) — взаимоисключающие. JSON
преобразуется по правилам protobuf-JSON: имена полей принимаются как в виде
имени из proto, так и в camelCase `json_name`, 64-битные целые — строками,
перечисления — именами. Ответы появляются в `body` (унарные вызовы) и
`messages` (стримы) по тем же правилам.

Строковые листья `payload` могут содержать те же токены с одинарными скобками
`${…}`, что и при [отправке по WebSocket](websocket.md#dynamic-messages), —
они подставляются при каждом вызове/отправке (`${seq}` продолжает считать на
канал при унарных вызовах и на стрим при отправках в стрим). `payload_base64`
декодируется один раз и отправляется как есть — без подстановки токенов.
Значения metadata поддерживают только интерполяцию `${{ … }}`.

## Проверка ответов

Шаги приёма/закрытия стрима предоставляют список `messages`; `std/check@v1`
проверяет его с квантором **any** (хотя бы одно сообщение совпадает):

```yaml
check:
  message_contains: "trade"              # some message contains the substring
  message_matches: { type: trade }       # some message JSON-subset-matches
  messages_count_gte: 5                  # at least 5 messages arrived
```

Для детерминированных обменов адресуйте конкретное сообщение по индексу:
`check: { on: got.messages.0, message_matches: { type: welcome } }`.

Унарные вызовы проверяют код статуса через `expect_status` (например,
`expect_status: 5` проходит, когда сервер возвращает NOT_FOUND, и падает при
OK) и задержку через `check: { duration_ms_lt: … }`; JSON-`body` можно
проверять по полям через `outputs` и `on:`.

## Метрики

- **`grpc_req_duration`** — гистограмма задержки унарных вызовов (только
  `std/grpc@v1` и `std/grpc-call@v1`). Стримы намеренно не попадают в неё: их
  время жизни охватывает пользовательские шаги.
- **`grpc_msg_rtt`** — RTT сообщения на уровне приложения. При успешном
  унарном вызове равен длительности запроса; при recv в стриме это время от
  отправки до совпадения, записывается только если сработало правило `until_*`
  и перед этим на том же стриме был `grpc-stream-send`.
- **`grpc_msgs_sent` / `grpc_msgs_received`** — счётчики пропускной
  способности сообщений, по вызову и по шагу стрима.
- **`grpc_req_failed`** — RPC, не соответствующие `expect_status`. Для
  стримов финальный статус в этот счётчик превращает именно
  `grpc-stream-close`.

В отличие от WebSocket-рукопожатия, шаги gRPC никогда не попадают в
`http_req_duration` / `http_req_failed` — серии `grpc_*` описывают всё
полностью, а неудачное подключение не порождает метрик вообще (RPC не было).

## Ограничения

Размер входящих сообщений ограничен `max_recv_size` (по умолчанию 16 МиБ);
слишком большое сообщение проваливает вызов с RESOURCE_EXCEEDED. Бинарные
(`-bin`) ключи metadata не поддерживаются — значения metadata являются
строками. Для работы `reflection: true` серверный reflection должен быть явно
включён на сервере. Значения таймаутов, количество повторов `repeat` и длина
дренирования встроенных ограничений не имеют.
