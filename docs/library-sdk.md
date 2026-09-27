# Library SDKs

SDKs for authoring **perfscale libraries** — WASM components that provide
value-generating functions to `${alias.fn(...)}` tokens in test payloads
(RFC 005). If you want to *use* libraries in a test, start with the
[libraries guide](core/libraries.md); this page is for writing your own.

One ABI contract (`perfscale:library`, WASI Preview 2 component model), one
SDK shape, three languages:

| Language | Package | Status |
|---|---|---|
| **Rust** | [`perfscale-library-sdk`](../crates/perfscale-library-sdk) (this repo) | Stable, versioned with the engine |
| **TypeScript / JavaScript** | [`@perfscale/library-sdk`](https://www.npmjs.com/package/@perfscale/library-sdk) ([sdk-libraries](https://github.com/Perfscale/sdk-libraries)) | Stable (v0.1.x) |
| **Go** | [TinyGo recipe](https://github.com/Perfscale/sdk-libraries/tree/main/go) | Experimental (no SDK package yet) |

## What every SDK gives you

The SDK hides all WIT plumbing. You declare a library name and its
functions; the SDK provides:

- **`Ctx`** — the per-call context: `message_seq`, `iteration_seq`,
  `vu_id`, `seed`, `time_ms` (plus the run's frozen `settings_json` on the
  0.2 ABI).
- **`memo(key, fn)`** — keyed reuse of a generated value within one
  message: `${fix.id(new)}` appearing twice in one message yields one id.
- **A seeded PRNG bit-identical across all SDKs and the engine's built-in
  generator** (xorshift64, same draw order) — a `seed:` run reproduces
  regardless of the language the library is written in. `wasi:random` is
  never provided to guests; draw all randomness from the SDK's PRNG.
- **Argument helpers** with author-friendly errors, and a **runtime-free
  test harness** — unit-test your library on the host, no wasm runtime
  needed.

## Rust

The Rust SDK lives in this repository at
[`crates/perfscale-library-sdk`](../crates/perfscale-library-sdk) and
publishes to crates.io. Rust ≥ 1.82 emits WASI Preview 2 components
natively — no cargo-component needed:

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

The [library-random](https://github.com/Perfscale/library-random) repo —
the WASM port of the engine's `@std/random` built-in — doubles as the
reference implementation and author template.

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

Build to a component with the bundled CLI (ComponentizeJS embeds a JS
engine — that is how TS becomes WASM):

```console
$ npx perfscale-library-build mylib.ts -o mylib.wasm
```

Unit-test without any WASM runtime:

```ts
import assert from "node:assert/strict";
import { Ctx, testCall } from "@perfscale/library-sdk";
import lib from "./mylib.ts";

const ctx = new Ctx(42);
assert.equal(testCall(lib, ctx, "token", []), testCall(lib, ctx, "token", []));
```

Caveat: jco components always import `wasi:filesystem/*` and
`wasi:clocks/wall-clock`, so a TS library needs `capabilities: [fs]` on its
`libraries:` entry (a read-only preopen of the run's `fs_root`) and
`allow_library_capabilities: true` in the config — even when logically
pure. The build already strips `wasi:random`, `wasi:http` and timers.

## Go

The [sdk-libraries](https://github.com/Perfscale/sdk-libraries) repo
carries a documented TinyGo recipe (`wasip2` target + generated bindings),
same shape as the other SDKs. Experimental — no packaged SDK yet.

## The ABI contract

A library component exports one interface
(`wit/library.wit` in this repo, vendored into each SDK repo):

```wit
info: func() -> string;   // JSON: { name, version, functions: […] }
init: func(config-json: string) -> result<_, string>;
call: func(ctx: context, func-name: string, args-json: string)
    -> result<string, string>;
```

Arguments and results are JSON strings (every language has JSON; the
destination is always a payload slot). The engine accepts components built
against WIT `perfscale:library@0.1.x` and `@0.2.x` — 0.2 adds
`settings-json` to the call context; 0.1 components keep working and simply
never see it. Minors are additive-only; an unsupported major fails to load
with both versions named in the error.

Functions marked `secret: true` in `info()` have every result masked
(`***`) in the run log — minted credentials never leak into logs. Declare
it on the outermost producing function; never derive-and-return partial
secrets.

## Running and distributing your library

- **Local iteration**: declare `use: ./mylib.wasm` (path relative to the
  declaring YAML) and run — nothing else needed.
- **Distribution**: HTTPS (`sha256:` pin required) or
  `git+<repo>@<ref>#<path>` sources, fetched once by `perfscale install`
  into the content-addressed cache and pinned in `perfscale.lock`; `run`
  and `lint` then work fully offline.
- **Performance**: `perfscale install` also *burns* each library — AOT
  precompiles it to a native artifact, so runs deserialize instead of
  recompiling per run. `perfscale burn -f test.yaml -o ./perfscale+libs`
  goes further and embeds the libraries into a standalone binary copy —
  one file to ship to load generators.
- **Capabilities**: libraries run in a fail-closed wasmtime sandbox — see
  the [libraries guide](core/libraries.md) for the capability model.
- **Docker**: the sdk-libraries repo carries a hermetic authoring image
  (`ts/docker/Dockerfile` — Node 24 + the published SDK; `docker build -t
  perfscale-library-build ts/docker`, then `docker run --rm -v "$PWD:/src"
  -w /src perfscale-library-build mylib.ts -o mylib.wasm`). Running
  libraries in the engine's Docker images — mount layout, the install
  cache, burned standalone binaries — is covered in
  [Running perfscale in Docker](core/docker.md#wasm-libraries).

## Links

- [Libraries guide](core/libraries.md) — using libraries in tests
- [YAML reference](yaml-reference.md#libraries-libraries) — the full
  `libraries:` config surface
- [RFC 005](../rfcs/005-libraries.md) — design rationale
- [Perfscale/sdk-libraries](https://github.com/Perfscale/sdk-libraries) —
  TS/JS SDK + Go recipe ·
  [library-random](https://github.com/Perfscale/library-random) — author
  template ·
  [@perfscale/library-sdk on npm](https://www.npmjs.com/package/@perfscale/library-sdk)
