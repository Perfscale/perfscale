# WebRTC (pro)

> Pro-модуль. Шаги ниже поставляются в платной сборке агента (префикс `pro/`
> ограничен тарифом); в OSS-движке блок `webrtc:` или шаг `pro/webrtc-*`
> падает при загрузке с понятной ошибкой «требуется pro-модуль».
> Дизайн: [RFC 007](../rfcs/007-webrtc.md).

Шаги `pro/webrtc-*` нагружают WebRTC-системы через **полный медиа-тракт**:
настоящие ICE/DTLS-SRTP-хендшейки и настоящие RTP-потоки между виртуальными
пользователями и целью. Движок владеет медиа-плоскостью; **сигналинг** —
либо встроенный WHIP/WHEP-клиент (стандартизованный ingest/playback), либо
кастомный протокол через [библиотеку](library-sdk.md) — движок не обрастает
кодом под каждого вендора.

## Конфигурация

```yaml
# config.yaml
webrtc:
  ice_servers:                  # по умолчанию: stun:stun.l.google.com:19302
    - urls: ["turn:turn.example.com:3478"]
      username: ${TURN_USER}
      credential: ${TURN_PASS}
  max_peer_connections: 500     # опциональный guardrail; без ключа — без лимита
```

Периодичность сбора статистики (фоновый `getStats` против итоговых
агрегатов) наследуется от конфигурации метрик запуска — модуль не добавляет
своих настроек.

## Шаги

### `pro/webrtc-connect@v1`

Создаёт peer connection и завершает сигналинг. Возвращает хендл соединения
(`rtc-1`, …) через `outputs:` — модель живых соединений, как у `std/ws-*`.

| Параметр | Тип | По умолчанию | Описание |
|----------|-----|--------------|----------|
| `signal` | string | — | `whip` \| `whep` \| `library` |
| `url` | string | — | WHIP/WHEP-эндпоинт (только `signal: whip\|whep`; токены `${…}` раскрываются, напр. `cam-${vu}`) |
| `bearer` | string | — | Bearer-токен эндпоинта (только `signal: whip\|whep`) |
| `library_call` | string | — | Только `signal: library`: один токен `${alias.fn(args)}`, указывающий функцию библиотеки, которая отвечает на SDP-оффер |
| `trickle` | bool | `false` | Дотекание ICE-кандидатов через WHIP PATCH (только WHIP/WHEP — library-сигналинг всегда non-trickle, кандидаты встроены в оффер); по умолчанию non-trickle (детерминированные метрики сетапа) |
| `on_disconnect` | string | `fail_fast` | `fail_fast` завершает последующие шаги при ICE disconnect; `restart` выполняет настоящий перезапуск ICE (см. ниже) |
| `timeout` | ms | `10000` | Таймаут сигналинга и подключения (включая вызов библиотеки) |

#### `signal: library`

Кастомный сигналинг (LiveKit, mediasoup, Janus, …) делегируется
[библиотеке RFC 005](library-sdk.md): шаг сам строит SDP-оффер (трансиверы
объявляются как send+receive, так что library-соединение умеет и публиковать,
и подписываться), встраивает собранные ICE-кандидаты в него и вызывает
функцию из `library_call`, передавая **SDP-оффер первым аргументом**, дальше —
объявленные в токене аргументы (действует контракт отображения текст→JSON из
RFC 005; токены `${…}` внутри аргументов раскрываются первыми — работает
`identity=vu-${vu}`). Библиотека возвращает SDP-ответ JSON-строкой
`{"sdp": "…", "type": "answer"}`; кривой ответ завершает шаг ошибкой с именем
вызова. `library_call` требует `signal: library` и наоборот; `url`/`bearer`/
`trickle` с `library` отклоняются (всё необходимое библиотеке передавайте
через аргументы вызова).

#### `on_disconnect: restart`

При ICE disconnect соединение выполняет настоящий перезапуск ICE вместо
смерти: свежие ICE-креды (`restart_ice`), новый раунд сбора кандидатов и
повторная сигнализация кредов — WHIP/WHEP отправляет PATCH на session-ресурс
с ICE-restart-фрагментом `application/trickle-ice-sdpfrag` (форма перезапуска
из драфта; если сервер не прислал `Location` при connect, session-ресурса нет
и перезапуск невозможен), `signal: library` повторно вызывает функцию
библиотеки с новым SDP-оффером. Трансиверы и треки переживают перезапуск,
поэтому publish/subscribe продолжаются на новой ICE-сессии. На соединение
выделяется максимум **3 попытки перезапуска** (с паузой 500 мс, каждая
ограничена `timeout` шага); если цель окончательно мертва, бюджет
исчерпывается и последующие шаги падают ровно как при `fail_fast`. Успешные
перезапуски учитываются в `webrtc_ice_restarts_total` и поле снапшота
`ice_restarts`, а также пишутся в лог. `pro/webrtc-call@v1` всегда работает
в режиме `fail_fast`.

```yaml
libraries:
  - use: ./livekit-signaling.wasm
    capabilities: []

steps:
  - name: join room
    use: pro/webrtc-connect@v1
    with:
      signal: library
      library_call: ${livekit.offer(room=loadtest, identity=vu-${vu})}
    outputs: room
```

### `pro/webrtc-publish@v1`

Прикрепляет треки к соединению и начинает отправку.

| Параметр | Тип | По умолчанию | Описание |
|----------|-----|--------------|----------|
| `id` | string | — | Хендл из connect (`rtc-N`) |
| `tracks` | list | — | `kind: audio\|video`, `codec: opus\|vp8\|h264`, `source: synthetic\|file`, `bitrate`, `resolution` (видео), интервал ключевых кадров |

Синтетическое аудио — тон Opus с дизерингом амплитуды (кодер не уходит в
DTX); синтетическое видео — движущийся тестовый паттерн с меткой времени
в кадре. С `source: file` трек зацикливает файл-образец (`path:`, относительно
рабочей директории): **IVF** (VP8) и **Annex-B H.264** (`.h264`, темп по
`fps:`, по умолчанию 30) для видео, **Opus-in-Ogg** (`.ogg`) для аудио;
кодек должен соответствовать контейнеру. AV1 и SVC/simulcast — фаза 3.

### `pro/webrtc-subscribe@v1`

Принимает удалённые треки и измеряет их.

| Параметр | Тип | По умолчанию | Описание |
|----------|-----|--------------|----------|
| `id` | string | — | Хендл из connect |
| `sink` | string | `measure` | `measure` считает кадры/пакеты; `record` дополнительно пишет каждый принятый трек на диск |
| `record.dir` | string | — | Каталог для `sink: record` (файлы: `<шаг>-vu<vu>-track<idx>-<тип>.<ext>`; Opus → `.ogg`, VP8 → `.ivf`, H.264 → `.h264`) |
| `jitter_buffer_ms` | ms | выкл. (пакеты доставляются по прибытии) | Jitter-буфер на приёме: пакеты удерживаются до момента воспроизведения по расписанию RTP-таймстампов со смещением на эту задержку и отдаются в порядке sequence-номеров. Цена — рост TTFF/задержки на ту же величину (TTFF включает задержку). Применяется со следующего пакета, на всё соединение |

### `pro/webrtc-stats@v1`

Снимок `getStats` в outputs шага (совместим с проверками `std/check@v1`) и
запуск фонового сэмплирования метрик качества медиа.

### `pro/webrtc-close@v1`

Корректное закрытие с финальным сбросом статистики. Припаркованные
соединения также закрываются в конце итерации VU.

### `pro/webrtc-call@v1`

Композитный P2P-звонок — парность → connect → медиа в обе стороны → hold →
stats → close, одним шагом. Пары VU находят друг друга через эфемерные ключи
[общих переменных](core/shared-variables.md) (объявление
`shared_variables:` не нужно); с `driver: redis` тот же тест масштабируется
на несколько инстансов движка.

| Параметр | Тип | По умолчанию | Описание |
|----------|-----|--------------|----------|
| `pairing.driver` | string | `memory` | Драйвер общих переменных для рандеву (`memory` — в пределах одного движка, `redis` — между инстансами) |
| `pairing.key` | string | — | Пространство имён рандеву; офферы/ансверы живут под `<key>:offer\|answer:<pair_id>` с TTL |
| `pairing.strategy` | string | `adjacent` | `adjacent`: vu 2k-1 звонит vu 2k; `custom`: вы вычисляете оба параметра сами |
| `pairing.pair_id` | string | — | только `custom` — id пары; `${vu}` раскрывается |
| `pairing.role` | string | — | только `custom` — `offer` или `answer` |
| `media` | string | `bidirectional` | Пока только `bidirectional` — для односторонних звонков комбинируйте connect/publish/subscribe |
| `hold` | duration | — | Длительность звонка перед stats + close (например, `30s`) |
| `timeout` | ms | `10000` | Дедлайн ожидания на рандеву (та же семантика, что у subscribe в `std/pubsub@v1`) |
| `tracks` | list | синтетика: Opus-аудио + VP8-видео | Переопределение треков, тот же формат, что у `pro/webrtc-publish@v1` |

Ошибки несут тег стадии (`ice`, `dtls`, `signaling`, `publish`,
`subscribe`, `hold`) в outputs шага и в счётчиках
`webrtc_call_errors_<stage>`. При нечётном числе VU старший нечётный VU
остаётся без пары: он отправляет оффер и отваливается по таймауту —
учитываемая ошибка стадии `signaling`, никаких молчаливых пропусков.

## Метрики

Сетап: `webrtc_ice_duration_ms`, `webrtc_dtls_duration_ms`,
`webrtc_setup_ms`, `webrtc_connect_total` (+ автоматический sibling с долей
ошибок). TTFF: `webrtc_ttff_ms`. Качество медиа (по типу трека):
`webrtc_{audio,video}_rtt_ms`, `webrtc_{audio,video}_jitter_ms`,
`webrtc_{audio,video}_packets_lost_total`,
`webrtc_{audio,video}_bitrate_bps`, `webrtc_frames_decoded_total`.
Перезапуски ICE: `webrtc_ice_restarts_total`.
Композитные звонки: `webrtc_calls_total`, `webrtc_call_duration_ms`,
`webrtc_call_errors_{ice,dtls,signaling,publish,subscribe,hold}`.

## Пример

```yaml
steps:
  - name: publish camera
    use: pro/webrtc-connect@v1
    with:
      signal: whip
      url: https://stream.example.com/whip/cam-${vu}
      bearer: ${WHIP_TOKEN}
    outputs: cam
  - name: send media
    use: pro/webrtc-publish@v1
    with:
      id: ${cam.id}
      tracks:
        - kind: video
          codec: h264
          source: synthetic
          bitrate: 1500kbps
          resolution: 1280x720
        - kind: audio
          codec: opus
          source: synthetic
          bitrate: 64kbps
```

## Дорожная карта

Фаза 2 вышла: `pro/webrtc-call@v1` (композитные P2P-звонки между парами VU,
SDP-рандеву через [общие переменные](core/shared-variables.md) с
настраиваемым правилом парности), файловые источники (`source: file`),
`sink: record`, `signal: library`.
Фаза 3 пока что: настоящий перезапуск ICE (`on_disconnect: restart` +
`webrtc_ice_restarts_total`) и jitter-буфер на приёме (`jitter_buffer_ms`).
Осталось: AV1, SVC/simulcast-слои при публикации.
