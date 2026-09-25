# RFC 005: Libraries — WASM value generators

- **Status**: Draft
- **Author**: Perfscale Team
- **Created**: 2026-09-24
- **Requires**: none (informs RFC 002 execution model; WIT/SDK work shares infra with RFC 001)
- **Required by**: none

## Summary

Introduce **libraries**: user-loadable WASM modules that provide functions for
generating values inside `${...}` generator tokens — UUIDs, ids, faker-style
data, and any domain-specific "magic" variables a protocol needs (FIX ClOrdID
patterns, SOAP message ids, signed tokens). Libraries are declared in YAML
(`libraries: - use: "@std/random@v1"`), addressed in payloads through an alias
(`${random.ulid()}`), and run in a wasmtime sandbox under an explicit,
fail-closed capability model. This RFC covers the config surface, the
token-resolution semantics, the WASM ABI (WASI Preview 2 + WIT), the SDK for
library authors (Rust, TS/JS, Go), distribution, and the agent story. It is
deliberately scoped to **value generation**; WASM *step/actions* are a
separate future RFC that will reuse this runtime.

## Motivation

- The `${...}` generator layer (`crates/perfscale-core/src/generate.rs`) is
  hardcoded to seven tokens (`seq`, `uuid`, `now`, `now_ms`, `now_iso`,
  `rand`, `randf`, `choice`). `${uuid}` is not even a real UUID — it is 32
  hex chars from two xorshift draws. Every richer need (uuid v7, ULID,
  faker data, protocol-specific ids) is impossible without patching the
  engine.
- FIX, SOAP, and similar protocols live on "magic" variables that must be
  generated *and reused* across fields of one message (ClOrdID and its echo).
  The engine has no mechanism for keyed reuse beyond `${seq}`.
- The proprietary `pro/fix` actions already share the `${...}` expander —
  the doc comment in `generate.rs` promises "${…} means exactly one thing
  across all protocols", but `std/http` does not even expand tokens today.
  The promise and the reality have drifted apart.
- RFC 002 names WASM components as the credible execution model for
  third-party code but defers the investment "until a concrete code-action
  need justifies it". Value generators are that concrete need — smaller,
  safer, and useful on their own.

## Goals

- A `libraries:` config surface where users declare WASM (or built-in)
  libraries and call their functions from `${...}` tokens in any action's
  payload.
- A stable, versioned WIT interface (`perfscale:library`) with SDKs for
  Rust, TS/JS, and Go — the three languages that compile to WASI p2
  components today.
- A fail-closed capability model: fs/net/clock access is granted explicitly
  per library in YAML, and network egress is restricted to allowlisted hosts
  via a host-provided HTTP interface (no raw sockets).
- Keyed reuse of generated values within one message (`memo(key)` semantics),
  optional run-level determinism (`seed:`), and secret masking for generated
  credentials.
- Distribution via built-ins, local paths, HTTPS (sha256-pinned), and git
  refs — fetched only by an explicit `perfscale install` into a lockfile.
- Full support on the perfscaled agent from day one, under a fleet policy
  that intersects with task-granted capabilities.

## Non-goals (this RFC)

- **WASM step/actions.** A component that *executes a step* (does its own
  protocol I/O as an action) is a different host contract (metrics integrity,
  connection pooling — pitfalls 2 and 7 of RFC 002) and gets its own RFC.
  Libraries only produce strings.
- A marketplace/registry service. Distribution channels here are
  filesystem/git/HTTPS; a browsable registry remains RFC 002 territory.
- Changing the existing hardcoded tokens (`${seq}`, `${rand}`, …). They
  stay exactly as they are — see Tradeoffs.
- Monetization, paid libraries, web catalog.

## Detailed design

### Configuration

`libraries:` is accepted in both the config file and the test file (merged
across `import:` like other composition; an alias collision is a hard
validation error):

```yaml
libraries:
  - use: "@std/random@v1"                  # built-in (quotes required: `@` is a reserved YAML indicator)
  - use: ./libs/fixer-ids.wasm             # local path (relative to the declaring file)
    as: fix                                # alias → token prefix (default: library name)
    capabilities: [clock]                  # explicit grant, fail-closed
    with:                                  # JSON config passed to init(); may use ${{ env.X }}
      comp_id_seed: "TRADER"
  - use: https://artifacts.acme.com/faker-1.2.0.wasm
    sha256: "9f2c…"                        # mandatory for HTTPS
  - use: git+https://github.com/acme/perfscale-isin@v1.2.0
    capabilities:
      - fs
      - net: ["api.acme.com", "*.internal"]  # host allowlist for egress
```

Library references use `@ns/name@vN` — the leading `@` distinguishes a
library reference from an action ID (`std/http@v1`) at resolution time.

The global gate follows the existing `allow_file_actions` /
`allow_process_actions` pattern: without `allow_library_capabilities: true`
in the config, any non-empty `capabilities:` grant is a validation error.

### Token resolution

Libraries register into the generator behind an alias:

```
${random.ulid()}        ${fix.clordid(new)}        ${faker.email()}
```

- `Gen::eval` (`generate.rs`) gains an alias resolver: `alias → provider`.
  Built-in tokens keep matching first and keep their exact current behavior.
- Unknown alias → token left verbatim (current behavior, backward
  compatible; `perfscale lint` flags it). Known alias + unknown function →
  **step failure** (the alias is a declared contract; a typo must not ship
  silently into a payload).
- A failed call — guest error, trap, fuel exhaustion, wall-time timeout —
  fails the step with the cause recorded; severity/metrics handle it like
  any other action error. A load test must never send wrong data and report
  green.

### Expansion coverage

`${...}` expansion becomes uniform across every action that carries string
payloads: `std/http` (body, headers, URL), raw TCP/UDP, LLM prompts, DB
parameters — joining ws/gRPC/GraphQL where it already exists. Unknown tokens
stay verbatim everywhere, so existing configs containing literal `${...}`
text are unaffected. One rule, one code path, documentation matching
reality.

### Call context and reuse semantics

Every call receives a context record:

```wit
record context {
    message-seq: u64,     // same counter as ${seq}; bumped per message send
    iteration-seq: u64,   // VU loop iteration
    vu-id: u64,           // for per-VU partitioning (id ranges, comp ids)
    seed: u64,            // per-instance deterministic seed (see below)
    time-ms: u64,         // wall clock; libraries never import wasi:clocks for this
}
```

- **Reuse within a message** is an SDK primitive, not host magic:
  `memo(key, f)` returns the same value for repeated calls with the same
  `key` within one `message-seq`. `${fix.id(new)}` appearing twice in one
  FIX message yields one ClOrdID; `${random.uuid4()}` is always fresh.
  The host stays dumb; semantics belong to the library author.
- **Cross-step reuse** already exists — `outputs:` + `${{ resp.id }}`
  interpolation. Libraries deliberately do not duplicate it. There is no
  step/iteration/run-scoped memo state hidden inside WASM instances.
- `vu-id`/`iteration-seq` in the context let libraries partition data
  across VUs (non-overlapping id ranges are a constant load-testing need).

### Determinism

An optional `seed:` in the config makes a run reproducible: per-instance
seeds derive as `hash(seed, vu_id, conn_seq)`. SDKs provide a seeded PRNG
and libraries must draw entropy from it; `wasi:random` is **not** provided
to guest modules at all. Without `seed:`, behavior is as today (random).
Wall-clock time stays non-deterministic and is documented as such — virtual
clocks would break realism (TTLs, timeouts, server-side time).

### WASM ABI

wasmtime + WASI Preview 2 + the component model — the only target where all
three SDK languages compile natively (Rust via `cargo-component`, TS/JS via
`jco componentize`, Go via TinyGo `wasip2`). Draft interface:

```wit
package perfscale:library@0.1.0;

interface library {
    record context { /* above */ }

    record function-info {
        name: string,
        description: string,
        secret: bool,        // results are registered into the SecretRegistry
    }

    info: func() -> string;  // JSON: { name, version, functions: [function-info] }
    init: func(config-json: string) -> result<_, string>;
    call: func(ctx: context, func-name: string, args-json: string)
        -> result<string, string>;
}

world perfscale-library {
    export library;
    // WASI imports are connected by the host only per granted capabilities
}
```

Design points:

- **Arguments and results are JSON strings.** Every language has JSON;
  functions quickly outgrow `list<string>`. Results are strings because the
  destination is always a payload slot.
- **Single synchronous call model.** The guest sees a blocking `call()`;
  the host executes instances on tokio and implements `wasi:http` async
  under the hood. Per-call fuel and a wall-time timeout (default ~5 ms,
  configurable per library) bound the damage a runaway module can do on the
  hot path. Time spent inside `call()` — including host-mediated I/O — is
  counted in the enclosing step's duration: library cost is visible in
  metrics, never hidden.
- **Versioning.** The engine supports a range of WIT interface majors
  (0.1.x now, 1.x later); a module built against an unsupported major fails
  to load with both versions named in the error. Minors are additive-only
  (new context fields are `option<>`). A library's own `@vN` is independent
  semver resolved through the lockfile.
- **Instance lifecycle.** One instance per (library × generator owner),
  parked alongside `Gen` in `step/resources.rs` (per VU, per live
  connection). `init` runs once per instance with its derived seed.

### Capabilities and sandboxing

The guest's WIT imports declare what it needs; the YAML `capabilities:`
list grants what it may use. The host connects exactly the intersection;
a module importing more than granted is a hard load error, a grant wider
than the imports is a lint warning.

| Capability | Host provides | Boundaries |
|---|---|---|
| `fs` | `wasi:filesystem` preopens | only paths under `fs_root`, read-only by default |
| `net: [hosts]` | `wasi:http` outgoing-handler | destination host/port checked against the allowlist; **raw `wasi:sockets` is never provided in v1** |
| `clock` | `wasi:clocks` monotonic | wall time already arrives via `context.time-ms` |

Rationale for `wasi:http`-only egress: an allowlist is only enforceable if
the host mediates the connection. Giving a third-party module raw sockets
in a tool whose entire job is "send many requests fast" is the
weaponization vector named as pitfall 1 in RFC 002. Narrowing a binary
`net` flag later would be a breaking change; starting narrow is not.

### Secrets

Two surfaces, two existing mechanisms:

1. **Library config secrets** (API keys in `with:`) — authored as
   `${{ env.X }}`; the existing interpolation + `SecretRegistry` masking
   covers them. No new mechanism.
2. **Generated secrets** — a function marked `secret: true` in `info()`
   has every result registered into the `SecretRegistry`, so a minted
   session token is masked in logs exactly like an env secret. Declared by
   the library author, enforced by the engine. (Masking *all* results is a
   non-starter: registering short strings like `"7"` would mask half the
   log output.)

### Distribution and fetching

**Implemented in v0.22.0** (`perfscale install`, `perfscale.lock`, HTTPS/git
sources, offline cache).

Resolution order: built-ins (`@std/*`) → local paths → cache. Network
sources (HTTPS, git) are fetched **only** by `perfscale install`, which
verifies integrity and writes `perfscale.lock`:

```yaml
libraries:
  - use: 'https://vendor.example.com/fixer-ids.wasm'
    sha256: '…64 hex…'   # required for HTTPS
  - use: 'git+https://github.com/org/repo.git@v1.2.3#libs/fixer-ids.wasm'
```

- HTTPS entries pin the declared `sha256`; a re-published artifact with a
  different digest is a hard error, never a silent swap.
- git refs resolve to a commit sha recorded in the lock along with the
  artifact digest (tags are mutable; commits are not). The `git+` syntax is
  `git+<repo-url>@<ref>#<path>`: the ref split is at the *last* `@` (a
  userinfo `@` in the URL survives), and the `#path` artifact name is
  mandatory. `perfscale install --refresh` re-resolves the ref.
- `perfscale.lock` is TOML, keyed by the exact `use:` string, and lives
  next to the declaring document (repository root for git-imported
  documents). Artifacts are content-addressed under
  `<cache>/libraries/<sha256>.wasm` (same cache root as `import:` clones,
  `PERFSCALE_CACHE_DIR` respected).
- Run and lint are fully offline against the cache: load-time resolution
  rewrites each remote ref to its cached artifact path before validation,
  and a missing lock/entry/artifact is a hard error pointing at
  `perfscale install`. This is the RFC 002 discipline ("explicit install;
  run time is offline") adopted from day one, so the future marketplace
  registry becomes a pure addition, not a migration.

### The `@std/random@v1` built-in

Ships natively (no wasmtime needed) and dogfoods the exact provider trait
external WASM libraries will implement:

- **Core id-gen**: `uuid4`, `uuid7`, `ulid`, `nanoid(len)`, `int(a,b)`,
  `float(a,b,dp)`, `pick(a|b|c)`, `weighted(a:10|b:90)`, `seq(key)` (named
  counters), `pattern("ORD-####-????")`.
- **Faker base**: names, emails, usernames, companies, lorem — small
  deterministic corpora embedded in the binary.
- **Dates**: `date(a,b)`, `timestamp(a,b)`, formatted time — complementing
  the existing hardcoded `now`/`now_ms`/`now_iso`.

Finance/FIX-specific generators (ISIN, Luhn, ClOrdID patterns) are
intentionally left to third-party libraries — that is the ecosystem case
this RFC exists to enable.

The hardcoded tokens (`${seq}`, `${rand}`, …) are **not** moved into a
library: they are zero-cost, universally available without declaration, and
every existing config depends on them.

### Agent (perfscaled) support

Libraries work on the agent from day one:

- The agent gains a managed install path (artifact → verify → cache keyed
  by sha256); task configs reference cached digests, never fetch at run
  time.
- Effective capabilities = **fleet policy ∩ task grant**. The agent's own
  config sets the maximum allowed capabilities (and optionally an allowlist
  of library digests); a task asking for more than fleet policy is a
  validation error. A tenant that can create controlplane tasks must not be
  able to grant itself `net` on a shared fleet machine.
- Cache poisoning/yank propagation follow the RFC 002 stance: warn loudly,
  let CI break, never silently swap.

### SDKs for authors

One shape in three languages, hiding all WIT plumbing:

```rust
// Rust — perfscale-library-sdk
#[perfscale::library(name = "fixer-ids", version = "1.0.0")]
impl Library for FixerIds {
    fn functions(&self) -> Vec<FunctionInfo> { /* … */ }
    fn init(&mut self, config: &Value, ctx: InitContext) -> Result<(), Error> { /* … */ }
    fn call(&mut self, ctx: &Ctx, f: &str, args: &Value) -> Result<String, Error> {
        ctx.memo("new", || self.next_clordid())
    }
}
```

```ts
// TS/JS — @perfscale/library-sdk, built with jco componentize
export default defineLibrary({
  functions: { ulid: { secret: false } },
  call: (ctx, fn, args) => ulid(ctx.rng),
});
```

Go: TinyGo `wasip2` + generated bindings, same shape. All three SDKs
provide: seeded PRNG, `memo()`, argument parsing, error mapping, and a
local test harness that feeds recorded contexts so a library can be unit
tested without the engine.

### Repository layout, docs, and Docker

Decided after phase 1 (2026-09-24):

- **Rust SDK** lives in the perfscale workspace as
  `crates/perfscale-library-sdk`, versioned with the engine; the WIT files
  live beside it as the single source of truth. Publishes to crates.io.
- **TS/JS and Go SDKs** live in a new public repo
  `Perfscale/sdk-libraries` (`ts/`, `go/`). (The existing `Perfscale/sdk-js`
  is private and is the *test-authoring* SDK of RFC 001 — a different
  product surface; do not mix them.)
- **`Perfscale/library-random`** — new public repo with the WASM source of
  `@std/random`, doubling as the reference implementation and author
  template. The engine keeps the native implementation for benchmarks and
  embeds the compiled component from this repo at build time
  (`include_bytes!` of the released artifact), because `@std/*` must
  resolve offline without `perfscale install`.
- **Docs**: OSS docs are `perfscale/docs/`, which the site serves at
  `/docs/oss` via a git submodule — the `libraries:` reference shipped in
  phase 1 and publishes on the next submodule bump. Platform docs
  (`Perfscale/docs`, en/ru) get their libraries page with phase 4 (agent
  support), when the feature stops being CLI-only.
- **Docker**: running needs only mounted `.wasm` files (paths resolve
  relative to the declaring config) plus network access for `net`-granted
  libraries. Authoring gets a Dockerfile/devcontainer per SDK repo so the
  cargo-component/jco/TinyGo toolchains are reproducible. The agent
  (phase 4) keeps its library cache on a named volume so installs survive
  container restarts.

### Lint, schema, metrics

- `libraries:` fields derive from serde structs with `schemars`, so JSON
  Schema, validation, and IDE autocomplete follow automatically.
- `perfscale lint` loads declared libraries (offline, from cache) and
  validates `${alias.fn(...)}` tokens against exported function lists —
  typos are caught before the run.
- Summary gains per-library counters: calls, errors (trap/timeout split
  out), p95 call duration — atomics + existing HDR histograms, no per-call
  logging. A library that becomes the bottleneck must be visible as one.

## Benefits

- Closes the "I need a real uuid / ULID / faker value / FIX id" gap without
  engine patches — and without growing `std/` for niche needs.
- Turns the latent `@v1` namespace and the `${...}` generator layer into a
  real extension point with one consistent semantics across all protocols.
- Establishes the WASM runtime, capability model, install/lockfile flow,
  and author SDKs that a future WASM-actions RFC (RFC 002's option 2) will
  build on — the risky infrastructure is paid for once, on the smallest
  useful surface.
- Deterministic seeded runs make load-test failures reproducible — a
  capability the engine simply does not have today.
- Fail-closed capabilities + host-mediated egress make third-party code
  viable on a shared agent fleet instead of permanently CLI-only.

## Drawbacks

- **wasmtime is heavy**: minutes of build time and a meaningfully larger
  binary, for every user including those who never load a library. (A
  cargo feature gate is possible but forks the test matrix; see Open
  questions.)
- **Sandboxing is a permanent security commitment** — inherited from
  RFC 002, now activated. Capability enforcement, WASI upgrades, and
  wasmtime CVEs become ongoing maintenance, not a finished feature.
- **The config stops being fully self-contained**: lockfile, cache,
  install step. The "one YAML and go" ergonomics degrade for
  library-using tests (mitigated: `@std/*` needs none of it).
- **Three SDKs to keep in sync** with the WIT interface; the compat matrix
  (SDK × interface × engine) needs CI budget — the same warning RFC 001
  gives itself.
- Hot-path footgun remains real: nothing *prevents* a library author from
  doing a network call per message. Limits and metrics expose it, but the
  engine cannot make a slow library fast.

## Tradeoffs

- **Full capability access in v1 vs pure-generators-first**: pure functions
  would make the sandbox trivial, but real generators need corpora (fs)
  and token services (net); shipping capabilities later would mean
  retrofitting the security model onto an installed base. Chosen: full
  access with the fail-closed model now — the model is cheaper to build
  than to retrofit.
- **WASI p2 component model vs WASI p1 + custom ABI**: p1 is more mature
  and big-Go-friendly, but has no typed contract and leaves TS/JS to an
  embedded interpreter hack. p2 is the standard all three SDK languages
  compile to natively. Chosen: p2, accepting TinyGo-only for Go.
- **Host-mediated `wasi:http` vs raw sockets**: only the former is
  allowlist-enforceable; the latter is strictly more powerful. Chosen:
  safety over power; revisit raw sockets only with a concrete need.
- **Sync call model vs pure/io library classes**: splitting classes would
  protect metrics by construction but complicates the WIT surface
  (init-time loading hooks, caching) and kills legitimate per-call I/O
  cases. Chosen: one model + limits + honest metrics attribution.
- **SDK `memo(key)` vs host-side memoization by token text**: host
  memoization is transparent but makes two fresh uuids in one message
  impossible without ugly workarounds. Chosen: context + SDK helper;
  semantics belong to the author, the host stays dumb.
- **Built-ins stay hardcoded vs everything-becomes-a-library**: moving
  `${seq}` into an auto-loaded `@std/core` is conceptually cleaner but
  adds a layer where hardcode is honestly faster and risks breaking every
  existing config. Chosen: built-ins untouched; `@std/random` is additive.
- **Fetch policy**: install+lockfile vs fetch-on-run. Chosen: explicit
  install, offline runs — the RFC 002 discipline, before habits form.

## Non-obvious pitfalls

1. **Fuel is not a fairness guarantee on the hot path.** A library that
   burns its 5 ms timeout on 1% of calls is invisible in averages and
   poisons tail latency. The p95-per-library metric exists precisely for
   this; the timeout default may need tuning per deployment, hence
   configurable per library, not global.
2. **JSON-in-JSON argument escaping.** Token args arrive as text
   (`${random.int(1,100)}`) but the ABI speaks JSON. The resolver must
   define exactly how `1,100` becomes `[1,100]` (positional → JSON array),
   how strings with commas/quotes survive, and this mapping is part of the
   *interface* contract — get it wrong and every SDK parses differently.
3. **Component-model toolchain drift.** `cargo-component`, `jco`, and
   TinyGo's wasip2 support all move fast and occasionally disagree about
   WIT subtleties (e.g., `option<>` lowering). The engine's WIT must be
   pinned, vendored, and tested against all three toolchains in CI from
   the first SDK — interface bugs found after external libraries exist are
   unfixable without breakage.
4. **Secrets can leak through memoization and logs in combination.**
   `memo()` caches a `secret: true` result inside the guest instance; the
   host masks the registered string, but if the library *transforms* the
   secret (token + suffix) the transformed value is a new unregistered
   string. Documentation must state: mark the outermost producing function
   secret, never derive-and-return partial secrets.
5. **wasmtime instance memory is per-instance and adds up.** One instance
   per (library × VU) at 1000 VUs with a faker corpus loaded per instance
   is real memory. The SDK should lazy-load corpora; the engine should
   document instance overhead and consider shared read-only precompiled
   modules (wasmtime `Module` sharing is safe; `Store`s are not).
6. **The agent cache is a supply-chain artifact.** Full agent support from
   day one means cache keying, digest allowlists, and fleet policy are
   v1 scope, not later hardening. Skimping here repeats pitfall 5 of
   RFC 002 with real tenants.
7. **Uniform `${...}` expansion changes `std/http` behavior.** Literal
   `${...}` text in existing HTTP payloads that happens to match a loaded
   library's function will now expand. Unknown-token-verbatim keeps this
   rare, but the changelog must call it out loudly, and lint gains a
   "resolved token" report so users can see what *would* expand before
   upgrading.
8. **`sha256:` in YAML for HTTPS sources duplicates the lockfile.** The
   inline pin is the authoring-time intent; the lockfile is the resolved
   truth. When they disagree the lockfile wins for cache lookup but the
   mismatch is a hard error — never silently "update" the YAML pin.

## Alternatives considered

- **Grow the hardcoded token set instead.** Cheap per token, but every
  need becomes an engine PR, and proprietary/domain-specific generators
  (FIX desks, internal id schemes) can never be served. Rejected — it is
  the status quo that motivates this RFC.
- **Embedded scripting (Lua/JS via rquickjs/rhai) instead of WASM.**
  Simpler host integration and no toolchain for authors, but: no
  multi-language authoring, weak resource control, and it throws away the
  RFC 002 alignment that makes this investment reusable for WASM actions
  later. Rejected.
- **Composition-only "generator templates"** (parameterized bundles of
  existing tokens, in the spirit of RFC 003). Zero sandbox risk, but
  cannot express a uuid v7 or a Luhn checksum — it composes scarcity.
  Rejected as the mechanism; fine as a complement later.
- **xk6-style recompile-the-engine extensions.** Maximum performance,
  kills the single-static-binary property and needs a Rust toolchain per
  user. Rejected (same conclusion as RFC 002).
- **Libraries as subprocess plugins.** Trivial sandbox story via
  seccomp/containers on some platforms, none on others; per-call process
  overhead on the hot path is fatal. Rejected.

## Rollout plan

0. **This RFC** circulated; WIT interface text finalized during phase 2.
1. **Native-first value**: config schema + alias resolver in `Gen`,
   uniform `${...}` expansion across all actions, `@std/random@v1`
   (native, on the provider trait), error/severity semantics, lint token
   validation, per-library metrics. No wasmtime yet — shippable value with
   zero new heavy dependencies.
2. **WASM runtime**: wasmtime + WIT 0.1 + capability model (fs/net/clock,
   host-mediated HTTP with allowlists) + local-path loading + Rust SDK.
   Dogfood: `@std/random` reimplemented as a WASM component in the public
   `Perfscale/library-random` repo and embedded into the engine at build
   time; the native implementation stays for comparison/benchmarks.
3. **Distribution**: `perfscale install` + `perfscale.lock` + HTTPS/git
   sources + offline cache. **(Shipped in v0.22.0.)**
4. **Agent**: managed install path, digest-keyed cache, fleet policy ∩
   task grant enforcement on perfscaled.
5. **SDK breadth**: TS/JS SDK, Go SDK, author docs, examples; then
   evaluate the WASM-actions RFC on top of this runtime.

Each phase is a working release; phases 3+ are independently deferrable
without stranding phases 1–2.

## Open questions

- Feature-gate wasmtime (`--features wasm-libs`) or unconditional? A gate
  keeps the binary lean for non-users but forks the build/test matrix and
  complicates perfscaled packaging. Leaning: unconditional once phase 2
  lands, gate during development.
- Exact WIT surface: whether `info()` returns JSON or a typed record list
  (JSON is friendlier to JS authors; typed is self-validating), and
  whether `init` failures are fatal to the run or to the library only.
  Leaning: JSON + fatal-to-run, decide with SDK dogfooding.
- Lockfile format and location (project root vs alongside the config
  file), and how `import:` chains interact with a single lock.
- Yank/revocation propagation to agent caches: inherit RFC 002's
  "warn loudly, never silently swap", but the *channel* for revocation
  notices on an offline-capable agent is undesigned.
- Should `${...}` gain an escaping mechanism (`\${literal}`) now that
  expansion is uniform? Cheap to add in phase 1, expensive to retrofit.
- Do generated values participate in `outputs:` capture ergonomics, or is
  the existing capture-what-was-sent flow sufficient?
- **JS library ergonomics (found in phase 2):** jco/StarlingMonkey components
  import `wasi:filesystem/types` + `preopens` + `wasi:clocks/wall-clock`
  unconditionally, so a TS-authored library — even a pure one — needs an
  `fs` grant today (Rust wasip2 components only carry import-free noise for
  cli/io/monotonic-clock, which the engine sinks). Options: a JS-specific
  allowance list in the engine, a `pure-js` source marker in `info()`, or
  QuickJS-based componentize backends that link less WASI. Decide from real
  JS-library adoption; the current fail-closed rule is the safe default.

## Success metrics

- `@std/random` replaces the hardcoded `${uuid}` in all first-party
  examples; real uuid v4/v7 available without any WASM.
- One internal proprietary generator (a FIX id library) runs as an
  external WASM component against the Rust SDK — the dogfood that proves
  the ABI before outsiders depend on it.
- Phase 2 benchmark: WASM `@std/random` within 2× of the native
  implementation per call at 1k VUs, or the gap is documented and
  understood.
- One external author publishes a library (any channel) within a release
  cycle of the SDK shipping.
- Zero capability-model bypasses found in review or pen-test (the metric
  that is only ever "so far so good").
