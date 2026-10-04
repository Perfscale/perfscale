# Метрики GPU

Нативный движок умеет на протяжении всего прогона снимать показания GPU хоста —
загрузку, VRAM, температуру и потребление — и помещает секцию `gpu` в сводку
прогона. Это нужно прежде всего для **нагрузочного тестирования LLM**: при
работе с [`std/llm@v1`](llm.md) против локального сервера (Ollama, vLLM, …)
GPU и *есть* тестируемая система, и именно корреляция TTFT/токенов в секунду
с загрузкой SM и нехваткой памяти позволяет отличить «модель насыщена» от
«сервер неправильно настроен».

Сбор работает в режиме **best-effort**: отсутствие GPU, отсутствие бинарника
`nvidia-smi` или недоступный экспортер приводят к одному предупреждению в
начале прогона, после чего прогон продолжается без метрик GPU — он никогда
не завершается ошибкой из-за этого.

## Конфигурация

Блок `gpu:` находится в [конфиге прогона](../yaml-reference.md#config--c-configyaml)
рядом с `vus`/`duration` (только нативный движок):

```yaml
vus: 10
duration: 5m
gpu:
  enabled: true
  interval_ms: 1000      # default 1000 (min 10)
  source: nvidia-smi     # nvidia-smi (default) | dcgm | powermetrics (macOS)
  dcgm_url: http://127.0.0.1:9400/metrics  # for source: dcgm
  devices: [0, 1]        # optional; default — every GPU the source reports
```

| Поле | Значение по умолчанию | Описание |
|---|---|---|
| `enabled` | `false` | Главный переключатель; без него секция игнорируется |
| `interval_ms` | `1000` | Интервал сэмплирования (один снимок на GPU за тик) |
| `source` | `nvidia-smi` | `nvidia-smi` вызывает бинарник; `dcgm` опрашивает HTTP-эндпоинт dcgm-exporter; `powermetrics` снимает показания GPU/ANE Apple Silicon на macOS (требует root) |
| `dcgm_url` | `http://127.0.0.1:9400/metrics` | Эндпоинт метрик dcgm-exporter (только для `source: dcgm`) |
| `devices` | все | Ограничить сэмплирование указанными индексами GPU |

## Источники

**`nvidia-smi`** (по умолчанию) — раз в тик выполняет
`nvidia-smi --query-gpu=index,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw --format=csv,noheader,nounits`.
Работает везде, где установлен драйвер NVIDIA, дополнительный демон не нужен.
Поля, которые драйвер сообщает как `N/A` (например, потребление на некоторых
виртуализованных GPU), записываются как отсутствующие, а не как ноль.

**`dcgm`** — HTTP GET на `dcgm_url` и разбор текстового формата Prometheus от
[dcgm-exporter](https://github.com/NVIDIA/dcgm-exporter):
`DCGM_FI_DEV_GPU_UTIL`, `DCGM_FI_DEV_FB_USED`/`DCGM_FI_DEV_FB_FREE` (общий
объём VRAM вычисляется как used+free), `DCGM_FI_DEV_GPU_TEMP`,
`DCGM_FI_DEV_POWER_USAGE`. Лучше подходит для GPU-серверов и Kubernetes, где
dcgm-exporter обычно уже запущен.

**`powermetrics`** (macOS, Apple Silicon) — раз в тик выполняет
`sudo powermetrics --samplers cpu_power,gpu_power -n 1 -f plist`. Встроенный
GPU (индекс 0) получает настоящий показатель загрузки (active residency %) и
потребление по своей линии питания. **У Neural Engine (ANE/NPU) нет публичного
API загрузки на macOS** — единственный сигнал — его потребление, — поэтому
ANE приходит как `ane_power_w` наряду с `cpu_power_w`/`package_power_w` и
отображается отдельным рядом (в том числе в during_run). Память и температура
остаются отсутствующими: у unified memory нет понятия VRAM, а `powermetrics`
сообщает тепловое *давление*, а не °C. Учтите, что LLM-инференс на Mac
(llama.cpp, Ollama, MLX) выполняется на GPU через Metal — ANE задействуют
только модели CoreML, скомпилированные под него, поэтому в первую очередь
смотрите на `gpu_utilization_pct`, а `ane_power_w` воспринимайте как
косвенный индикатор активности NPU.

`powermetrics` требует root. Либо запускайте CLI под sudo, либо дайте
пользователю, запускающему нагрузочный тест, беспарольный sudo только для
этой утилиты:

```sh
echo "$USER ALL=(root) NOPASSWD: /usr/bin/powermetrics" | sudo tee /etc/sudoers.d/powermetrics
```

Без root прогон выводит одно предупреждение и продолжается без метрик GPU
(обычный контракт best-effort).

Замечание о частоте сэмплирования: каждый вызов `powermetrics -n 1` измеряет
окно длиной `gpu.interval_ms` (ограничено диапазоном 1s–60s) и блокируется
примерно на длительность окна плюс ~1s накладных расходов на запуск, поэтому
эффективная частота — это тик плюс окно плюс накладные расходы (~2s при тике
1s по умолчанию). Держите `interval_ms` на уровне 5s и выше для почти
непрерывного покрытия окнами.

## Вывод

Пока работают виртуальные пользователи (VU), сэмплер делает один снимок на
GPU за тик (первый — сразу в начале прогона). После сводки метрик прогон
печатает компактный блок:

```text
gpu: 1 device, 300 samples every 1000ms (nvidia-smi)
gpu0: util avg=64.3% max=100.0% vram max=41088/81559MiB temp max=71.0C power max=512.3W
```

…за которым следует одна машиночитаемая строка `gpu: {...}` с полным временным
рядом, попадающая в `perfscale run --summary-export` под ключом `gpu`
(такое же оформление, как у строки гейта `thresholds: {...}`):

```json
{
  "gpu": {
    "source": "nvidia-smi",
    "interval_ms": 1000,
    "devices": [
      {
        "index": 0,
        "samples": [
          { "ts_ms": 1720000000000, "index": 0, "utilization_pct": 64.0,
            "memory_used_mib": 41088.0, "memory_total_mib": 81559.0,
            "temperature_c": 71.0, "power_w": 512.3 }
        ],
        "avg_utilization_pct": 64.3,
        "max_utilization_pct": 100.0,
        "max_memory_used_mib": 41088.0,
        "memory_total_mib": 81559.0,
        "max_temperature_c": 71.0,
        "max_power_w": 512.3
      }
    ]
  }
}
```

Каждый сэмпл несёт `ts_ms` (миллисекунды epoch) на той же шкале времени, что
и строки [stats](metrics.md#live-stats-lines), так что пропускную способность/
задержку и состояние GPU можно строить на одном графике. Отсутствующие
необязательные поля означают, что источник сообщил `N/A` для этой метрики.
Markdown-экспорты (`--summary-export out.md`) получают по одной компактной
строке на устройство на агрегат.

Состояние GPU также стримится **во время прогона**: при включённом
[`report.during_run`](metrics.md#during-run-metrics-reportduring_run) каждый
снимок движка несёт сэмплы GPU, собранные с предыдущего снимка, — они
отправляются коллектору как gauge-метрики `gpu_utilization_pct`,
`gpu_memory_used_mib`, `gpu_memory_total_mib`, `gpu_temperature_c` и
`gpu_power_w` с лейблом `gpu="<index>"`, каждая — с собственной меткой
времени коллектора. Полную таблицу имён и семантику доставки см. в разделе
[метрики во время прогона](metrics.md#during-run-metrics-reportduring_run).

## Пример: Ollama под нагрузкой с наблюдением за GPU

```yaml
# config.yaml
vus: 8
duration: 2m
gpu:
  enabled: true
```

```yaml
# test.yaml
steps:
  - name: llama completion
    use: std/llm@v1
    with:
      url: http://127.0.0.1:11434/v1/chat/completions
      model: llama3.1
      prompt: "Summarize the CAP theorem in two sentences."
      max_tokens: 128
    check:
      status: 200
```

```sh
perfscale run -f test.yaml -c config.yaml --summary-export gpu-run.json
```

Читаем результат целиком: `llm_tokens_per_sec` не растёт, а `gpu0 util avg`
сидит на ~100% → узкое место — GPU (добавьте карту, разделите модель по
шардам или снизьте `vus`); загрузка заметно ниже 100% при растущем TTFT →
смотрите на сервер (очереди, лимиты контекста). VRAM, ползущий к
`memory_total_mib`, объясняет вытеснения/OOM в середине прогона.

## Пример: нагрузка в стиле игрового рендеринга

Нагрузочное тестирование GPU — это не только про LLM-серверы. Другой
классический вопрос — **плотность сессий**: сколько одновременных сессий
рендеринга — игровых клиентов на ноде облачного гейминга, стриминговых
вьюпортов, рендереров цифровых двойников — выдержит одна карта, прежде чем
частота кадров рухнет. Схема та же, что и выше, только «тестируемая система»
здесь — набор процессов-рендереров, которыми perfscale управляет через
[`std/child_process@v1`](actions.md#stdchild_processv1), пока сэмплер `gpu:`
записывает, что делает карта. Сайдкар в `before:` добавляет второй ингредиент
реальной ноды — тяжёлую фоновую задачу на GPU (стадия кодирования),
конкурирующую с сессиями за ту же карту.

Подойдёт любой рендерер, который печатает FPS. В этом примере используется
[glmark2](https://github.com/glmark2/glmark2) (OpenGL, `apt install
glmark2`), зацикливающий свои 3D-сцены в роли заменителя игрового клиента;
`vkmark` — аналог для Vulkan, а headless-сборка Unity/Unreal подключается без
изменений — отличается только `command`.

```yaml
# config.yaml
vus: 1                      # нагрузку создают рендереры; один VU просто
duration: 5m                # держит прогон открытым (см. test.yaml ниже)
allow_process_actions: true # required for child_process/kill_process

gpu:
  enabled: true
  interval_ms: 1000

before:
  # Одна "игровая сессия" = один процесс-рендерер. Ферма запускается как
  # единая управляемая группа процессов, поэтому `after:` останавливает
  # все сессии разом.
  - name: render-farm
    uses: std/child_process@v1
    with:
      command: sh
      args: ["-c", "for i in $(seq 4); do glmark2 --run-forever & done; wait"]
      waitUntil:
        stdout_contains: GL_RENDERER  # GL context is up
        on_timeout: continue
      restart: never                  # упавшая сессия не должна перезапускать вторую ферму

  # Сайдкар: тяжёлая задача на GPU, делящая карту с сессиями, — стадия
  # кодирования в пайплайне игрового стриминга. Зацикленный 1080p60 из
  # сгенерированного источника, кодируется NVENC, вывод отбрасывается в null.
  - name: encode-sidecar
    uses: std/child_process@v1
    with:
      command: ffmpeg
      args: ["-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=60",
             "-c:v", "h264_nvenc", "-f", "null", "-"]
      waitUntil:
        stderr_contains: "Press [q]"  # ffmpeg сообщает о работающем цикле в stderr
        on_timeout: continue
      restart: on-failure             # упавший энкодер перезапускается

after:
  - name: stop the farm
    uses: std/kill_process@v1
    with: { name: render-farm }       # tree: true по умолчанию → все сессии
  - name: stop the sidecar
    uses: std/kill_process@v1
    with: { name: encode-sidecar }
```

```yaml
# test.yaml
steps:
  - name: keep the run open
    use: std/sleep@v1
    with: { seconds: 30 }
```

```sh
perfscale run -f test.yaml -c config.yaml --summary-export render.json
```

Headless-ноды: glmark2 нужен GL-контекст. На GPU-сервере без дисплея
используйте DRM-сборку (`glmark2-drm` / `glmark2-es2-drm` — рендерит через
GBM напрямую на карте) или оберните команду в `xvfb-run -a`.

### Чтение результата

Строки FPS рендерера стримятся в лог прогона с префиксом `render-farm: `;
сводка `gpu:` фиксирует, что тем временем делала карта. Метод — это серия
прогонов, а не один: увеличивайте число сессий (`seq 4` → 1, 2, 4, 8) между
прогонами:

- FPS по сценам делится примерно на N, а `util max` упирается в 100% → GPU
  насыщен; это число сессий — потолок карты для данной нагрузки;
- FPS деградирует, а загрузка остаётся ниже 100% → ограничение в другом месте
  (CPU, переключение контекста) — перепроверьте `temp max` / `power max` на
  предмет троттлинга по температуре или питанию;
- `vram max` при разном числе сессий напрямую отвечает на вопрос ёмкости:
  сколько сессий помещается в `memory_total_mib`, прежде чем драйвер начнёт
  свопить;
- цена сайдкара — это **дельта FPS** между прогоном только с фермой
  (закомментируйте блок `encode-sidecar`) и прогоном ферма+сайдкар при том же
  числе сессий — именно столько ноде облачного гейминга стоит совместное
  использование карты с пайплайном кодирования.

perfscale не разбирает FPS в метрики — числа частоты кадров живут в логе
прогона; именно временной ряд `gpu:` (каждый сэмпл с меткой `ts_ms` на
[шкале времени stats](metrics.md#live-stats-lines)) — то, с чем вы их
сопоставляете на графике.

Одна оговорка для таких прогонов: `utilization.gpu` отражает 3D/compute-движок —
энкодер NVENC, который жгёт сайдкар, там **не учитывается**, поэтому оценивайте
сайдкар по VRAM, потреблению и тому, сколько FPS он отнимает у сессий, а не
по линии загрузки.

## Пример: нагрузка на ANE (Neural Engine) через Core ML

На Apple Silicon вопрос про NPU зеркалит вопрос про GPU: действительно ли
нагрузка задействует Neural Engine и где его потолок? Прилагаемый пример
[`examples/coreml-ane/`](https://github.com/Perfscale/perfscale/tree/main/examples/coreml-ane)
создаёт настоящую нагрузку на ANE сайдкаром на Core ML — небольшая свёрточная
сеть, собранная MIL-билдером [coremltools](https://github.com/apple/coremltools)
(без скачивания модели), зацикленно выполняет предсказания с
`compute_units=ALL`, — пока `gpu.source: powermetrics` записывает показания SoC:

```yaml
# config.yaml (фрагмент)
gpu:
  enabled: true
  source: powermetrics
  interval_ms: 5000

before:
  - name: ane-sidecar
    uses: std/child_process@v1
    with:
      command: sh
      args: ["-c", ".venv/bin/python ane_load.py"]
      waitUntil: { stdout_contains: "inference loop", on_timeout: fail }
      restart: never

after:
  - name: stop the sidecar
    uses: std/kill_process@v1
    with: { name: ane-sidecar }
```

Проверено на M2 Pro: `ane_power_w` поднимается примерно на 2 W над базовой
линией (до ≈7 W в устоявшемся режиме), пока цикл выполняет ≈1 450 инференсов/с
на встроенной сети, а `gpu_utilization_pct` и `gpu_power_w` остаются ровными —
доказательство того, что нагрузка идёт на Neural Engine, а не на GPU.
(powermetrics моделирует несколько ватт потребления ANE даже в простое на
M2 Pro/Max, поэтому читайте дельту, а не абсолютный уровень.) Чтение
результата:

- `ane_power_w` ровно на базовой линии → ANE не задействован: модель упирается
  в CPU/GPU или была загружена без `compute_units=ALL`. (Стек данных Python —
  pandas, NumPy, Anaconda — никогда не трогает ANE; `coremltools`/Core ML —
  единственный путь к нему из пользовательского кода.)
- Масштабируйтесь (больше сайдкаров, `--size 224` или свой `--model
  x.mlpackage`) и смотрите, где инференсы/с перестают расти — это потолок ANE
  чипа для данной нагрузки.

Настройка (venv + правило sudoers для powermetrics) и метод серии прогонов
описаны в README примера.

## Набор GPU-бенчмарков

В репозитории поставляется готовый локальный набор в
[`bench/gpu/`](https://github.com/Perfscale/perfscale/tree/main/bench/gpu):
сценарии `std/llm@v1` против Ollama и vLLM с включёнными метриками `gpu:`,
ступенчатый профиль с нарастающим числом VU (конкурентность против деградации
tok/s / TTFT) и профиль с интенсивностью поступления запросов (найдите
интенсивность, при которой растут TTFT и `dropped_iterations`). Набор только
локальный — у CI-раннеров нет GPU.

```sh
bench/gpu/run.sh ollama        # or: vllm, or both
```

…запускает каждый профиль, записывает JSON `--summary-export` и сырые логи в
`bench/gpu/results/<timestamp>/` и печатает компактную таблицу:

```text
scenario        reqs  req/s  tok/s avg  ttft p50 ms  ttft p95 ms  gpu util max  vram max MiB  dropped
--------------  ----  -----  ---------  -----------  -----------  ------------  ------------  -------
ollama-stages   152   0.42   38.71      212.40       890.15       100%          5104          0
ollama-arrival  210   0.63   31.05      340.72       2410.30      100%          5112          17
```

Настройка (Ollama / vLLM), требования и как читать числа:
`bench/gpu/README.md`.

## Точка расширения

`perfscale-core` экспортирует `gpu::GpuCollector` и
`gpu::register_gpu_collector` — тот же паттерн, что и
[`register_pubsub_driver`](pubsub.md#drivers): сторонняя (проприетарная)
сборка может регистрировать более богатые коллекторы (память по процессам на
базе NVML, причины троттлинга SM clock, `rocm-smi` для AMD, `powermetrics`
для Apple silicon) и выбирать их через `gpu.source` либо перекрывать
встроенные под их собственными именами. Базовые метрики выше — это
OSS-базовый уровень, который сообщает каждый коллектор.
