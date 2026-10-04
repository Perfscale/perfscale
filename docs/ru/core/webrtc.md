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
| `signal` | string | — | `whip` \| `whep` (`library` — фаза 2) |
| `url` | string | — | WHIP/WHEP-эндпоинт |
| `bearer` | string | — | Bearer-токен эндпоинта |
| `trickle` | bool | `false` | Дотекание ICE-кандидатов через WHIP PATCH; по умолчанию non-trickle (детерминированные метрики сетапа) |
| `on_disconnect` | string | `fail_fast` | `fail_fast` завершает шаг при ICE disconnect; `restart` пытается перезапустить ICE |
| `timeout` | ms | `10000` | Таймаут сигналинга и подключения |

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
| `jitter_buffer_ms` | ms | дефолт стека | Переопределение размера jitter-буфера |

### `pro/webrtc-stats@v1`

Снимок `getStats` в outputs шага (совместим с проверками `std/check@v1`) и
запуск фонового сэмплирования метрик качества медиа.

### `pro/webrtc-close@v1`

Корректное закрытие с финальным сбросом статистики. Припаркованные
соединения также закрываются в конце итерации VU.

## Метрики

Сетап: `webrtc_ice_duration_ms`, `webrtc_dtls_duration_ms`,
`webrtc_setup_ms`, `webrtc_connect_total` (+ автоматический sibling с долей
ошибок). TTFF: `webrtc_ttff_ms`. Качество медиа (по типу трека):
`webrtc_{audio,video}_rtt_ms`, `webrtc_{audio,video}_jitter_ms`,
`webrtc_{audio,video}_packets_lost_total`,
`webrtc_{audio,video}_bitrate_bps`, `webrtc_frames_decoded_total`.

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

Фаза 2: `pro/webrtc-call@v1` (композитные P2P-звонки между парами VU,
SDP-рандеву через [общие переменные](core/shared-variables.md) с
настраиваемым правилом парности), `signal: library`. Файловые источники
(`source: file`) и `sink: record` вышли в фазе 2a. Фаза 3: AV1,
SVC/simulcast-слои при публикации.
