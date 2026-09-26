# Libraries — custom value generators (WASM)

Every perfscale payload can embed `${...}` generator tokens — `${seq}`,
`${uuid}`, `${rand(1,100)}` — that the engine expands when a message is
sent. **Libraries** are the extension point: a library contributes its own
functions, callable from any payload as `${alias.fn(...)}` with the same
syntax across every protocol (HTTP, WebSocket, gRPC, GraphQL, raw TCP/UDP,
LLM prompts, DB parameters).

A library is either a **built-in** shipped with the engine (`@std/random`)
or a **WASM component** you wrote yourself — this page covers both, with a
focus on authoring your own. The full config surface lives in the
[YAML reference](../yaml-reference.md#libraries-libraries); the design
rationale is [RFC 005](../rfcs/005-libraries.md).

## Declaring and using

```yaml
libraries:
  - use: '@std/random@v1'        # built-in, no files needed

steps:
  - use: std/db-query@v1
    with:
      query: INSERT INTO orders (id, code, who) VALUES (?, ?, ?)
      params:
        - "${random.ulid()}"
        - "${random.pattern("ORD-####-^^")}"
        - "${random.email()}"
```

- `use:` — a built-in `@ns/name@vN` reference (quotes required), a local
  `.wasm` path (relative to the declaring file), an HTTPS URL (requires
  `sha256:`), or a `git+<repo>@<ref>#<path>` reference.
- `as:` — the token prefix. Default is the library's own name.
- `capabilities:` — explicit sandbox grants, see below.
- `with:` — JSON config passed to the library's `init()`; may use
  `${{ env.X }}` so secrets stay masked in logs.

Remote sources (`https:`, `git+`) are **never fetched at run time**:
`perfscale install` downloads each one once, verifies the digest, stores it
in a content-addressed cache, and writes a `perfscale.lock` next to the
declaring file — commit the lock. `run` and `lint` then resolve everything
fully offline.

Token semantics in short:

- Built-in tokens (`${seq}`, `${rand}`, …) match first and are unchanged; an
  alias may not shadow them.
- Unknown alias → the token stays verbatim (backward compatible; `perfscale
  lint` flags it). Known alias + unknown function → the **step fails** — a
  typo must not ship silently into a payload.
- Any function's optional trailing `key` argument **memoizes** the result
  within one message: two `${random.uuid4(order)}` in one message yield one
  id; the next message generates a fresh one.
- `seed: 42` in the config makes a run reproducible — every library instance
  derives its seed as `hash(seed, vu_id, conn_seq)`.
- A failed call (guest error, trap, timeout) fails the step with the cause
  recorded — a load test never sends wrong data and reports green.

## The `@std/random@v1` built-in

Ships with the engine — no WASM, no install, no capabilities. Every
function accepts the optional trailing memo `key`:

| Function | Returns |
|---|---|
| `uuid4([key])` / `uuid7([key])` | Random v4 / time-ordered v7 UUID |
| `ulid([key])` | 26-char Crockford-base32 ULID |
| `nanoid([len], [key])` | URL-safe id (default len 21) |
| `int(a, b, [key])` | Random integer in `[a, b]` |
| `float(a, b, [dp], [key])` | Random float, `dp` decimals |
| `pick(a\|b\|c, [key])` | Random pick among `\|`-separated options |
| `weighted(a:10\|b:90, [key])` | Weighted random pick |
| `seq(name)` | Named monotonic counter, starts at 1 |
| `pattern("ORD-####-????")` | Template fill: `#`→digit, `?`→a-z, `^`→A-Z, `*`→alphanumeric |
| `first_name()` / `last_name()` / `name()` | Random names |
| `username()` / `email()` / `company()` | Random username / email / company |
| `lorem([words])` | Lorem-ipsum words (default 5) |
| `phone()` | Random phone number |
| `date(a, b)` / `timestamp(a, b)` / `datetime(a, b)` | Random dates and timestamps in a range |

## Capabilities and the sandbox

Custom libraries run in a wasmtime sandbox under a **fail-closed**
capability model: the component's WASI imports are checked against the
`capabilities:` grant in YAML, and the host connects exactly the
intersection. A component importing more than granted is a hard load error.

| Capability | Host provides | Boundaries |
|---|---|---|
| `fs` | `wasi:filesystem` preopens | only paths under `fs_root`, read-only |
| `clock` | `wasi:clocks` | wall time also arrives via the call context |

Any non-empty grant requires `allow_library_capabilities: true` in the
config — the same fail-closed pattern as `allow_file_actions`. Raw sockets
are never provided. `wasi:random` is not provided either: libraries draw
all randomness from the seeded PRNG their SDK exposes, which is what makes
`seed:` runs reproducible. Per-call fuel and a wall-time timeout bound what
a runaway component can do on the hot path, and time inside a library call
is counted in the enclosing step's duration — library cost shows up in
metrics, never hidden.

A library entry can also restrict and mask its surface:

```yaml
libraries:
  - use: ./libs/fixer-ids.wasm
    allow: [uuid4, ulid]   # only these functions callable
    log:
      secret: true         # mask every result of this library in the run log
```

## Writing your own library

One shape, three languages — each SDK hides the WIT plumbing
(`perfscale:library@0.2.0`, WASI Preview 2) and gives you a seeded PRNG
(bit-identical across SDKs and the engine's built-in generator),
`memo()` for per-message reuse, argument helpers, and a runtime-free test
harness.

### TypeScript / JavaScript — `@perfscale/library-sdk`

SDK and build tooling live in the
[sdk-libraries](https://github.com/Perfscale/sdk-libraries) repo; install
from npm:

```console
$ npm install @perfscale/library-sdk
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

Build to a WASM component (jco's ComponentizeJS embeds a JS engine):

```console
$ npx perfscale-library-build mylib.ts -o mylib.wasm
```

Then reference the `.wasm` from `libraries:` and call `${mylib.token()}` in
payloads. The SDK's `Ctx` carries `messageSeq`, `iterationSeq`, `vuId`,
`seed`, `timeMs`, plus `ctx.memo()` and `ctx.rng()`; `testCall` runs unit
tests under `node:test` with no WASM involved.

Sandboxing note: jco components always import `wasi:filesystem` and
`wasi:clocks/wall-clock`, so TS libraries need `capabilities: [fs]` even
when pure — one `fs` grant satisfies both.

### Rust — `perfscale-library-sdk`

Full SDK in the main repo:
[`crates/perfscale-library-sdk`](https://github.com/Perfscale/perfscale/tree/main/crates/perfscale-library-sdk),
builds to `wasm32-wasip2` with no extra toolchain. The
[library-random](https://github.com/Perfscale/library-random) repo — the
WASM port of `@std/random` — doubles as the reference implementation and
author template.

### Go — documented recipe (experimental)

TinyGo + `wasm32-wasip2` + `wit-bindgen-go` walkthrough in
[`go/README.md`](https://github.com/Perfscale/sdk-libraries/blob/main/go/README.md)
of the sdk-libraries repo. No SDK crate yet — the WIT contract is the same,
so generated bindings work against any perfscale release speaking
`perfscale:library@0.2.0`.

### Starting from a template

[library-template](https://github.com/Perfscale/library-template) is a
minimal ready-to-build library (greet + memoized token) with the SDK wired
up — clone it, rename, and add functions.

## Availability and linting

Libraries are part of the test/config YAML, so the same definition runs
from the CLI and on your machines. The `@std/random` built-in needs no
files at all; custom WASM components must be present on the load generator
(relative paths resolve against the declaring file, remote refs come from
the install cache). `perfscale lint` loads declared libraries and validates
every `${alias.fn(...)}` token against the functions they actually export —
typos are caught before the run.
