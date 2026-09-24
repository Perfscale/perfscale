# perfscale-library-sdk

Rust SDK for authoring **perfscale libraries** — WASM components that provide
value-generating functions to `${alias.fn(...)}` tokens in perfscale test
payloads (RFC 005). The engine runs your component in a wasmtime sandbox
under a fail-closed capability model.

## Write a library in five minutes

```console
$ cargo init --lib fixer-ids
$ cd fixer-ids
```

`Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
perfscale-library-sdk = { path = "/path/to/perfscale/crates/perfscale-library-sdk" }
serde_json = "1"
```

`src/lib.rs`:

```rust
use perfscale_library_sdk::{args, export_library, Ctx, Error, FunctionInfo, Library};

#[derive(Default)]
struct FixerIds;

impl Library for FixerIds {
    fn functions(&self) -> Vec<FunctionInfo> {
        vec![FunctionInfo {
            name: "clordid",
            description: "FIX ClOrdID; memo key reuses it within one message",
            secret: false,
        }]
    }

    fn init(&mut self, config: serde_json::Value) -> Result<(), Error> {
        // The YAML `with:` block arrives here as JSON. Default: ignore it.
        if let Some(v) = config.get("comp_id_seed") { /* … */ let _ = v; }
        Ok(())
    }

    fn call(&mut self, ctx: &mut Ctx, func: &str, args: Vec<serde_json::Value>)
        -> Result<String, Error>
    {
        match func {
            "clordid" => {
                let mint = |c: &mut Ctx| Ok(format!("ORD-{:08}", c.prng().below(100_000_000)));
                // `${fix.clordid(new)}` twice in one message → one id;
                // `${fix.clordid()}` is always fresh.
                match args::optional_string(&args, 0) {
                    Some(key) => ctx.memo(&key, mint),
                    None => mint(ctx),
                }
            }
            other => Err(Error::new(format!("fix.{other}: unknown function"))),
        }
    }
}

export_library!(FixerIds);
```

Build (Rust ≥ 1.82 emits WASI Preview 2 components natively — no
cargo-component needed):

```console
$ rustup target add wasm32-wasip2
$ cargo build --release --target wasm32-wasip2
```

Reference the artifact from your perfscale config, relative to the
declaring file:

```yaml
libraries:
  - use: ./target/wasm32-wasip2/release/fixer_ids.wasm
    as: fix
    capabilities: [clock]        # only if the component imports wasi:clocks
    with:
      comp_id_seed: TRADER
```

Then `${fix.clordid(new)}` in any `${...}`-expanding payload.

## The rules that matter

- **State**: one instance per (library × VU/connection owner). `init` runs
  once per instance; `init` failure is fatal to the run.
- **memo**: `ctx.memo(key, f)` caches by `(message_seq, key)` and resets when
  the message moves on. There is no step/iteration/run-scoped state —
  cross-step reuse is `outputs:` + `${{ }}`.
- **Determinism**: draw *all* randomness from `ctx.prng()` (xorshift64 seeded
  per instance — the same algorithm as the engine's own generator). The
  sandbox never provides `wasi:random`; avoid `HashMap`/`RandomState` (use
  `Vec`/`BTreeMap`) and `println!` (imports `wasi:cli/*`, never granted).
- **Capabilities**: `fs` (read-only preopen of `fs_root`), `clock`
  (`wasi:clocks`) are granted in YAML and enforced at load — importing more
  than granted is a hard load error. `net` is not yet supported.
- **Secrets**: mark credential-minting functions `secret: true` — the engine
  masks their results in logs.

## Unit testing without the engine

The trait is plain Rust — no wasm runtime needed:

```rust
let mut lib = FixerIds;
let mut ctx = Ctx::new(42);        // seed
ctx.message_seq = 1;
let a = perfscale_library_sdk::test_call(&mut lib, &mut ctx, "clordid", vec![])?;
```

See [`examples/hello`](examples/hello) for a full, test-covered example.
