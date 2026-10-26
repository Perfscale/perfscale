# SDK для библиотек

SDK для создания **библиотек perfscale** — WASM-компонентов, предоставляющих
генерирующие значения функции для токенов `${alias.fn(...)}` в полезной
нагрузке тестов (RFC 005). Если вы хотите *использовать* библиотеки в тесте,
начните с [руководства по библиотекам](core/libraries.md); эта страница — про
написание собственных.

Один ABI-контракт (`perfscale:library`, модель компонентов WASI Preview 2),
одна форма SDK, три языка:

| Язык | Пакет | Статус |
|---|---|---|
| **Rust** | [`perfscale-library-sdk`](../crates/perfscale-library-sdk) (этот репозиторий) | Стабильный, версионируется вместе с движком |
| **TypeScript / JavaScript** | [`@perfscale/library-sdk`](https://www.npmjs.com/package/@perfscale/library-sdk) ([sdk-libraries](https://github.com/Perfscale/sdk-libraries)) | Стабильный (v0.3.x) |
| **Go** | [Рецепт на TinyGo](https://github.com/Perfscale/sdk-libraries/tree/main/go) | Экспериментальный (пакета SDK пока нет) |

## Что даёт каждый SDK

SDK скрывает всю WIT-обвязку. Вы объявляете имя библиотеки и её функции;
SDK предоставляет:

- **`Ctx`** — контекст одного вызова: `message_seq`, `iteration_seq`,
  `vu_id`, `seed`, `time_ms`, а также зафиксированный для прогона
  `settings_json` в ABI 0.2+ (включая `extensions` конфига библиотек в 0.3).
- **`memo(key, fn)`** — повторное использование сгенерированного значения по
  ключу в пределах одного сообщения: `${fix.id(new)}`, встреченный дважды в
  одном сообщении, даст один и тот же id.
- **PRNG с seed, бит-в-бит идентичный во всех SDK и во встроенном генераторе
  движка** (xorshift64, тот же порядок выборок) — прогон с `seed:`
  воспроизводится независимо от языка, на котором написана библиотека.
  `wasi:random` гостевым компонентам никогда не предоставляется; всю
  случайность получайте из PRNG SDK.
- **Хелперы для аргументов** с понятными авторам ошибками и **тестовый стенд
  без рантайма** — юнит-тестируйте библиотеку на хосте, без wasm-рантайма.

## Rust

Rust SDK находится в этом репозитории в
[`crates/perfscale-library-sdk`](../crates/perfscale-library-sdk) и
публикуется на crates.io. Rust ≥ 1.82 нативно генерирует компоненты WASI
Preview 2 — cargo-component не нужен:

```rust
#[perfscale::library(name = "fixer-ids", version = "1.0.0")]
impl Library for FixerIds {
    fn functions(&self) -> Vec<FunctionInfo> { /* … */ }
    fn init(&mut self, config: &Value, ctx: InitContext) -> Result<(), Error> { /* … */ }
    fn call(&mut self, ctx: &Ctx, f: &str, args: &Value) -> Result<String, Error> {
        ctx.memo("new", || self.next_clordid())
    }
}
```

```console
$ rustup target add wasm32-wasip2
$ cargo build --release --target wasm32-wasip2   # → target/wasm32-wasip2/release/<name>.wasm
```

Репозиторий [library-random](https://github.com/Perfscale/library-random) —
WASM-порт встроенного модуля движка `@std/random` — одновременно служит
эталонной реализацией и шаблоном для авторов.

## TypeScript / JavaScript

```console
$ npm install @perfscale/library-sdk     # requires Node.js ≥ 24
```

```ts
import { args, defineLibrary } from "@perfscale/library-sdk";

export default defineLibrary({
  name: "mylib",
  functions: {
    token: {
      description: "Deterministic per-instance token; memo key reuses it within one message",
      call(argv, ctx) {
        const mint = () => `tok-${ctx.rng().nextU64().toString(16)}`;
        const key = args.optionalString(argv, 0);
        return key === undefined ? mint() : ctx.memo(key, mint);
      },
    },
  },
});
```

Сборка в компонент — через встроенный CLI (ComponentizeJS встраивает
JS-движок — так TS превращается в WASM):

```console
$ npx perfscale-library-build mylib.ts -o mylib.wasm
```

Юнит-тесты без какого-либо WASM-рантайма:

```ts
import assert from "node:assert/strict";
import { Ctx, testCall } from "@perfscale/library-sdk";
import lib from "./mylib.ts";

const ctx = new Ctx(42);
assert.equal(testCall(lib, ctx, "token", []), testCall(lib, ctx, "token", []));
```

Примечание: компоненты jco всегда импортируют `wasi:filesystem/*` и
`wasi:clocks/wall-clock`. TS-библиотека, которая никогда не обращается к
файлам или к системным часам, объявляет `"pure": true` в своём JSON из
`info()` (TS SDK делает это за вас, когда библиотека включает эту опцию), и
тогда ей **не нужны** ни разрешение `capabilities:`, ни
`allow_library_capabilities: true`: движок линкует эти интерфейсы, чтобы
компонент инстанцировался, но не подключает никаких preopen — любой
реальный доступ к файлам завершится ошибкой во время выполнения. (Сам
**ключ** `capabilities:` по-прежнему обязателен в каждой записи
`libraries:` — для библиотеки без разрешений пишите `capabilities: []`;
пропуск ключа — ошибка валидации.) TS-библиотека, которая действительно
читает файлы, по-прежнему объявляет `capabilities: [fs]` (preopen только на
чтение к `fs_root` прогона) плюс `allow_library_capabilities: true`. Сборка
уже вырезает `wasi:random`, `wasi:http` и таймеры.

### Расширения конфига (WIT 0.3)

TS-библиотека может заявить один топ-левел ключ `config.yaml` и его JSON
Schema через `configSchema`. Этот пример экспортирует JSON-контракт
`{ "key", "schema" }` из [справочника YAML](yaml-reference.md), а затем читает
проверенный блок из настроек запуска:

```ts
import { defineLibrary } from "@perfscale/library-sdk";

export default defineLibrary({
  name: "configlib",
  configSchema: {
    key: "configlib",
    schema: {
      type: "object",
      properties: {
        endpoint: { type: "string" },
        pool_size: { type: "integer", minimum: 1 },
      },
      required: ["endpoint"],
      additionalProperties: false,
    },
  },
  functions: {
    endpoint: {
      description: "The configured endpoint",
      call(_argv, ctx) {
        const block = (ctx.settings.extensions ?? {})["configlib"] as
          | { endpoint?: string; pool_size?: number }
          | undefined;
        return block?.endpoint ?? "https://localhost";
      },
    },
  },
});
```

```yaml
libraries:
  - use: ./configlib.wasm
    capabilities: []
configlib:
  endpoint: https://api.test
  pool_size: 16
```

Сам блок расширения необязателен: если он отсутствует, в `extensions` нет
записи `configlib`, и библиотека может применить собственное значение по
умолчанию. Если блок задан, он валидируется на lint и при старте запуска —
в данном примере `endpoint` обязателен, `pool_size` должен быть не меньше 1,
а неизвестные поля отклоняются. Ключ должен соответствовать
`[a-z][a-z0-9_]*` и быть уникальным среди встроенных ключей и загруженных
библиотек. `perfscale-library-build` автоматически выбирает world
`perfscale-library-config`, если задан `configSchema`.

Rust-макрос `export_library!` сейчас экспортирует базовый world
`perfscale-library`. Чтобы объявить схему из Rust, используйте прямые
WIT-bindings для `perfscale-library-config`, как в полном
[примере `configlib`](../crates/perfscale-library-sdk/examples/configlib/src/lib.rs).

## Go

Репозиторий [sdk-libraries](https://github.com/Perfscale/sdk-libraries)
содержит документированный рецепт на TinyGo (таргет `wasip2` +
сгенерированные биндинги) той же формы, что и остальные SDK.
Экспериментальный — пакетного SDK пока нет.

## ABI-контракт

Каждый библиотечный компонент экспортирует `library`; компонент с расширением
конфига дополнительно экспортирует `config-schema` через world
`perfscale-library-config` в ABI 0.3 (`wit/library.wit` в этом репозитории,
копируется в репозиторий каждого SDK):

```wit
// interface library
info: func() -> string;   // JSON: { name, version, functions: […] }
init: func(config-json: string) -> result<_, string>;
call: func(ctx: context, func-name: string, args-json: string)
    -> result<string, string>;

// interface config-schema, дополнительно экспортируется config world в ABI 0.3
config-schema: func() -> string; // JSON: { key, schema }
```

Аргументы и результаты — JSON-строки (JSON есть в любом языке, а назначение
— всегда слот полезной нагрузки). Движок принимает компоненты, собранные под
WIT `perfscale:library@0.1.x`, `@0.2.x` и `@0.3.x`. ABI 0.2 добавил
`settings-json`; 0.3 — config world для объявления расширений. Старые
компоненты продолжают загружаться: в 0.1 нет поля настроек, а для объявления
`config-schema` нужно пересобрать компонент против 0.3. Минорные версии —
строго аддитивные; неподдерживаемая мажорная версия не загружается, и в
ошибке называются обе версии.

### Передача настроек прогона

Манифест `info()` может также содержать `"settings_once": true`. Rust SDK
публикует этот флаг и сохраняет замороженные настройки прогона, когда
последующие вызовы приносят пустую строку настроек. Актуальный движок
передаёт такому компоненту полные настройки при первом завершённом вызове и
пустую строку при последующих, устраняя новую аллокацию и копирование на
хосте при каждом вызове. Компоненты без флага продолжают получать полные
настройки при каждом вызове.

Совместимость двусторонняя: старый движок игнорирует неизвестное поле
манифеста и по-прежнему передаёт полные настройки при каждом вызове, а новый
SDK заменяет сохранённые настройки при получении любой непустой строки.
WIT-контракт и смысл `Ctx.settings()` / `ctx.settings_json` не меняются.

У функций, помеченных `secret: true` в `info()`, каждый результат маскируется
(`***`) в журнале прогона — сгенерированные учётные данные никогда не
попадают в логи. Объявляйте его на самой внешней производящей функции;
никогда не возвращайте производные частичные секреты.

## Запуск и распространение вашей библиотеки

- **Локальная разработка**: объявите `use: ./mylib.wasm` (путь относительно
  объявляющего YAML) и запускайте — больше ничего не нужно.
- **Распространение**: источники по HTTPS или
  `git+<repo>@<ref>#<path>`, скачиваются один раз командой `perfscale install`
  в контентно-адресуемый кэш и фиксируются в `perfscale.lock`; после этого
  `run` и `lint` работают полностью офлайн. Рекомендуется пин `sha256:`
  (проверяется при установке) — без него вычисленный дайджест фиксируется с
  предупреждением.
- **Производительность**: `perfscale install` также *прожигает* каждую
  библиотеку — AOT-прекомпилирует её в нативный артефакт, так что при
  прогонах она десериализуется вместо перекомпиляции на каждый прогон.
  `perfscale burn -f test.yaml -o ./perfscale+libs` идёт дальше и встраивает
  библиотеки в автономную бинарную копию — один файл для доставки на
  генераторы нагрузки.
- **Capabilities**: библиотеки работают в fail-closed песочнице wasmtime —
  см. модель разрешений в [руководстве по библиотекам](core/libraries.md).
- **Docker**: репозиторий sdk-libraries содержит герметичный образ для
  авторов (`ts/docker/Dockerfile` — Node 24 + опубликованный SDK;
  `docker build -t perfscale-library-build ts/docker`, затем
  `docker run --rm -v "$PWD:/src" -w /src perfscale-library-build mylib.ts -o
  mylib.wasm`). Запуск библиотек в Docker-образах движка — схема монтирования,
  кэш установки, прожжённые автономные бинарники — описан в
  [Запуск perfscale в Docker](core/docker.md#wasm-libraries).

## Ссылки

- [Руководство по библиотекам](core/libraries.md) — использование библиотек
  в тестах
- [Справочник YAML](yaml-reference.md#libraries-libraries) — вся поверхность
  конфигурации `libraries:`
- [RFC 005](../rfcs/005-libraries.md) — обоснование дизайна
- [Perfscale/sdk-libraries](https://github.com/Perfscale/sdk-libraries) —
  TS/JS SDK + рецепт на Go ·
  [library-random](https://github.com/Perfscale/library-random) — шаблон для
  авторов ·
  [@perfscale/library-sdk в npm](https://www.npmjs.com/package/@perfscale/library-sdk)
