# Документация perfscale

perfscale — это единый CLI для запуска нагрузочных тестов с [k6](https://k6.io),
[locust](https://locust.io) или собственным нативным шаговым движком perfscale —
плюс небольшой локальный dev-сервер для сбора результатов.

## С чего начать

- [Начало работы](getting-started.md) — установка, первый запуск, первые результаты
- [Справочник YAML](yaml-reference.md) — форматы `test.yaml` / `config.yaml`
- [SDK для библиотек](library-sdk.md) — создание WASM-библиотек генераторов
  значений на Rust, TypeScript или Go

## CLI (бинарник `perfscale`)

- [Команды](cli/commands.md) — справочник по `run`, `serve`, `lint` и `schema`
- [Рецепты](cli/examples.md) — готовые примеры для типовых задач
- [Бенчмарки](benchmarks.md) — методология сравнения движков и запуски в CI
- [MCP-сервер](mcp.md) — управление perfscale из AI-агентов по протоколу Model Context Protocol

## Ядро (библиотека `perfscale-core`)

- [Архитектура](core/architecture.md) — как части системы связаны между собой
- [Раннеры](core/runners.md) — k6, locust, JMeter и нативный движок
- [Действия](core/actions.md) — `std/http`, `std/tcp`, `std/udp`, `std/ws*`
  (WebSocket), `std/graphql` (GraphQL), `std/grpc*` (gRPC), `std/db-*` (базы данных SQL), `std/check`,
  `std/sleep`, `std/log`,
  `std/file-*`, `std/child_process`, `std/kill_process`,
  `std/set_shared_variable`, `std/get_shared_variable`, `std/thresholds`
- [Setup и teardown](core/setup-teardown.md) — жизненный цикл `before:`/`after:`,
  поток данных `config.*`/`vars.*`, фоновые процессы
- [Импорты](core/imports.md) — сборка документов теста/конфига из общей
  базы: локальные пути, HTTP-URL, git-репозитории; семантика слияния, кеширование
  и защитный барьер `--allow-remote-import`
- [Руководство по библиотекам](core/libraries.md) — пользовательские генераторы
  значений для токенов `${alias.fn(...)}`: встроенный `@std/random`, WASM-компоненты,
  sandbox на основе capability и создание собственных библиотек с SDK для Rust / TypeScript
  / Go
- [Метрики](core/metrics.md) — `http_req_*`, строки `[stats]`, пользовательские
  счётчики/гистограммы, `--quiet`, пересылка сводки
- [Руководство по WebSocket](core/websocket.md) — сессии, живые соединения, RTT
  сообщений, проверки
- [Руководство по GraphQL](core/graphql.md) — запросы/мутации, валидация схемы
  через интроспекцию
- [Руководство по gRPC](core/grpc.md) — динамические схемы (reflection /
  descriptor set), унарные вызовы, потоки
- [Руководство по разделяемым переменным](core/shared-variables.md) — разделяемое
  изменяемое состояние между VU: счётчики, очереди producer/consumer, барьеры
- [Руководство по метрикам GPU](core/gpu.md) — загрузка/VRAM/температура/потребление
  во время прогона, источники `nvidia-smi` и dcgm-exporter
- [Руководство по JMeter](core/jmeter.md) — headless-запуск существующих планов
  `.jmx`, транслированная k6-совместимая сводка, параметризация, ограничения
- [Docker](core/docker.md) — готовые к запуску образы ghcr.io (варианты slim и
  со встроенным k6), монтирование сценариев, рецепты для CI
- [Kubernetes](core/kubernetes.md) — Helm-чарт агента perfscaled
  (DaemonSet, политика fleet-библиотек, том кеша) и разовые CLI-задачи Jobs

## Для контрибьюторов

- [README репозитория](../README.md) — структура, локальная разработка, сборки релизов
- [Примеры](../examples/) — запускаемые примеры файлов для каждого движка
- [JSON Schemas](../schema/) — сгенерированные схемы для автодополнения в редакторе
