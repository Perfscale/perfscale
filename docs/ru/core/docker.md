# Запуск perfscale в Docker

perfscale публикует готовые к запуску образы в GitHub Container Registry при
каждом релизе — без установки, без тулчейна Rust, без необходимости управлять
бинарниками раннеров. Смонтируйте свои сценарии в контейнер и запустите:

```sh
docker run --rm -v "$PWD:/work" -w /work ghcr.io/perfscale/perfscale:latest \
  run -f test.yaml -c config.yaml
```

Всё, что идёт после имени образа, передаётся напрямую в CLI `perfscale`
(entrypoint образа — `perfscale`), поэтому все команды и флаги работают так
же, как при локальной установке: `run`, `lint`, `serve`, `man`, `--help`.

## Варианты образов

| Образ | Состав | Для чего использовать |
|---|---|---|
| `ghcr.io/perfscale/perfscale:<version>` | только perfscale (~50 МБ) | Сценарии на нативном движке шагов (`-f`/`-c`) |
| `ghcr.io/perfscale/perfscale:<version>-k6` | + k6 | Существующие скрипты k6 (`--k6`) |
| `ghcr.io/perfscale/perfscale:<version>-jmeter` | + JMeter (headless JRE) | Существующие `.jmx`-планы (`--jmeter`) |
| `ghcr.io/perfscale/perfscale:<version>-locust` | + locust (Python) | Существующие locust-файлы (`--locust`) |
| `ghcr.io/perfscale/perfscale:<version>-full` | + k6, JMeter, locust | Всё сразу |

Все варианты мультиархитектурные (`linux/amd64` и `linux/arm64`) и помечаются
тегами `X.Y.Z`, `X.Y` и `latest` (варианты с раннерами используют те же теги
со своим суффиксом: `latest-k6`, `latest-jmeter`, `latest-locust`,
`latest-full`). Версии раннеров зафиксированы в образе (k6, JMeter, locust —
точные версии см. в `docker/image.Dockerfile`). В CI фиксируйте точную версию
perfscale — считайте `latest` удобством для локального использования.

Нужен другой набор инструментов? Соберите образ поверх любого варианта — см.
[Расширение образа](#расширение-образа).

## Монтирование сценариев

Контейнер работает под непривилегированным пользователем (`perfscale`, uid
10001) и не имеет собственной рабочей директории, поэтому стандартный паттерн —
bind-монтирование:

```sh
# Scenarios live in the current directory
docker run --rm -v "$PWD:/work" -w /work ghcr.io/perfscale/perfscale:latest \
  run -f test.yaml -c config.yaml

# Read-only mount is fine for running tests
docker run --rm -v "$PWD:/work:ro" -w /work ghcr.io/perfscale/perfscale:latest \
  lint test.yaml
```

Bind-монтирование принадлежит вашему пользователю на хосте, поэтому всё, что
perfscale *записывает* (`--summary-export`, `std/file-write@v1`), создаётся от
uid 10001 контейнера. Чтобы файлы оставались вашими, запускайте со своим uid и
монтированием, доступным на запись:

```sh
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work \
  ghcr.io/perfscale/perfscale:latest run -f test.yaml -c config.yaml \
  --summary-export summary.json
```

Секреты и конфигурация передаются теми же механизмами, что и при локальной
установке, — передавайте переменные окружения через `-e`
(`-e API_TOKEN=...`) и ссылайтесь на них как `${{ env.API_TOKEN }}` в сценарии.

## WASM-библиотеки

Библиотеки `${alias.fn(...)}` (RFC 005) работают в контейнере — поддержка WASM
всегда вкомпилирована в движок. Что монтировать, зависит от источника:

**Локальные компоненты** (`use: ./libs/fixer-ids.wasm`) не требуют ничего
дополнительного: пути разрешаются относительно объявляющего файла, поэтому
обычного монтирования `$PWD:/work` достаточно, если сохранена относительная
структура каталогов. Библиотека с `capabilities: [fs]` получает preopen только
на чтение для `fs_root` запуска — по умолчанию это директория файла
конфигурации, то есть внутри `/work`.

**Удалённые компоненты** (`https://…`, `git+…`) скачиваются только командой
`perfscale install`, которая записывает `perfscale.lock` рядом с конфигом (в
смонтированной директории), а кэш артефактов — в `~/.cache/perfscale` на хосте.
Держите кэш внутри смонтированной директории, чтобы хост и контейнер
согласовывались между собой:

```sh
# host: install into a project-local cache (perfscale.lock lands next to the config)
PERFSCALE_CACHE_DIR="$PWD/.perfscale-cache" perfscale install test.yaml config.yaml

# container: same cache dir, fully offline run
docker run --rm -v "$PWD:/work" -w /work \
  -e PERFSCALE_CACHE_DIR=/work/.perfscale-cache \
  ghcr.io/perfscale/perfscale:latest run -f test.yaml -c config.yaml
```

`install` также *прожигает* (AOT-прекомпилирует) каждую библиотеку — удалённую
или локальную — в кэш как `<sha256>.cwasm` рядом с `.wasm`-артефактом, поэтому
контейнерные запуски пропускают и компиляцию Cranelift при каждом запуске:
движок десериализует прекомпилированный артефакт за миллисекунды вместо
компиляции компонента при загрузке. Прожиг при установке — рекомендательный:
ошибка прекомпиляции понижается до предупреждения, но никогда не приводит к
неудачной установке, — а запуск без валидного `.cwasm` откатывается к
компиляции из `.wasm`-исходника при загрузке (JIT-путь: более медленный
холодный старт, идентичное поведение и метрики после запуска). Устаревший
артефакт (прожжённый более старой сборкой perfscale — в заголовке записаны
версия wasmtime и целевой триплет) игнорируется таким же образом, поэтому
после обновления образа повторите `install`. Для всего этого директория кэша
должна быть общей между установкой и запуском, как показано выше.

**Автономные бинарники** — `perfscale burn` идёт дальше и встраивает
библиотеки сценария в копию самого бинарника (AOT-артефакты за трейлером
`PFSEMBED`), так что монтировать не нужно ничего, кроме YAML:

```sh
# on the host (match the target architecture), then ship one file:
perfscale burn -f test.yaml -c config.yaml -o perfscale+libs
```

Производному бинарнику не нужны `.wasm`-файлы, кэш или `perfscale.lock` на
машине, где он запускается, — см. [`perfscale burn`](../cli/commands.md#perfscale-burn).

## Внешние раннеры (k6 / JMeter / locust)

Варианты с раннерами содержат соответствующий движок, поэтому существующие
скрипты и планы запускаются без какой-либо установки на хосте:

```sh
# k6
docker run --rm -v "$PWD:/work" -w /work \
  ghcr.io/perfscale/perfscale:latest-k6 run --k6 script.js

# JMeter (.jmx plans)
docker run --rm -v "$PWD:/work" -w /work \
  ghcr.io/perfscale/perfscale:latest-jmeter run --jmeter plan.jmx

# locust
docker run --rm -v "$PWD:/work" -w /work \
  ghcr.io/perfscale/perfscale:latest-locust run --locust locustfile.py --host https://example.com
```

Образ `-full` содержит все три — используйте его, когда в одной задаче
запускается несколько движков или когда вы не хотите задумываться, какой
вариант нужен скрипту.

## В CI

### GitHub Actions

```yaml
jobs:
  load:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
      - name: Run load test
        run: |
          docker run --rm -v "$PWD:/work" -w /work \
            ghcr.io/perfscale/perfscale:0.17.0 \
            run -f load.test.yaml -c load.config.yaml
```

Пороговые проверки (`std/thresholds@v1`) заставляют контейнер завершаться с
ненулевым кодом при нарушениях SLO, поэтому шаг проваливает задачу так же, как
это сделал бы локальный запуск.

### Kubernetes

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: perfscale-load
spec:
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: perfscale
          image: ghcr.io/perfscale/perfscale:0.17.0
          args: ["run", "-f", "/scenarios/load.test.yaml", "-c", "/scenarios/load.config.yaml"]
          volumeMounts:
            - name: scenarios
              mountPath: /scenarios
      volumes:
        - name: scenarios
          configMap:
            name: perfscale-scenarios
```

## Расширение образа

Нужны дополнительные инструменты, которых нет в вариантах, — плагины JMeter,
собственные CA-сертификаты, другая сборка k6? Используйте любой вариант как
базовый — бинарники остаются на месте, вы добавляете то, что нужно:

```dockerfile
FROM ghcr.io/perfscale/perfscale:0.17.0-jmeter

USER root
# Example: JMeter plugins land in lib/ext of the installation
RUN curl -fsSL -o /opt/apache-jmeter-5.6.3/lib/ext/jpgc-casutg.jar \
      https://repo1.maven.org/maven2/kg/apc/jmeter-plugins-casutg/2.10/jmeter-plugins-casutg-2.10.jar
USER perfscale
```

## Проверка релизного образа

Образы собираются из тех же статических musl-бинарников, что публикуются в
GitHub Releases, а релизный workflow перед публикацией проводит смоук-тесты
каждого варианта: реальный сценарий на нативном движке в slim-образе и запуск
каждого бинарника раннера (`k6 version`, `jmeter --version`,
`locust --version`) в его варианте и в `-full`. Чтобы перепроверить локально:

```sh
docker run --rm ghcr.io/perfscale/perfscale:0.17.0 --version
docker run --rm --entrypoint k6 ghcr.io/perfscale/perfscale:0.17.0-k6 version
```

## Kubernetes

Платформенный агент (perfscaled) поставляется как Helm-чарт в
[Perfscale/charts](https://github.com/Perfscale/charts): DaemonSet, запускающий
по одному агенту на узел (образ `ghcr.io`, непривилегированный пользователь,
корневая файловая система только для чтения, секрет через `existingSecret`).
Агент самостоятельно регистрируется в controlplane и забирает задачи.

```sh
helm install perfscaled oci://ghcr.io/perfscale/charts/perfscaled \
  --set-file secret...  # or --set existingSecret=perfscaled-env
```

Примечания к чарту:

- `readOnlyRootFilesystem: true` — пути, доступные на запись, предоставляются
  через тома. Кэш библиотек (RFC 005) размещается на выделенном томе
  `library-cache` (по умолчанию `emptyDir`; переключите на `hostPath` в
  `values.yaml`, чтобы переживать перепланирование подов), смонтированном в
  `/var/lib/perfscale/cache` и подключённом как `PERFSCALE_CACHE_DIR` — так
  библиотеки, установленные через `perfscaled library install` /
  `POST /api/v1/libraries/install`, сохраняются между перезапусками
  контейнера.
- Политика библиотек флота задаётся через переменные окружения агента:
  `PERFSCALE_LIBRARY_CAPABILITIES` (потолок флота, по умолчанию — никаких) и
  `PERFSCALE_LIBRARY_ALLOW_DIGESTS` (опциональный список разрешённых sha256) —
  агент пересекает их с разрешениями каждой задачи и отклоняет задачи,
  превышающие лимит, с ошибкой валидации.

Для разовых нагрузочных запусков в кластере (без агента) образы движка выше
отлично работают как
[Job](https://kubernetes.io/docs/concepts/workloads/controllers/job/):
смонтируйте сценарий через ConfigMap/PVC и используйте entrypoint образа, как в
примерах здесь.

## Ограничения

- **Версии раннеров зафиксированы** — чтобы запустить другую версию
  k6/JMeter/locust, расширьте образ (см. выше).
- **JMeter пишет `jmeter.log` (и любые результаты `.jtl`) в рабочую
  директорию** — дайте ему монтирование, доступное на запись
  (`-v "$PWD:/work" -w /work`, или `--user "$(id -u):$(id -g)"`, чтобы файлы
  оставались вашими).
- **`perfscale serve` требует опубликованного порта**: добавьте `-p 7999:7999`
  и отправляйте POST с отчётами на адрес контейнера, а не на `localhost`.
- **Файловые действия ограничены контейнером**: `std/file-read@v1`/`std/file-write@v1`
  видят файловую систему контейнера — смонтируйте через bind то, что нужно
  сценарию.
