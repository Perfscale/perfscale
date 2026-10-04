# Kubernetes

Есть два способа запускать perfscale в кластере: разовые или расписанием
**Jobs** для самого CLI (Helm-чарт ниже) и **агент perfscaled** в виде
DaemonSet (он присоединяется к вашему флоту и забирает задачи платформы).

## Чарт perfscale (Jobs и CronJobs)

Репозиторий [Perfscale/charts](https://github.com/Perfscale/charts)
поставляет CLI в виде чарта — по умолчанию разовый Job, а при заданном
`schedule` — CronJob:

```sh
helm repo add perfscale https://raw.githubusercontent.com/Perfscale/charts/main/
helm install smoke perfscale/perfscale                                    # one-shot Job
helm install nightly perfscale/perfscale --set schedule="0 3 * * *"       # CronJob
```

Сценарий берётся из встроенного ConfigMap (`--set-file
scenario.test=my.test.yaml`) или из существующего
(`--set scenario.existingConfigMap=…`). Сценарий по умолчанию —
самодостаточный smoke-тест на SQLite, так что `helm install` работает из
коробки. Варианты движка (`image.flavor: k6|jmeter|locust|full`), лимиты
ресурсов и дополнительные аргументы CLI задаются обычными values — см.
`values.yaml` чарта.

### Библиотеки в чарте (RFC 005)

Включите `cache.enabled`, чтобы получить PVC `library-cache`,
смонтированный в `/var/lib/perfscale/cache` (`PERFSCALE_CACHE_DIR`):
заполните его заранее с помощью `perfscale install` (из разового Job на
том же claim), и каждый запуск будет читать библиотеки из общего кеша. С
отключённым кешем библиотеки едут на том же томе, что и сценарий, как в
Docker, либо вжигаются в автономный бинарник.

## Разовые запуски обычными Jobs

Helm не нужен — [образы движков](docker.md) работают как обычные Jobs.
Смонтируйте сценарий (ConfigMap для небольших тестов, иначе PVC) и
запустите:

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: perfscale-smoke
spec:
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: perfscale
          image: ghcr.io/perfscale/perfscale:latest
          args: ["run", "-f", "/work/test.yaml", "-c", "/work/config.yaml"]
          volumeMounts:
            - name: scenario
              mountPath: /work
      volumes:
        - name: scenario
          configMap:
            name: perfscale-smoke
```

Библиотеки работают так же, как в Docker: локальные компоненты едут на
смонтированном томе (пути разрешаются относительно объявляющего файла);
удалённым нужен общий кеш установки (`PERFSCALE_CACHE_DIR` на PVC),
заполненный командой `perfscale install`, либо
[вжигаемый автономный бинарник](docker.md#wasm-libraries) со встроенными
библиотеками.

## Агент perfscaled (DaemonSet)

Агент платформы запускается по одному поду на узел в виде DaemonSet и
присоединяется к вашему флоту. Его Helm-чарт припаркован до тех пор, пока
агент не выпустит контейнерный образ — следите за
[Perfscale/charts#1](https://github.com/Perfscale/charts/issues/1); исходники
чарта до парковки есть в git-истории репозитория
(`git show 9718911:perfscaled/values.yaml`).

Агенты применяют fail-closed политику флота к `libraries:`, объявленным в
задачах:

- `PERFSCALE_LIBRARY_CAPABILITIES` — потолок флота (`fs`, `clock`,
  `net:<host>` — glob-шаблоны). По умолчанию: **none** — библиотеки
  запускаются, но с нулевым набором capability. Задача, запрашивающая
  больше, отклоняется с ошибкой валидации и никогда не понижается молча.
- `PERFSCALE_LIBRARY_ALLOW_DIGESTS` — необязательный allowlist sha256
  артефактов, вообще допущенных на флоте.
- Удалённые библиотеки никогда не скачиваются во время выполнения: они
  устанавливаются по дайджесту (`perfscaled library install <url> --sha256 …`
  или `POST /api/v1/libraries/install`) в content-addressed кеш, а
  закешированные байты перехешируются при принятии задачи.

## Ссылки

- [Запуск perfscale в Docker](docker.md) — образы, варианты, монтирование
- [Руководство по библиотекам](libraries.md) — сама функциональность библиотек
- [Perfscale/charts](https://github.com/Perfscale/charts) — исходники чарта
