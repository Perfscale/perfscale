# RFC 009: Custom SDK steps — wasm components as `use:` actions

- **Status**: Draft
- **Author**: Perfscale Team
- **Created**: 2026-10-05
- **Requires**: RFC 005 (libraries — the wasmtime runtime, capability model,
  install/lockfile flow, and SDKs this RFC extends),
  RFC 008 (pro step families — the action seam and the in-step metrics
  contract wasm steps must satisfy)
- **Required by**: none

## Summary

Extend the library SDK (`Perfscale/sdk-libraries`) into a general **step
SDK**: the same wasm component that today exports value-generating functions
(`${alias.fn(...)}`) may additionally export **custom steps** dispatchable
from a test's `use:` slot. A step is referenced as
`use: lib/<alias>/<step>` — the `lib/` namespace is reserved, scoped by the
`libraries:` alias, and can never shadow `std/*` or `pro/*`. The component
receives the interpolated `with:` params as JSON plus the step context (step
name, VU id, iteration, run settings) and returns a success flag, an output
JSON value, tagged log lines, a metrics map, and an optional HTTP timing
sample — exactly the `ActionOutput` shape of RFC 008, so wasm steps fold
into pacing, `check:`, thresholds, and the run summary with no new runner
machinery. Capabilities stay at the RFC 005 model, narrowed for v1: **pure
and `fs`/`clock` only — no network egress of any kind for steps in v1**;
protocol I/O remains `std/*`/`pro/*` territory. Save-time BFF validation
checks `lib/` references syntactically and defers step-existence to the
agent, exactly like the existing off-agent pro-gate posture.

This is the "WASM step/actions" follow-up RFC 005 explicitly deferred
("Libraries only produce strings… WASM *step/actions* are a separate future
RFC that will reuse this runtime") and RFC 002's option 2, now paid for on
an existing runtime.

## Motivation

- **The custom-protocol gap is the platform's biggest extensibility hole.**
  Today a user with a proprietary TCP framing, an internal RPC system, a
  message-signing scheme that must wrap a whole request, or a "compute →
  assert → record metrics" step has exactly three options: squeeze it into
  `std/tcp` + generator tokens, ask for an engine PR, or buy a pro family.
  RFC 008 made pro families cheap *for us*; it did nothing for users who
  will never be a product line.
- **The infrastructure is already bought.** RFC 005 shipped the wasmtime
  runtime, the fail-closed capability model, `perfscale install` /
  `perfscale.lock` / the content-addressed cache, the `pure` marker, the
  burn pipeline, and two SDKs. The incremental cost of "a component also
  exports steps" is a new WIT interface and a dispatch arm — the sandbox,
  distribution, and ops story are unchanged.
- **Value generation and steps are one authoring surface.** Real extension
  needs arrive in pairs: a signing *function* (`${sig.token(...)}`) and the
  *step* that uses it (`lib/sig/login`). Shipping them as separate artifact
  types with separate toolchains would double the author's learning curve;
  one component, one build, one `use:` line is the honest ergonomics.
- **The seam is already the right shape.** RFC 008's metrics contract
  (reserved `metrics` key, `_duration`/`_rtt` failure-rate fold,
  `http_sample`, tagged logs) is a serialization format waiting to be a WIT
  record. Wasm steps that satisfy it inherit thresholds, failure rates, and
  summary percentiles for free.

## Non-goals

- **No network egress for wasm steps in v1** — no raw sockets (already
  rejected by RFC 005 for libraries) and no host-mediated `wasi:http` either
  (see "Capabilities for steps" — the can of worms is honest). Steps that
  need to talk to a target use `std/http`, `std/ws`, … from YAML, or wait
  for a follow-up RFC.
- No connection parking across steps (`Context::extensions()`-style live
  handles). A wasm step is stateless per invocation; cross-step reuse is
  `outputs:` + `${{ }}` interpolation, same as libraries' stance on
  cross-message state.
- No changes to the RFC 005 library ABI — `perfscale:library@0.2.0` is
  untouched; steps are an additive world. Existing components keep working.
- No marketplace/registry story (RFC 002 territory) and no pro-tier gating
  of user steps — anyone can author and run a wasm step on the OSS engine.
- No third SDK language. Rust SDK support ships in
  `crates/perfscale-library-sdk` alongside TS and Go, same matrix as RFC 005.
- Replacing pro families. Pro crates keep process-level access (tokio
  sockets, connection registries, GPU collectors) wasm guests deliberately
  never get.

## Guide-level explanation

### What a wasm custom step is

A `.wasm` component — the same artifact a library already is, declared in
the same `libraries:` block — whose `info()` manifest lists one or more
`steps`. Each listed step becomes dispatchable in `use:`:

```yaml
libraries:
  - use: ./libs/acme-signer.wasm
    capabilities: [clock]
    as: sig

steps:
  - name: login
    use: lib/sig/login            # wasm-provided step
    with:
      user: "trader-${{ vu }}"
      password: ${{ env.DESK_PASSWORD }}
  - name: order
    use: std/tcp@v1
    with:
      address: fix.internal:9876
      send: "35=D … 11=${sig.clordid(new)} …"   # same component, value side
  - name: verify
    use: std/check@v1
    with:
      on: login.session
      body_contains: "ok"
```

Everything around the step is unchanged: `${{ }}` interpolation (including
secrets) happens **before** the component is called, generator tokens
(`${sig.clordid(...)}`) resolve through the same component's library world,
`check:` asserts on the step's output, thresholds fold its metrics, and a
failed wasm step is a failed step in every report exactly like a failed
`std/http`.

### The step id: `lib/<alias>/<step>`

- **`lib/` is a reserved namespace**, exactly as `std/` and `pro/` are. A
  built-in will never use it, and `register_action` rejects a handler that
  claims to match `lib/*` (loud panic at startup — a pro crate has no
  business squatting the user namespace). Wasm steps therefore can **never
  shadow** `std/*` or `pro/*`, and pro can never shadow a wasm step.
- **`<alias>` is the `libraries:` alias** (`as:`, or the component's
  default name) — the same scoping token `${alias.fn(...)}` uses, already
  validated as unique per run and barred from colliding with built-in token
  names. Step-name collisions between components are impossible by
  construction.
- **No `@vN` on step ids.** Versioning is the artifact's, not the id's: the
  component version is pinned by the local path, the `sha256:`, or the
  `perfscale.lock` entry — upgrading a step is upgrading the library, which
  the lockfile already makes explicit and reviewable. (`std/*`'s `@v1`
  exists because those ids float free of any pinned artifact; `lib/*` ids
  never do.) The WIT *ABI* major versions exactly as libraries do: the
  engine accepts a range of `perfscale:step` majors and names both versions
  in the load error.
- **Failure modes are loud and specific**: `lib/sig/login` with no `sig`
  alias declared → "unknown library alias 'sig' in step id"; known alias,
  unknown step → step failure listing the component's exported steps;
  component lists no `steps` at all → the same error with "this component
  exports no steps".

### Writing a step

TypeScript (sketch — final API lands with the SDK):

```ts
// acme-signer/entry.ts — one component, both worlds
import { defineLibrary, defineSteps } from "@perfscale/sdk";

export const library = defineLibrary({
  name: "sig",
  pure: false,                       // uses the clock capability
  functions: {
    clordid: { call: (args, ctx) => ctx.memo(args[0] ?? "", (c) => nextId(c)) },
  },
});

export const steps = defineSteps({
  login: {
    description: "Desk login; returns { session, duration_ms }",
    async run(params, ctx) {
      const t0 = ctx.timeMs;
      const session = await deskLogin(params.user, params.password, ctx);
      return {
        success: true,
        output: { session },
        metrics: { sig_login_duration: [ctx.timeMs - t0] },  // → sig_login_failed rate for free
        logs: [{ tag: "out", line: `login ok for ${params.user}` }],
      };
    },
  },
});
```

Go (TinyGo, sketch):

```go
func init() {
    steps.Export(stepdef.Def{
        "login": func(params json.RawMessage, ctx *stepdef.Ctx) (*stepdef.Result, error) {
            // …
            return stepdef.Ok(map[string]any{"session": s}).
                WithHistogram("sig_login_duration", elapsedMs), nil
        },
    })
}
```

Both compile with the existing toolchain commands
(`perfscale-library-build`, the TinyGo pipeline), which gain a second WIT
world to generate against; the build output is one `.wasm`.

### Instance lifecycle and cost

Steps fire per VU per iteration — the same order as library token calls
today, and they reuse the same instance: one component instance per
(component × generator owner), parked alongside `Gen` exactly as RFC 005
already does. There is **no new pooling layer**: the RFC 005
instantiate-per-owner model, fuel limit, and wall-time timeout apply to
`run()` with one amendment — the default step timeout is the step's own
`timeout`-style budget (default 30 s, matching action norms), not the
5 ms library-call budget, and fuel is only a trap bound, not a fairness
mechanism (RFC 005 pitfall 1 applies unchanged). A step is *not*
reentrant: one call per instance at a time, guaranteed by the VU loop being
sequential — the engine never needs instance pools for correctness, only
the existing per-VU parking.

### Capabilities for steps

v1 grants for step-exporting components are the RFC 005 set minus `net`:

| Grant | Host provides | Notes |
|---|---|---|
| *(none)* / `pure` marker | nothing beyond the call context | the common case: compute, transform, assert |
| `fs` | `wasi:filesystem` preopens under `fs_root`, read-only | corpora, fixtures, templates |
| `clock` | `wasi:clocks` monotonic | wall time already arrives via context |

**No network in v1.** Two reasons, stated plainly:

1. *Raw sockets* are the RFC 002 weaponization vector — unchanged, still
   never.
2. *Host-mediated `wasi:http`* (the RFC 005 `net: [hosts]` design) is a can
   of worms *for steps specifically*: a step doing its own HTTP produces a
   timing sample the runner must attribute honestly (was the p95 the target
   or the guest's marshaling?), duplicates `std/http`'s
   retries/TLS/cookie-jar semantics inside a sandbox where none of that is
   tunable from YAML, and invites users to rebuild `std/http` badly instead
   of composing steps. The moment we ship it, it's load-bearing for someone.

The v1 answer is composition: network stays in `std/*`/`pro/*` steps, and
wasm steps do the parts those can't — transform, sign, correlate, assert,
emit metrics. If a concrete "I need the wasm sandbox *and* one HTTP call"
case survives that, it gets its own RFC with the metrics-attribution
question answered on paper first. (Library-side `net:` for value
generators, if it ships per RFC 005, stays library-side; the grant is
declared per entry and the step world does not expand it.)

**Would a future `net` grant move the pro families into wasm? No — net was
never the binding constraint.** Even with a full `wasi:sockets` grant:

1. *Guests have no liveness.* A component runs only while the host is
   calling it — no guest threads, no host-driven ticks. A FIX session must
   heartbeat between orders, a Kafka consumer must fetch, RTP must pace at
   20–66 ms per track; between step calls the guest is frozen. Protocols
   with their own timers cannot live there without a host-polled lifecycle
   — the second, stateful ABI this RFC deliberately does not build.
2. *The engine must see the connection.* Metrics, connection registries,
   iteration-end drain, and stats steps all operate on host-side handles.
   A socket parked in guest state is invisible; exposing it means a host
   call per operation, at which point the host re-implements the protocol
   engine and the guest becomes a thin config shim — the wasm layer buys
   nothing over a native crate and pays the boundary cost per packet.
3. *The load point.* The families worth productizing are the expensive
   ones (media plane, high-rate sessions) — exactly the paths that cannot
   afford a boundary crossing per packet.

What a `net` grant *would* unlock — and the shape it should take when a
concrete case survives composition: host-mediated request/response only
(`wasi:http`-class, no listeners, no raw sockets), host-attributed timing
(the exchange folds like `http_sample`, the guest's own compute time
reported separately), a YAML-declared egress allow-list
(`net: [hosts]`), and per-component call budgets. That class covers
**custom signaling with real HTTP** (e.g. a LiveKit token/room exchange
inside `signal: library`), SOAP-like bounded protocols, and webhook-style
integrations — bounded calls that need no liveness between invocations.
Session and media families it does not cover, and never will cover well:
those stay native pro crates (RFC 008).

### Discovery, lint, and save-time validation

- **Discovery is the `info()` probe, not a new manifest format.** The
  component's existing `info()` JSON gains an optional `steps: [{ name,
  description }]` array alongside `functions`. The engine already probes
  `info()` sandboxed (fuel-limited, no preopens) at load time for every
  wasm library; step registration falls out of the same probe — at
  `validate_libraries` time each declared component's steps are collected
  into a run-scoped `alias → {step names}` map, and `execute_action`
  dispatches `lib/<alias>/<step>` against it. A component that exports the
  step world but lists a step in `steps` it doesn't implement (or vice
  versa) is a load error — the manifest and the ABI may not disagree.
- **`perfscale lint`** loads declared components offline (from cache, as
  today) and validates every `lib/` step id against the probed manifest —
  typos are caught before the run, same standard as `${alias.fn(...)}`.
- **BFF save-time validation never executes wasm.** The controlplane BFF
  has no library cache and no wasmtime dependency, and this RFC adds
  neither. `test_from_value_off_agent` learns the `lib/<alias>/<step>`
  *grammar* only: reserved-prefix check, well-formedness, and — because the
  `libraries:` block is part of the validated document — "alias declared?"
  A typo'd alias fails at save (422) as today; whether `login` exists in
  the component is **deferred to the agent**, which re-validates with the
  cache present and fails the run before the first VU starts — the exact
  off-agent posture the BFF already takes toward pro-module gates. The
  error the agent produces names the component, its exported steps, and the
  save-time-deferred nature of the check. (Alternative considered and
  rejected below: a bytes-readable manifest section in the `.wasm`.)

### Testing a step locally

- **Author-side unit tests**: the SDKs ship a runtime-free harness mirroring
  RFC 005's `testCall` — `testStep(steps, "login", params, ctx)` invokes the
  handler directly and returns the result struct for assertions; Go gets the
  equivalent. No wasm runtime needed.
- **Engine-side**: `perfscale lint` (manifest check) and
  `perfscale run -f test.yaml -c config.yaml` with a local-path `use:` —
  the golden-file pattern the `sdk-libraries` examples already use
  (`hello.test.yaml` / `hello-out.txt`) extends to steps unchanged.
- **SDK CI**: the sdk repo builds every example to `.wasm`, runs the engine
  CLI against fixture YAMLs, and diffs golden output — plus a compat-matrix
  job (SDK × WIT major × engine version) on the model RFC 005 mandates for
  toolchains.

## Reference-level explanation

### The WIT surface

One new interface in a new package, living next to `library.wit` in the SDK
repo and vendored into the engine (single source of truth discipline from
RFC 005):

```wit
package perfscale:step@0.1.0;

interface step {
    /// Same context shape as library calls, plus step identity.
    record step-context {
        iteration-seq: u64,   // VU loop iteration
        vu-id: u64,           // virtual-user id
        time-ms: u64,         // wall clock, unix ms (only time source)
        settings-json: string, // run settings, frozen at run start (RFC 005 shape)
        step-name: string,    // the YAML step's `name:` (for log lines)
    }

    /// Invoke one exported step. `params-json` is the step's `with:` block
    /// with `${{ }}` interpolation already applied by the host (secrets
    /// resolved; they arrive masked-registered exactly as for built-ins).
    /// `result-json` is the ActionOutput shape, below.
    run: func(ctx: step-context, step-name-id: string, params-json: string)
        -> result<string, string>;
}

world perfscale-step {
    export step;
    // WASI imports are connected by the host only per granted capabilities
    // (v1: filesystem, clocks — never sockets, never wasi:http for steps).
}
```

The component exports `world perfscale-step` **in addition to** (not instead
of) `world perfscale-library`. `info()` stays on the library interface and
is the single manifest: it gains `steps`, so a steps-only component exports
both worlds with `functions: []`. Host dispatch by ABI major is unchanged:
a 0.1 `perfscale:step` component on a newer engine just works; an
unsupported major fails to load naming both versions.

`result-json` — the serialized `ActionOutput` (RFC 008), all keys optional
except `success`:

```json
{
  "success": true,
  "output": { "session": "abc", "anything": "goes" },
  "logs": [{ "tag": "out|err|sys", "line": "…" }],
  "metrics": {
    "sig_logins_total": 1,
    "sig_login_duration": [12.4]
  },
  "http_sample": { "duration_ms": 12.4, "status": 200, "failed": false }
}
```

Fold rules are the RFC 008 contract, byte-for-byte: a number is a counter,
an array is histogram samples in milliseconds, `_duration`/`_rtt` names mint
the automatic `<family>_failed` rate, `metrics` stays in the output for
`std/check@v1`, `http_sample` rides the `std/http` path, and a guest that
omits `metrics`/`logs`/`http_sample` gets empty defaults. The host enforces
metric-name hygiene (lowercase, `[a-z0-9_]`) and rejects names reserved by
the engine (`http_req_duration`, `vus`, …) with a step failure — a guest
must not be able to write into the engine's own series. Trapping, exceeding
the wall-time budget, or returning `Err` fails the step with the cause
recorded; severity/metrics handle it like any action error (RFC 005's
"never report green on wrong data" applies to steps verbatim).

### Dispatch integration

`execute_action` (actions.rs) gains one arm, ordered deliberately:

1. built-in `std/*` match (unchanged, never consulted for `lib/` ids);
2. **`lib/` prefix → run-scoped wasm-step dispatch** — split
   `lib/<alias>/<step>`, look the alias up in the run's `LibrarySet` (a
   missing alias is the same "unknown action" failure shape, with a
   targeted message), look the step up in the component's probed manifest,
   then call `run()` on the calling VU's parked instance;
3. registered `ActionHandler`s (pro families) — unchanged;
4. "unknown action" — unchanged, and now listing `lib/` as a possible cause.

`register_action` gains the reservation: a handler whose `matches()` returns
true for any `lib/`… probe id is rejected at registration (panic with the
crate named — registration is startup code and a startup lie is worse than
a startup crash). Dispatch order makes shadowing impossible even if the
check is bypassed: `lib/` is resolved before pro handlers are consulted.

Per-library metrics (RFC 005: calls, errors, p95 call duration) extend to
step calls under the same per-component summary, with a `step_calls` split
so a component's token-call cost and step cost are separately visible.

### Secrets and interpolation

`with:` params are interpolated by the host before `run()` — `${{ env.X }}`
values are resolved and registered in the `SecretRegistry` on the existing
path, so a secret passed into a guest is already masked in every log the
engine emits. What the guest *does* with it is the guest's business, but
guest log lines pass through the same registry masking on the way out (log
lines are engine strings; masking is a registry pass, not a trust
decision). A step that returns a minted secret (session token) marks it via
a new optional result key `secret_output_paths: ["session"]` whose values
the host registers — the guest declares, the engine enforces, same posture
as `FunctionInfo.secret`.

### BFF and agent

- BFF: `controlplane/src/testdef.rs` and the `*_off_agent` entry points gain
  the `lib/` grammar check only (schema-level; alias cross-check against
  the document's own `libraries:`). No wasmtime dependency, no cache, no
  manifest reads — including for `sha256:`-pinned remote components, whose
  bytes the BFF deliberately never fetches.
- Agent (perfscaled): run dispatch already re-validates with full library
  resolution; step-manifest validation joins that path, so a bad step id
  fails the run at dispatch, before VUs spawn. Fleet policy (capability
  intersection, digest allowlists) applies to step-exporting components
  unchanged — from the fleet's perspective it is the same artifact type.

### SDK and repo changes

- **Repo**: `Perfscale/sdk-libraries` is renamed to **`Perfscale/sdk`**
  (GitHub rename keeps redirects); layout stays `ts/`, `go/`. The Rust SDK
  stays in the perfscale workspace (`crates/perfscale-library-sdk`, to be
  renamed `perfscale-sdk` on its next semver window — it is versioned with
  the engine, so the rename rides an engine minor).
- **Packages**: `@perfscale/library-sdk` becomes `@perfscale/sdk`; the old
  package gets one final release that re-exports the new one with a
  deprecation notice. `perfscale-library-build` keeps its name as an alias
  of the new `perfscale-sdk-build` for one release cycle.
- **WIT**: `wit/step.wit` added beside `wit/library.wit`; the package
  versions are independent (`perfscale:step@0.1.0` starts fresh).
- **Migration for existing users**: none. Existing libraries need no
  rebuild (the library ABI is untouched); the YAML surface only grows.
- **Examples**: `examples/hello` in both languages gains a step
  (`lib/hello/echo` + a metrics-emitting `lib/hello/work`), and a new
  `examples/signer` component demonstrates the paired function+step case
  from the guide section. The Go example graduates from "no SDK, working
  example" to a thin `stepdef` package once the surface settles — the RFC
  deliberately does not gate the TS SDK on the Go one.

## Drawbacks

- **Per-step wasm overhead at high VU counts is real.** A `lib/` step pays
  instance memory per VU (RFC 005 pitfall 5) plus JSON marshal/unmarshal of
  params and results on every call — fine for a login step, a lie for
  anyone rebuilding `std/http` in wasm at 10k VUs. The capability scope
  (no net) mostly prevents the worst case by construction, but the docs must
  say plainly: wasm steps are for what built-ins can't do, not a
  replacement authoring model; per-component p95 metrics exist precisely to
  catch misuse.
- **Debugging UX is worse than native.** A failing guest gives a trap or an
  `Err` string; there is no debugger, no stack you can map without
  sourcemaps/dwarf work we are not committing to. Mitigation is the
  runtime-free harness (`testStep`) so logic bugs are caught pre-wasm —
  but "works in the harness, traps componentized" will happen, especially
  on the TinyGo toolchain (which RFC 005 already documents fighting).
- **Capability escalation pressure is now permanent.** Every future
  request — net, env vars, exec — arrives with a concrete user need
  attached, and each grant is one-way. This RFC's v1 line (pure/fs/clock)
  will be litigated in review forever; that is the cost of the sandbox
  commitment RFC 005 already accepted, now with higher stakes because steps
  are where the I/O wants to live.
- **The manifest is self-declared.** `info()` saying `"pure": true` or
  listing steps is the guest's word; enforcement is structural (no preopens,
  import intersection) rather than the manifest being trusted. That holds,
  but every new manifest field is a new place a guest can be *mistaken*
  (not malicious) and produce confusing load errors — the
  manifest-vs-ABI disagreement check exists for exactly this.
- **Two more ABIs to keep compatible forever** (`perfscale:step` plus the
  result-JSON schema, which is a de-facto ABI). The result JSON is
  versioned only by the WIT major — a field rename later means a major
  bump and dual-stack host code.

## Alternatives considered

- **Status quo — pro crates and engine PRs only** (RFC 008's world).
  Works for productized protocols, structurally cannot serve a user's
  private protocol or internal signing scheme. Rejected: it is the gap this
  RFC exists to close.
- **Remote steps over gRPC** (step server as a sidecar; engine calls out per
  step). Full language freedom, real debuggers, no wasm — but per-call IPC
  on the hot path, a deployment story (sidecar per test) the platform has
  deliberately avoided, and a *worse* security story (the step process has
  full host network by default). Rejected for the core mechanism; nothing
  here precludes a future `grpc-step` pro family for shops that want it.
- **Embed a JS engine (rquickjs/boa) for steps.** No toolchain for authors
  and cheap calls, but RFC 005 already weighed and rejected this for
  libraries: single-language authoring, weak resource control, and it
  strands the component-model investment. Rejected for the same reasons;
  steps do not change the calculus.
- **Do nothing / defer until the marketplace (RFC 002).** The runtime is
  shipped and the demand is current; deferring couples a small additive
  feature to a large speculative one. Rejected.
- **Bytes-readable manifest (custom wasm section) so the BFF can validate
  step existence without executing wasm.** Technically clean (`info()` is
  already JSON; embedding it as a custom section is cheap at build time)
  but it requires the BFF to *have the bytes* — which it deliberately
  doesn't (no cache, no fetch of pinned remotes), so it would only work for
  inline/uploaded components and would fork the validation semantics by
  artifact source. Rejected for v1: uniform defer-to-agent, one rule. If
  save-time step-existence validation becomes a real UX complaint, the
  custom section is the way to add it — the manifest JSON is stable either
  way, so the door stays open without an ABI change.
- **`use: <alias>/<step>@v1` instead of `lib/<alias>/<step>`.** Closer to
  `std/*` cosmetics, but the `@vN` would promise per-step versioning the
  artifact model cannot honor (the lockfile pins the component, not a step),
  and an un-prefixed id makes the std/pro/lib collision rules harder to
  state. Rejected: `lib/` is uglier and honest.
- **Port the pro families themselves to wasm steps** (perfscaled ships
  baked-in components instead of linked crates). Attractive uniformity —
  one authoring model, hard dogfooding, artifact updates without an agent
  release, better IP obfuscation than a linked binary — but it collides
  with the two hardest constraints of the step model:
  - *No net in the sandbox.* Every pro family today IS a network protocol
    (FIX sessions, SOAP over HTTP, Kafka/MQTT/Redis drivers, the entire
    WebRTC media plane). Porting them requires exactly the raw-socket
    capability v1 rejects — for webrtc not just sockets but DTLS-SRTP
    crypto and the ICE state machine inside the guest.
  - *A step call is bounded; a pro connection is not.* Pro connect steps
    park live connections (peer connections, Kafka producers, FIX sessions)
    in Rust registries, and background tokio tasks drive media pacing and
    sampling across the whole VU iteration and beyond. A wasm step is a
    bounded call with JSON in/out: it cannot hold engine-side resources,
    spawn host tasks, or pace RTP on the monotonic clock from inside the
    guest. Supporting that means a second, stateful component lifecycle —
    a far larger ABI than this RFC's — for negative gain: the hot paths
    would pay the wasm-boundary cost per packet.
  The distribution upside (module updates without an agent release) is real
  but does not require wasm: pro crates are git-pinned deps baked at agent
  build, the agent release cadence already ships module updates, and a
  baked-in wasm artifact would trade a compile-time check for a runtime one
  with no change in trust (a proprietary blob either way). Rejected: pro
  protocol families stay native crates linked into perfscaled (RFC 008;
  with `perfscale-fix` wired, the agent ships the full catalog). SDK steps
  target user-side custom logic — signaling glue, transformations,
  assertions, business metrics — that composes `std/*`/`pro/*` network
  primitives. The supported pipeline runs the other way: a `lib/` step that
  proves broadly useful gets promoted into a native pro family.
- **`net:` for steps in v1.** Covered in the guide section — rejected as
  premature; the metrics-attribution and YAML-tunability questions must be
  answered before the sandbox learns HTTP, not after.

## Resolved during review

*(Draft — nothing resolved yet. Staged for review:)*

- **Step id shape**: `lib/<alias>/<step>`, no `@vN`; versioning is the
  pinned artifact's. Open to review, but the burden of proof is on any
  scheme that reintroduces per-step version ids.
- **v1 capability line drawn at pure/fs/clock, no `wasi:http` for steps** —
  flagged as the decision most likely to be relitigated; see the rejection
  rationale before proposing to move it.
- **BFF validation defers step-existence to the agent** (grammar + alias
  only at save) rather than gaining any wasm/bytes access.
- **SDK rename**: `sdk-libraries` → `sdk`, `@perfscale/library-sdk` →
  `@perfscale/sdk` with a deprecation alias; engine crate rename rides an
  engine minor.
- **Left undecided deliberately**: the default wall-time budget for a wasm
  step (30 s proposed, vs. inheriting per-action norms); whether
  `secret_output_paths` is paths or a flat list of output keys; whether
  `perfscale:step` and `perfscale:library` package versions are allowed to
  drift or must bump in lockstep; the exact Rust SDK API shape (deferred to
  implementation, TS/Go sketches above are the contract of intent).
