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
  sandbox never provides real entropy (`wasi:random/random`,
  `wasi:random/insecure` stay unlinked); `wasi:random/insecure-seed` — which
  std's `HashMap` imports on wasip2 — is sunk at a fixed value, so `HashMap`
  works but iterates in a fixed order. Avoid `println!` (imports
  `wasi:cli/*`, never granted).
- **Capabilities**: `fs` (read-only preopen of `fs_root`), `clock`
  (`wasi:clocks`), and `net: [hosts]` (`wasi:http` outgoing-handler, host
  allowlist) are granted in YAML and enforced at load — importing more than
  granted is a hard load error.

### Network access (`net`)

A library that performs outgoing HTTP imports `wasi:http/outgoing-handler`
from its own WIT dependencies — no SDK change is needed. The ergonomic way
from Rust is the [`wstd`](https://crates.io/crates/wstd) client (see
[`examples/netfetch`](examples/netfetch)):

```rust
let mut response = wstd::runtime::block_on(async {
    wstd::http::Client::new()
        .send(wstd::http::Request::get(url).body(wstd::http::Body::empty())?)
        .await
})?;
```

The YAML grant is an allowlist of destination hosts, and the engine checks
every request against it host-side (a request to anything else fails with an
error naming the destination):

```yaml
capabilities:
  - net: ["api.acme.com"]        # any port
  # - net: ["api.acme.com:8443"] # pinned port
  # - net: ["*.internal"]        # any host under the suffix
```

Host-side limits: 30 s per request, 16 MiB per request/response body. Raw
sockets (`wasi:sockets`) are never provided.
- **Secrets**: mark credential-minting functions `secret: true` — the engine
  masks their results in logs.
- **Run settings**: `ctx.settings_json` (or parsed via `ctx.settings()`)
  carries the run's frozen settings — `vus`/`duration_ms` (fixed profile)
  or `stages`/`arrival`, `seed`, and the config `variables:` with
  `${{ env.* }}` resolved. Identical for every call of the run.

## Unit testing without the engine

The trait is plain Rust — no wasm runtime needed:

```rust
let mut lib = FixerIds;
let mut ctx = Ctx::new(42);        // seed
ctx.message_seq = 1;
let a = perfscale_library_sdk::test_call(&mut lib, &mut ctx, "clordid", vec![])?;
```

See [`examples/hello`](examples/hello) for a full, test-covered example.
