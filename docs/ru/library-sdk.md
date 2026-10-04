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
| **TypeScript / JavaScript** | [`@perfscale/library-sdk`](https://www.npmjs.com/package/@perfscale/library-sdk) ([sdk-libraries](https://github.com/Perfscale/sdk-libraries)) | Стабильный (v0.1.x) |
| **Go** | [Рецепт на TinyGo](https://github.com/Perfscale/sdk-libraries/tree/main/go) | Экспериментальный (пакета SDK пока нет) |

## Что даёт каждый SDK

SDK скрывает всю WIT-обвязку. Вы объявляете имя библиотеки и её функции;
SDK предоставляет:

- **`Ctx`** — контекст одного вызова: `message_seq`, `iteration_seq`,
  `vu_id`, `seed`, `time_ms` (плюс зафиксированный для прогона `settings_json`
  в ABI 0.2).
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

## Go

Репозиторий [sdk-libraries](https://github.com/Perfscale/sdk-libraries)
содержит документированный рецепт на TinyGo (таргет `wasip2` +
сгенерированные биндинги) той же формы, что и остальные SDK.
Экспериментальный — пакетного SDK пока нет.

## ABI-контракт

Библиотечный компонент экспортирует один интерфейс
(`wit/library.wit` в этом репозитории, копируется в репозиторий каждого SDK):

```wit
info: func() -> string;   // JSON: { name, version, functions: […] }
init: func(config-json: string) -> result<_, string>;
call: func(ctx: context, func-name: string, args-json: string)
    -> result<string, string>;
```

Аргументы и результаты — JSON-строки (JSON есть в любом языке, а назначение
— всегда слот полезной нагрузки). Движок принимает компоненты, собранные под
WIT `perfscale:library@0.1.x` и `@0.2.x` — 0.2 добавляет `settings-json` в
контекст вызова; компоненты 0.1 продолжают работать и просто никогда его не
видят. Минорные версии — строго аддитивные; неподдерживаемая мажорная версия
не загружается, и в ошибке называются обе версии.

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
