# Setup и teardown

Нативный запуск — это не только цикл VU. Конфигурационный файл может обрамлять
нагрузку одноразовыми шагами: `before:` подготавливает окружение (получить
токен, запустить локальный сервер), `after:` очищает его — на любом пути
завершения.

## Жизненный цикл запуска

```text
before steps (once, in order, fail-fast)
      │
      ▼
VU loop (steps × iterations until duration expires)
      │
      ▼
after steps (once, best-effort)          ← runs on EVERY exit path:
      │                                    normal finish, failed run,
      ▼                                    failed before, Ctrl-C/SIGTERM
auto-kill of remaining managed processes
      │
      ▼
summary
```

- Шаги `before:` выполняются **один раз**, по порядку, до создания любого VU.
  Если какой-либо из них завершается с ошибкой, запуск прерывается до создания
  VU (`Setup failed, aborting run: …`) — сломанная подготовка привела бы к
  одинаковому провалу каждой итерации.
- Шаги `after:` выполняются **всегда**: после нормального завершения, после
  неудачного запуска, после неудачного `before:` (шаг подготовки мог уже
  запустить что-то, что teardown как раз и существует, чтобы убрать) и после
  Ctrl-C/SIGTERM. В отличие от `before:`, неудачный шаг очистки логируется
  (`teardown step '<name>' failed (continuing)`), а оставшиеся шаги всё равно
  выполняются — частичная очистка лучше, чем никакой.
- После teardown каждый управляемый процесс, всё ещё живой в реестре запуска,
  **останавливается автоматически** (SIGTERM с эскалацией до SIGKILL после
  периода ожидания, вся группа процессов). Забытый `kill_process` никогда не
  приводит к утечке сервера.
- Помимо очистки, в `after:` живут SLO-гейты уровня запуска: шаг
  `std/thresholds@v1` видит всё, что собрал запуск, и определяет по этому код
  завершения (см. [actions.md](actions.md#stdthresholdsv1)).
- Сводка метрик печатается последней.

## Поток данных: `vars.*` и `config.*`

Шаги питаются из двух блоков конфигурации:

| Блок | Доступен как | Виден |
|---|---|---|
| `variables:` | `${{ vars.<key> }}` | шагам `before`, тестовым шагам, шагам `after` |
| `outputs` шага `before:` | `${{ config.<name>.<field> }}` | тестовым шагам и шагам `after` |

```yaml
variables:
  region: eu-west

before:
  - uses: std/http@v1
    with:
      url: "https://api.example.com/token?region=${{ vars.region }}"
    outputs: auth            # → ${{ config.auth.status }} etc. later on
```

- Шаг `before` видит `${{ vars.* }}` и выходные данные **предыдущих** шагов
  подготовки (каждый под своим именем `outputs`).
- Тестовые шаги видят `config.*` и `vars.*` — но никогда не видят выходные
  данные друг друга между VU (контексты VU независимы).
- Шаги `after` видят те же `config.*` и `vars.*`, что и тестовые шаги, поэтому
  шаг очистки может ссылаться ровно на то, на что ссылался тест.
- Интерполяция всегда возвращает **строку**: числовое выходное значение вроде
  `${{ config.keeper.port }}` доходит до action как `"8080"`. Actions,
  принимающие числа, принимают и строковую форму.

## Что не работает в `before:`

Живые соединения (`std/ws-connect@v1`, `std/grpc-connect@v1` и семейство
stream) привязаны к контексту, который их создал. Контекст подготовки
исчезает до старта первого VU, поэтому соединение, открытое в `before:`, уже
мёртво к моменту, когда тестовый шаг запросит его id — вместо этого открывайте
соединения на каждой итерации VU (они освобождаются в конце итерации).

Процессы — исключение: `std/child_process@v1` регистрирует свой дочерний
процесс в реестре **с областью видимости запуска**, и именно поэтому сервер,
запущенный в `before:`, остаётся живым и доступным для остановки на протяжении
всего запуска.

## Фоновые процессы от начала до конца

Каноническая пара setup/teardown: запустить локальный сервис в `before:`,
обращаться к нему из теста, остановить в `after:`. Здесь — мок «position
keeper» для сценария CFD-трейдинга:

```yaml
# config.yaml
vus: 20
duration: 5m
allow_process_actions: true        # fail-closed gate, default false

before:
  - name: position-keeper
    uses: std/child_process@v1
    with:
      command: python3
      args: ["-m", "http.server", "8080"]
      port: 8080                   # echoed to outputs; port: 0 auto-assigns
      waitUntil:                   # block the step until the server is ready
        port_open: 8080
        timeout: 15s
      restart: on-failure          # supervisor restarts crashes (max 3, 1s apart)
    outputs: keeper                # → ${{ config.keeper.port }} etc.

after:
  - name: stop-keeper
    uses: std/kill_process@v1
    with:
      name: keeper                 # registry lookup — always the current pid
      signal: TERM
```

```yaml
# test.yaml
steps:
  - uses: std/http@v1
    with:
      url: "http://127.0.0.1:${{ config.keeper.port }}/positions"
    check:
      status: 200
```

Что происходит под капотом:

- **Стриминг**: stdout/stderr дочернего процесса стримятся в лог запуска с
  префиксом `position-keeper: ` (тот же формат, что и у раннера k6) и
  накапливаются в ограниченных tail-буферах (по умолчанию 64 КиБ на поток,
  настраивается параметром `buffer_kb`).
- **Супервизия**: с `restart: on-failure` (или `always`) задача-супервизор
  следит за дочерним процессом и перезапускает его после `backoff_ms` (по
  умолчанию 1000), не более `max_restarts` (по умолчанию 3) раз, логируя каждый
  перезапуск.
- **Выходные данные — это снимок**:
  `{ pid, ppid, pgid, port, stdout, stderr, restart_count }`. `ppid` — это сам
  perfscale; `pgid` — группа процессов дочернего процесса. После перезапуска
  сохранённый `pid` устаревает — именно поэтому `std/kill_process@v1` следует
  адресовать процесс по `name` (имя шага или имя `outputs`): поиск в реестре
  всегда разрешает *текущий* pid. `pid:` (сырой pid ОС) существует как
  best-effort лазейка для процессов вне реестра.
- **Автоостановка**: даже без шага `after:` keeper был бы остановлен по
  завершении запуска. Явный `kill_process` нужен для чистой и своевременной
  остановки — и для возможности проверить её утверждением.

Полные справочники параметров см. в
[`std/child_process@v1`](actions.md#stdchild_processv1) и
[`std/kill_process@v1`](actions.md#stdkill_processv1), а запускаемый вариант —
в
[examples/with-processes.config.yaml](../examples/with-processes.config.yaml).

## waitUntil — гейты готовности

Шаг `child_process` блокируется до готовности процесса (или до истечения
таймаута гейта). Две формы:

```yaml
# Object form — all listed matchers must hold:
waitUntil:
  stdout_contains: "Serving HTTP"    # substring in captured stdout
  stderr_contains: "listening"       # substring in captured stderr
  stdout_matches: "on port \\d+"     # regex against captured stdout
  stderr_matches: "err\\d+"          # regex against captured stderr
  port_open: 8080                    # TCP connect to 127.0.0.1:<port> works;
                                     # 0 = the step's own `port`
  timeout: 15s                       # duration string, default 30s
  on_timeout: fail                   # fail (default) | continue

# String form — one matcher, defaults for the rest:
waitUntil: 'contains(stdout, "Serving HTTP")'
waitUntil: 'matches(stderr, "err\\d+")'
waitUntil: 'port_open(8080)'
```

- С `on_timeout: fail` невыполненный гейт проваливает шаг (а значит, и запуск,
  если он используется в `before:`); `continue` логирует промах и продолжает
  выполнение.
- Процесс, который **завершился до наступления готовности**, немедленно
  проваливает шаг со своим кодом завершения и хвостом stderr — без ожидания
  таймаута.

## Прерывания (SIGINT / SIGTERM)

Двухступенчатая семантика:

- **Первый** SIGINT (Ctrl-C) или SIGTERM: цикл VU останавливается между шагами,
  и запуск проходит через обычный teardown — шаги `after:`, автоостановка
  процессов, сводка. Об этом сообщает строка в логе:
  `Interrupt received — stopping load, running teardown (interrupt again to
  force-quit)`.
- **Второй** сигнал: немедленный `exit(130)` — сам teardown мог зависнуть, и
  оператор явно хочет выйти. Всё, что к этому моменту ещё не остановлено, может
  утечь, поэтому лучше дождаться первой стадии.

## Безопасность и переносимость

- **Fail-closed гейт**: оба process actions завершаются с ошибкой
  `process actions disabled (allow_process_actions is false)`, если конфиг явно
  не включил их через `allow_process_actions: true` — список шагов из
  недоверенного источника не может порождать OS-процессы или посылать им
  сигналы. Тот же паттерн, что и `allow_file_actions` для файловых actions.
- **Группы процессов (unix)**: каждый дочерний процесс возглавляет свою группу
  (`pgid == pid`), поэтому остановка с `tree: true` посылает сигнал точно в
  группу дочернего процесса — она никогда не может задеть perfscale или его
  родителя.
- **Не-unix**: ни POSIX-сигналов, ни групп процессов нет.
  `std/kill_process@v1` по `name` завершает только непосредственный дочерний
  процесс (`tree` не действует); `pid:` не поддерживается. Всё остальное —
  захват вывода, супервизия перезапусков, `waitUntil`, автоостановка —
  работает так же.
