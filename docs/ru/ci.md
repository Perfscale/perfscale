# CI-конвейеры

perfscale изначально работает в неинтерактивном (headless) режиме, поэтому
он напрямую встраивается в CI-конвейер: сводка запуска выводится в stdout,
код завершения управляет исходом задания, а `--summary-export` записывает
машиночитаемый JSON (или Markdown-таблицу для интерфейса сводки задания)
для последующих шагов.

## GitHub Actions

Официальный composite action —
[**Perfscale/github-action**](https://github.com/Perfscale/github-action) —
устанавливает закреплённый релиз perfscale (с проверкой контрольной суммы по
файлу `sha256sums.txt` из релиза), запускает ваш тест, выводит таблицу
метрик в **сводку задания** (job summary) и упаковывает сводку + метрики в
zip-артефакт:

```yaml
- uses: Perfscale/github-action@v1
  id: loadtest
  with:
    file: test.yaml        # native engine; or: k6 / locust / jmeter
    config: config.yaml
- uses: actions/upload-artifact@v4
  with:
    name: perfscale-output
    path: ${{ steps.loadtest.outputs.output-file }}
```

Один входной параметр на движок — `k6`, `locust`, `jmeter` или `file`
(нативный) — соответствует соответствующему флагу `perfscale run`. Установка
бинарных файлов движков остаётся на ответственности workflow (action
устанавливает только сам perfscale): k6 и locust ставятся одним пакетом,
для JMeter нужна Java (предустановлена на раннерах, размещаемых GitHub)
плюс tar-архив дистрибутива — в README action есть готовый шаг установки,
который можно просто скопировать.

Полезные выходные данные для проверок (gating): `summary-json` (запросы,
RPS, перцентили задержки, доля ошибок + метаданные движка/VU/длительности)
и `exit-code`:

```yaml
- name: Gate on p95
  run: |
    p95=$(jq '.summary.p95_ms' "${{ steps.loadtest.outputs.summary-json }}")
    awk "BEGIN { exit !(${p95} < 500) }" || { echo '::error::p95 over budget'; exit 1; }
```

**Живое демо:** [Perfscale/perfscale-demo](https://github.com/Perfscale/perfscale-demo)
— публичный репозиторий-песочница: каждый push запускает нативный движок и
JMeter против локальной цели и публикует таблицы сводки задания и артефакты,
так что вы можете увидеть результат, ничего не настраивая.

## GitLab CI

Для GitLab есть репозиторий с шаблонами —
[**Perfscale/gitlab-ci**](https://github.com/Perfscale/gitlab-ci) — с
готовыми include-файлами `.gitlab-ci.yml` и примерами для тех же движков.

## Любой другой CI

Плагин не нужен — установите бинарный файл и запустите:

```sh
curl -fsSL https://github.com/Perfscale/perfscale/releases/latest/download/perfscale-linux-amd64 \
  -o perfscale && chmod +x perfscale
./perfscale run -f test.yaml -c config.yaml \
  --summary-export summary.json
```

Коды завершения повторяют коды CLI: `0` — запуск выполнен (отдельные
неудачные запросы/проверки на это не влияют — для gating используйте
`summary.json`), `1` — запуск не удалось выполнить, `2` — неверные
аргументы. Одно сознательное исключение: нарушение порога
`std/thresholds@v1` с `severity: fail` завершает процесс с ошибкой после
экспорта сводки, поэтому CI «краснеет» при нарушении SLO.
