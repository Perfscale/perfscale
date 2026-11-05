# Distributing and running components

## Declaring

```yaml
libraries:
  - use: ./libs/mylib.wasm              # path — relative to the declaring file
    capabilities: []                    # required; grants: fs, clock, process
    as: mylib                           # alias → ${mylib.fn()} / lib/mylib/<step>
  - use: '@std/random@v1'               # built-in, no file
    capabilities: []
  - use: 'https://vendor.example.com/ids.wasm'   # remote: fetched by `perfscale install`
    sha256: '9f2c…'                              # pinned digest
    capabilities: []
  - use: 'git+https://github.com/org/repo.git@v1.2.3#libs/ids.wasm'
    capabilities: []
```

Capabilities are fail-closed: the component's WIT imports are inspected at
load, and anything it imports without a grant refuses to load.

| Grant | Gives |
|---|---|
| `clock` | wall clock |
| `fs` | file access |
| `{ net: [hosts] }` | outbound connections to the listed hosts (value functions) |
| `process` | `ctx.exec()` host spawns (steps only; also needs `allow_process_actions: true`) |

Any non-empty grant requires `allow_library_capabilities: true` in the
config. Custom **steps** never get network egress (RFC 009 v1). A library
whose `info()` reports `"pure": true` needs no grant for the
`wasi:filesystem` / `wasi:clocks` imports the TS toolchain always adds —
and gets no preopens, so real file access fails.

## install / lock / burn

```console
$ perfscale install            # fetch remote components, verify, pin digests in perfscale.lock,
                               # and pre-compile (burn) libraries + WASM reporters into the cache
$ perfscale lint test.yaml config.yaml   # loads every component, checks lib/ refs, config blocks
$ perfscale cache clean        # drop cached imports + compiled library artifacts
```

Commit `perfscale.lock`: runs on CI and on machines resolve the same digests.
Without `install`, the first run compiles each component (slow, once);
after it, runs deserialize native code.

## Choosing the language

| SDK | Status | Build |
|---|---|---|
| TypeScript `@perfscale/library-sdk` | functions, steps, process steps, config extensions, reporters | `npx perfscale-library-build x.ts -o x.wasm --kind library\|steps\|reporter [--process]` |
| Rust `perfscale-library-sdk` | functions via `export_library!`; steps/config via direct WIT bindings (examples in the repo) | `cargo build --release --target wasm32-wasip2` |
| Go | experimental recipe (TinyGo + `wit-bindgen-go`) | see `sdk-libraries/go/README.md` |

Rust components are smaller and faster per call; TypeScript embeds a JS
engine (larger `.wasm`, more per-call cost) but needs no Rust toolchain.
