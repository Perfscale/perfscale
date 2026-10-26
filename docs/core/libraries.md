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
    capabilities: []             # required key — [] means "no grants"

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
- `capabilities:` — explicit sandbox grants, see below. **Required on every
  entry** — an empty list (`capabilities: []`) means "no grants". Omitting
  the key is a validation error. Migration from older documents: add
  `capabilities: []` to every entry that has no grants.
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
- `seed: 42` in the config makes a run reproducible — every library call
  carries a per-message seed derived as `hash(seed, vu_id, conn_seq)`, so
  two seeded runs mint the same values.

### Instance lifecycle

WASM instances are expensive to create (`Store::new` + WASI wiring +
`init()`), so the engine keeps **one instance per library per VU**, reused
by every message and connection that VU runs — not one per step. `init()`
therefore runs once per VU, and instance state persists across that VU's
messages; per-message isolation (memo keys) still holds because every
message carries a distinct `message_seq`. Memory: a fresh store is lazy —
measured ~0.5 MiB RSS per instance for a small component (500 VUs ≈
250 MB total engine RSS; the 64 MiB limit is a cap, not an allocation), so
even 1000 VUs with a WASM library stay in the hundreds of MB — but a
component that eagerly allocates its linear memory multiplies that. The
built-in `@std/random@v1` is *not* shared: its PRNG is seeded per instance
(`hash(config.seed, vu_id, conn_seq)`), and sharing would change the value
sequence — it stays per-generator.

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

## The `@std/json@v1` built-in

Ships with the engine — no WASM, no install. Reads JSON files into
generator tokens; the structured counterpart of the
[`std/read-json@v1`](actions.md#stdread-jsonv1) step:

```yaml
libraries:
  - use: '@std/json@v1'          # default alias: json
    capabilities: []

steps:
  - use: std/http@v1
    with:
      method: POST
      url: "https://api.example.com/login"
      body: '{"token": ${json.read("users.json", ".users[0].token")}}'
```

| Function | Returns |
|---|---|
| `read(file)` | The whole parsed document, as a compact JSON string |
| `read(file, query)` | The subvalue at a jq-style path (`.posts[0].id` ≡ `posts.0.id` — the same grammar as the `query:` parameter of the file steps); a string leaf verbatim, other values as compact JSON |
| `pick(file)` / `pick(file, index)` | An element of the JSON array in `file` — uniformly random per message, or the deterministic `index` modulo the array length; a string element verbatim, other values as compact JSON |

Notes:

- Parsed documents come from the **process-wide cache shared with
  `std/read-json@v1`**, revalidated by `(mtime, size)` — the first call pays
  the disk read and parse, every later call (step or library) hits RAM.
- **Fail-closed**: the run must set `allow_file_actions: true` (and paths
  honour `fs_root`), exactly like the `std/file-*@v1` steps.
- Error semantics differ from the step: a missing mandatory path segment is
  a token-expansion error naming the segment (fails the step it appears in),
  while a `?`-tolerant miss (`json.read("f.json", ".users?[0]")`) resolves
  to an **empty string** — interpolation-friendly, where the step yields a
  `null` output.
- `pick` respects seeded runs: the random index derives from the same
  deterministic pipeline as the rest of the engine (FNV-1a over the per-call
  seed — which folds in the run `seed:`, VU id, and generator sequence — plus
  the message sequence and file path), so a `seed:` run replays the exact
  pick sequence, unseeded runs are random, and repeat calls within one
  message pick the same element. A document that is not a JSON array, an
  empty array, or a non-integer `index` is a call error.

Like `@std/random@v1`, `@std/json@v1` is **opt-in**: nothing is
auto-registered — without the `libraries:` entry above, `${json.*}` tokens
do not resolve.

## Capabilities and the sandbox

Custom libraries run in a wasmtime sandbox under a **fail-closed**
capability model: the component's WASI imports are checked against the
`capabilities:` grant in YAML, and a component importing more than granted
is a hard load error. The one exception is a **self-declared pure library**:
when the component's `info()` reports `"pure": true`, its `wasi:filesystem`
and `wasi:clocks/wall-clock` imports are treated as toolchain noise
(jco-built TS/JS components always carry them) and no grant is needed. The
sandbox still holds — without an `fs` grant the guest gets zero preopens, so
any actual file access fails at runtime.

| Capability | Host provides | Boundaries |
|---|---|---|
| `fs` | `wasi:filesystem` preopens | only paths under `fs_root`, read-only |
| `clock` | `wasi:clocks` | wall time also arrives via the call context |
| `net: [hosts]` | `wasi:http` outgoing-handler | destination host/port checked against the allowlist on every request |

`net` takes a host allowlist instead of a bare flag:

```yaml
capabilities:
  - net: ["api.acme.com", "api.acme.com:8443", "*.internal"]
```

Entry forms (case-insensitive): `api.acme.com` allows that host on **any**
port; `api.acme.com:8443` pins the port; `*.internal` allows any host under
the suffix (`db.internal`, `a.b.internal` — but not bare `internal` or
`internal.evil.net`). A request to a destination outside the allowlist fails
with an error naming the destination. Fuel does not see time spent inside a
host HTTP call, so the host bounds it instead: **30 s per request** (connect,
first byte, between bytes, and the overall send) and a **16 MiB cap** on both
request and response bodies. **Raw `wasi:sockets` is never provided** — an
allowlist is only enforceable while the host mediates the connection.

Any non-empty grant requires `allow_library_capabilities: true` in the
config — the same fail-closed pattern as `allow_file_actions`. The
`capabilities:` key itself is required on every `libraries:` entry
(built-ins included): write `capabilities: []` when the library needs no
grants — omitting the key is a validation error at load, lint, and run.
Raw sockets are never provided. `wasi:random` is not provided either: libraries draw
all randomness from the seeded PRNG their SDK exposes, which is what makes
`seed:` runs reproducible. Per-call fuel and a wall-time timeout bound what
a runaway component can do on the hot path, and time inside a library call
is counted in the enclosing step's duration — library cost shows up in
metrics, never hidden.

A library entry can also restrict and mask its surface:

```yaml
libraries:
  - use: ./libs/fixer-ids.wasm
    capabilities: []       # no sandbox grants
    allow: [uuid4, ulid]   # only these functions callable
    log:
      secret: true         # mask every result of this library in the run log
```

## Writing your own library

One shape, three languages — each SDK hides the WIT plumbing
(`perfscale:library@0.3.0`, WASI Preview 2) and gives you a seeded PRNG
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

For a library-owned top-level config block validated against JSON Schema
(ABI 0.3), see [Config extensions](../library-sdk.md#config-extensions-wit-03)
in the SDK guide and the [YAML reference](../yaml-reference.md).

Sandboxing note: jco components always import `wasi:filesystem` and
`wasi:clocks/wall-clock`. A TS library that declares `"pure": true` in its
`info()` metadata needs no `capabilities:` grant for those imports — the
engine links the interfaces but provides no preopens, so real file access
would still fail. Libraries that do read files keep declaring
`capabilities: [fs]` (one `fs` grant satisfies both interfaces).

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
`perfscale:library@0.3.0`.

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
