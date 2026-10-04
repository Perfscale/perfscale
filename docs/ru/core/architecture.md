# Архитектура

`perfscale-core` — это библиотечный crate: всё, что умеет CLI, доступно
любой программе на Rust, в которую нужно встроить движок нагрузочного
тестирования.

```text
                        ┌─────────────────────────────────────┐
 CLI flags ──────────►  │            ExecutionPlan            │
                        │  K6Script | LocustScript | Native   │
                        └──────────────────┬──────────────────┘
                                           │ runner::execute(plan)
              ┌────────────────────────────┼────────────────────────────┐
              ▼                            ▼                            ▼
      runner::k6                   runner::locust               step::runner
   spawn `k6 run`,             spawn `locust --headless`,    N tokio tasks (VUs)
   stream stdout/err           stream + parse CSV stats      loop over steps
              │                            │                            │
              └────────────────────────────┴────────────────────────────┘
                                           │
                                           ▼
                          mpsc::Receiver<LogLine>
                     { source: stdout|stderr|system, text }
```

## Единственная важная абстракция: `LogLine`

Каждый движок — и внешний подпроцесс, и внутрипроцессный — сводится к одному
и тому же типу вывода:

```rust
pub struct LogLine {
    pub source: LogSource,   // Stdout | Stderr | System
    pub text: String,
}
```

Потребители (CLI, `perfscale serve`, будущий TUI) никогда не задумываются о
том, какой движок породил строку. Поток закрывается по завершении прогона —
отдельного сигнала о завершении нет.

## Единый формат сводки

Все три движка завершают свой поток одинаковым блоком сводки в стиле k6
(`http_req_duration`, `http_req_failed`, `http_reqs`, `vus`, `iterations`),
поэтому последующие парсеры не зависят от движка:

- нативный движок форматирует её из собственных собранных метрик
  (`step::runner::Metrics::summary_lines`)
- раннер locust строит её из файла статистики locust `--csv`
- k6 печатает её сам

## Карта модулей

| Модуль | Ответственность |
|---|---|
| `runner` | `ExecutionPlan`, диспетчер `execute()`, `LogLine`/`LogSource` |
| `runner::k6` | подпроцесс k6: работа с временными скриптами, стриминг, oneshot |
| `runner::locust` | подпроцесс locust: headless-флаги, преобразование CSV → сводка |
| `step` | модель теста: `TestDef`, `Step`, `RunConfig`, парсинг длительности, пресеты |
| `step::runner` | нативный планировщик VU и сбор метрик |
| `step::actions` | диспетчеризация встроенных действий (`std/*`) + реестр пользовательских действий |
| `step::http` | `std/http@v1` + общий HTTP-транспорт (пулы клиентов, замеренный обмен, отчётность) |
| `step::context` | хранилище переменных отдельного VU + интерполяция `${{ }}` |
| `step::resources` | типы хендлов семейств + кэш gRPC reflection поверх реестров соединений |
| `step::ws` / `step::grpc` / `step::db` | протокольные семейства живых соединений |
| `step::graphql` | `std/graphql@v1`: GraphQL по HTTP + валидация схемы через introspection/SDL |
| `step::thresholds` | SLO-гейты уровня прогона `std/thresholds@v1` |
| `step::process` | управляемые дочерние процессы (`std/child_process`, `std/kill_process`) |
| `yaml` | парсинг файлов теста/конфига с валидацией по схеме, `ConfigFile` |
| `schema` | генерация JSON Schema (schemars) для обоих форматов файлов |
| `models` | `RunResult` (результат oneshot-подпроцесса) |

Соседний crate в воркспейсе: **`perfscale-connection`** — обобщённый
реестр именованных соединений (трейт `Connection` + `ConnectionRegistry`),
на котором строится `step::resources`. Ноль зависимостей; пригоден для
любого движка шагов с жизненным циклом connect → park → use → close.

## Конвейер нативного движка

Нативный прогон проходит через эти компоненты по порядку:

```text
 test.yaml + config.yaml (step::yaml, schema-validated)
        │  steps, before/after, vars, run config (vus, duration)
        ▼
 step::runner::run_native ── before: steps once (outputs → ${{ config.* }})
        │
        │  spawn config.vus tokio tasks, each loops until duration expires:
        ▼
 step::context::Context     per VU: vars + ${{ }} interpolation,
        │                   per-VU generator, HTTP client shard
        ▼
 step::actions::execute_action   per step, strictly sequential:
        │   std/http·tcp·udp·ws*·grpc*·db-*·check·sleep·log·file-*·…
        ▼
 step::resources            live handles parked under Connection IDs
        │   (ws-1, grpc-1, grpcs-1, db-1) via perfscale-connection;
        │   drained after every iteration — nothing outlives it
        ▼
 step::runner::Metrics      HDR histograms (fixed memory), counters,
        │                   rates, threshold results
        ▼
 summary + thresholds       k6-compatible text summary streamed as
        │                   LogLines; NativeRunOutcome.thresholds
        ▼
 CLI exit code              non-zero when a severity:fail gate trips
```

Где подключаются расширения:

- **k6 / locust** — это соседние *движки*, а не шаги: `runner::execute`
  диспетчеризует `ExecutionPlan` их раннерам подпроцессов, которые сводятся
  к тому же потоку `LogLine` (диаграмма выше).
- **Семейства gRPC / WebSocket / DB** подключаются как действия `std/*` в
  `step::actions`, а их живые хендлы паркуются в `step::resources`
  (на базе crate `perfscale-connection`) — шаги подключения создают
  идентификаторы `ws-1` / `grpc-1` / `db-1`, на которые пользователь
  ссылается в последующих шагах.
- **Пороги (thresholds)** выполняются как шаги `std/thresholds@v1` в блоке
  `after:`, оценивая гейты по метрикам, собранным за прогон
  (`step::thresholds`); объединённый результат становится кодом выхода CLI.
- **Пользовательские действия** (например, downstream `pro/*`) регистрируют
  `ActionHandler` через `step::actions::register_action` и могут использовать
  `perfscale-connection` для собственных припаркованных хендлов.

Ключевые файлы — по одной строке на каждый:

| Файл | Роль |
|---|---|
| `crates/perfscale-core/src/runner/mod.rs` | диспетчеризация движков, `LogLine` |
| `crates/perfscale-core/src/step/runner.rs` | цикл VU, HDR-метрики, сводка, источник кода выхода |
| `crates/perfscale-core/src/step/context.rs` | интерполяция `${{ }}`, состояние отдельного VU |
| `crates/perfscale-core/src/step/actions.rs` | диспетчеризация действий `std/*` |
| `crates/perfscale-core/src/step/resources.rs` | типы хендлов семейств, связка с реестром |
| `crates/perfscale-connection/src/lib.rs` | паттерн реестра соединений, с документацией |
| `crates/perfscale-cli/src/main.rs` | CLI: парсинг аргументов, запуск плана, печать потока, код выхода |

## Пример встраивания

```rust
use perfscale_core::runner::{self, ExecutionPlan};
use perfscale_core::yaml;

let test = yaml::parse_test_file(&std::fs::read_to_string("test.yaml")?)?;
let config = yaml::parse_config_file(&std::fs::read_to_string("config.yaml")?)?;

let rx = runner::execute(ExecutionPlan::NativeSteps {
    test,
    config: config.run,
    before: config.before,
    after: config.after,
    variables: config.variables,
    quiet: false,
})
.await?;
while let Some(line) = rx.lines.recv().await {
    println!("[{:?}] {}", line.source, line.text);
}
```

## Архитектурные ограничения

- **Никаких проприетарных интеграций.** Всё здесь универсально; задачи
  control plane (аутентификация, пуш метрик, управление флитом) относятся
  к downstream-потребителям этого crate.
- **Внешние движки — подпроцессы, а не линкуемые зависимости.** k6 и locust
  ищутся в `PATH` во время выполнения; отсутствующий бинарник — это
  понятная ошибка, а не зависимость сборки.
- **Ограниченные каналы (512 строк).** Производители блокируются, когда
  потребитель не успевает, — вычитывайте ресивер параллельно с прогоном
  (как делает `execute()`), а не после него.
