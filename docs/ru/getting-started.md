# Начало работы

## Установка

Через npm — устанавливает автономный бинарный файл для вашей платформы
(Node.js во время выполнения не требуется):

```sh
npm install -g @perfscale/exe
```

Или скачайте бинарный файл для вашей платформы со страницы
[GitHub Releases](https://github.com/Perfscale/perfscale/releases):

| Платформа | Артефакт |
|---|---|
| Linux x86_64 | `perfscale-linux-amd64` (static) |
| Linux ARM64 | `perfscale-linux-arm64` (static) |
| macOS Apple Silicon | `perfscale-darwin-arm64` |
| macOS Intel | `perfscale-darwin-amd64` |
| Windows x86_64 | `perfscale-windows-amd64.exe` |
| Windows ARM64 | `perfscale-windows-arm64.exe` |

```sh
# пример для Linux/macOS
curl -fsSL -o perfscale https://github.com/Perfscale/perfscale/releases/latest/download/perfscale-linux-amd64
chmod +x perfscale
```

Проверьте файл с помощью `sha256sums.txt` из того же релиза. После установки
обновление выполняется одной командой — она проверяет контрольные суммы
и заменяет бинарный файл на месте:

```sh
perfscale self-update
```

Или соберите из исходников:

```sh
cargo build --release -p perfscale-cli
# бинарный файл: target/release/perfscale
```

Сам perfscale не имеет зависимостей во время выполнения. Внешние движки
необязательны и нужны, только если вы их используете:

- **k6** — [руководство по установке](https://k6.io/docs/get-started/installation/)
- **locust** — `pip install locust`
- **нативный движок** — встроен, ничего устанавливать не нужно

## Первый запуск (внешние инструменты не нужны)

Создайте `test.yaml`:

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/Perfscale/perfscale/main/schema/test.schema.json
steps:
  - name: homepage
    use: std/http@v1
    with:
      method: GET
      url: https://httpbin.org/get
    check:
      status: 200
    outputs: resp

  - name: log status
    use: std/log@v1
    with:
      message: "got status ${{ resp.status }}"
```

Создайте `config.yaml`:

```yaml
vus: 5
duration: 30s
```

Запустите:

```sh
perfscale run -f test.yaml -c config.yaml
```

Вы увидите живой вывод по каждому запросу, а затем сводку в формате,
совместимом с k6:

```text
vus....................: 5 min=1 max=5
iterations..............: 142 4.73/s
http_req_duration......: avg=213.40ms p(50)=201ms p(90)=280ms p(95)=310ms p(99)=352ms min=180ms max=390ms
http_req_failed........: 0.00%
http_reqs..............: 142 4.73/s
```

Помимо фиксированных `vus`/`duration`, нативный движок также выполняет
профили нагрузки в стиле k6: нарастание VU через `stages:` и arrival-rate
через `arrival:` — см. [Профили нагрузки](yaml-reference.md#load-profiles).

## Запуск скриптов k6 или locust

Уже есть скрипты? Просто укажите их perfscale — результат будет выведен
в том же едином формате сводки независимо от движка:

```sh
perfscale run --k6 script.js
perfscale run --locust locustfile.py --host https://target.example.com
```

Для locust параметр `-c config.yaml` сопоставляет `vus`/`duration`
с опциями locust `--users`/`--spawn-rate`/`--run-time`.

## Сбор результатов из нескольких запусков

Запустите локальный dev-сервер в одном терминале:

```sh
perfscale serve            # слушает порт :7999
```

Затем отправляйте результаты запусков на него откуда угодно:

```sh
perfscale run -f test.yaml -c config.yaml --report http://localhost:7999
```

Сервер выводит каждый входящий пакет сводок. Пересылается только сводка
метрик — полный лог итераций никогда не отправляется.

## Дальнейшие шаги

- [Справочник по YAML](yaml-reference.md) — все поля обоих форматов файлов
- [Команды CLI](cli/commands.md) — все флаги
- [Рецепты](cli/examples.md) — использование в CI, пресеты, многошаговые сценарии, WebSocket, gRPC
