# Value generators (`${alias.fn(...)}`)

A library function returns a string spliced into any payload — HTTP bodies,
URLs, headers, WebSocket/gRPC messages, GraphQL variables, TCP/UDP frames,
LLM prompts, DB parameters — wherever `${…}` tokens expand (once per message
sent).

```ts
import { args, defineLibrary } from "@perfscale/library-sdk";

export default defineLibrary({
  name: "mylib",
  functions: {
    token: {
      description: "Deterministic per-message token; a memo key reuses it within one message",
      call(argv, ctx) {
        const mint = () => `tok-${ctx.rng().nextU64().toString(16)}`;
        const key = args.optionalString(argv, 0);
        return key === undefined ? mint() : ctx.memo(key, mint);
      },
    },
  },
});
```

```console
$ npx perfscale-library-build mylib.ts -o mylib.wasm      # --kind library is the default
```

```yaml
libraries:
  - use: ./mylib.wasm
    capabilities: []
steps:
  - use: std/http@v1
    with:
      method: POST
      url: https://api.example.com/orders
      headers: { x-idempotency: "${mylib.token(order)}" }
      body: { id: "${mylib.token(order)}" }     # same value — memo key `order`
```

`Ctx` gives `messageSeq`, `iterationSeq`, `vuId`, `seed`, `timeMs`,
`settings`, `ctx.rng()` (seeded PRNG, bit-identical across SDKs and the
engine) and `ctx.memo(key, fn)` (per-message reuse). `args.*` helpers parse
arguments; throw `LibraryError` to fail the step with a message.

Built-ins already cover common cases — check before writing a library:
`${uuid}`, `${seq}`, `${vu}`, `${rand(a,b)}`, `${randf(a,b,dp)}`,
`${choice(a|b)}`, `${now}`, `${now_ms}`, `${now_iso}`; `@std/random@v1`
(`uuid4`, `uuid7`, `ulid`, `int`, `pattern`, `email`, …); `@std/json@v1`
(`read`, `pick` from JSON files).

## Policy and masking

```yaml
libraries:
  - use: ./mylib.wasm
    capabilities: []
    allow: [token]         # whitelist: other calls fail the step
    log: [token]           # always mask these results in logs
```

## Config extensions

A library can own one top-level `config.yaml` key validated against a JSON
Schema (WIT 0.3), and read it from `ctx.settings.extensions`:

```ts
export default defineLibrary({
  name: "configlib",
  configSchema: {
    key: "configlib",
    schema: {
      type: "object",
      properties: { endpoint: { type: "string" }, pool_size: { type: "integer", minimum: 1 } },
      required: ["endpoint"],
      additionalProperties: false,
    },
  },
  functions: {
    endpoint: {
      call(_argv, ctx) {
        const block = (ctx.settings.extensions ?? {})["configlib"] as { endpoint?: string } | undefined;
        return block?.endpoint ?? "https://localhost";
      },
    },
  },
});
```

```yaml
configlib:
  endpoint: https://api.test
  pool_size: 16
```

Validated at `perfscale lint` and run start; the key is `[a-z][a-z0-9_]*`
and unique across built-in keys and loaded libraries. The build picks the
`perfscale-library-config` world automatically when `configSchema` is set.

## Rust

```rust
use perfscale_library_sdk::{args, export_library, Ctx, Error, FunctionInfo, Library};
use serde_json::Value;

#[derive(Default)]
struct MyLib;

impl Library for MyLib {
    fn functions(&self) -> Vec<FunctionInfo> {
        vec![FunctionInfo { name: "token", description: "a token", secret: false }]
    }
    fn pure(&self) -> bool { true }
    fn init(&mut self, _config: Value) -> Result<(), Error> { Ok(()) }
    fn call(&mut self, ctx: &mut Ctx, func: &str, a: Vec<Value>) -> Result<String, Error> {
        match func {
            "token" => Ok(format!("tok-{:x}", ctx.prng().next_u64())),
            other => Err(Error::new(format!("unknown function {other}"))),
        }
    }
}

export_library!(MyLib);
```

`cargo build --release --target wasm32-wasip2`. Reference: `Perfscale/faker`,
`Perfscale/library-random`. Go: TinyGo + `wit-bindgen-go` recipe in the
`sdk-libraries` repo (`go/README.md`, experimental).
