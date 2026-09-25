# YAML reference

Two files drive a native run: the **test definition** (`-f`) describes *what*
to do; the **config** (`-c`) describes *how much load* to apply.

Both are validated against JSON Schemas before execution — errors point at
the offending field path, not a raw parser dump. The schemas live in
[`schema/`](../schema/) and can drive editor autocomplete via a modeline:

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/Perfscale/perfscale/main/schema/test.schema.json
```

## Test definition (`-f test.yaml`)

A single `steps` array. Each virtual user (VU) executes the whole list in a
loop until the configured duration expires. An optional top-level `import:`
inherits a base document — see
[Composing documents](#composing-documents-import). An optional top-level
`libraries:` declares value-generator libraries for `${alias.fn(...)}`
tokens — see [Libraries](#libraries-libraries).

```yaml
steps:
  - name: login                     # optional label used in log output
    use: std/http@v1                # required — action ID
    with:                           # action parameters (see below)
      method: POST
      url: https://api.example.com/login
      body:
        user: demo
    check:                          # optional inline assertions
      status: 200
      duration_ms_lt: 500
    outputs: login                  # optional — store output for later steps
```

### Step fields

| Field | Required | Description |
|---|---|---|
| `use` | yes | Action ID: `std/http@v1`, `std/graphql@v1`, `std/tcp@v1`, `std/udp@v1`, `std/pubsub@v1`, `std/llm@v1`, `std/ws@v1`, `std/ws-connect@v1`, `std/ws-send@v1`, `std/ws-recv@v1`, `std/ws-ping@v1`, `std/ws-close@v1`, `std/grpc@v1`, `std/grpc-connect@v1`, `std/grpc-call@v1`, `std/grpc-stream-open@v1`, `std/grpc-stream-send@v1`, `std/grpc-stream-recv@v1`, `std/grpc-stream-close@v1`, `std/db-connect@v1`, `std/db-query@v1`, `std/db-tx-begin@v1`, `std/db-tx-commit@v1`, `std/db-tx-rollback@v1`, `std/db-close@v1`, `std/check@v1`, `std/sleep@v1`, `std/log@v1`, `std/file-read@v1`, `std/file-write@v1`, `std/child_process@v1`, `std/kill_process@v1`, `std/thresholds@v1`, `std/set_shared_variable@v1`, `std/get_shared_variable@v1` (short aliases `http`, `graphql`, `tcp`, `udp`, `pubsub`, `llm`, `ws`, `ws-connect`, `ws-send`, `ws-recv`, `ws-ping`, `ws-close`, `grpc`, `grpc-connect`, `grpc-call`, `grpc-stream-open`, `grpc-stream-send`, `grpc-stream-recv`, `grpc-stream-close`, `db-connect`, `db-query`, `db-tx-begin`, `db-tx-commit`, `db-tx-rollback`, `db-close`, `check`, `sleep`, `log`, `file-read`, `file-write`, `child_process`, `kill_process`, `thresholds`, `set_shared_variable`, `get_shared_variable` also work). `uses:` is accepted as an alias for `use:` |
| `name` | no | Human-readable label shown in log lines |
| `with` | no | Action parameters — see [Actions](core/actions.md) |
| `check` | no | Assertions on this step's output — same keys as `std/check@v1` |
| `outputs` | no | Variable name to store the step output under |
| `severity` | no | `std/thresholds@v1` only: what a violated gate becomes — `fail` (default; the run exits non-zero), `warn`, or `info` |
| `message` | no | `std/thresholds@v1` only: custom label appended to the violation summary (interpolated) |

### Variables (`${{ ... }}`)

Steps pass data to later steps through GitHub-Actions-style placeholders.
A step stores its output under the name given by `outputs:`; any **string
value** in a later step's `with:` or `check:` can then reference it:

| Expression | Resolves to |
|---|---|
| `${{ name }}` | The whole stored output, stringified |
| `${{ name.field }}` | One field of a stored object (e.g. `.status`, `.body`, `.duration_ms`) |
| `${{ name.a.b }}` | Nested path — one JSON level per `.`, e.g. `${{ resp.headers.x-request-id }}` |
| `${{ __last__ }}` / `${{ __last__.field }}` | The immediately preceding step's output — always available, no `outputs:` needed |
| `${{ config.<name>.<field> }}` | Output of a config-file `before:` setup step (see [Config](#config--c-configyaml)) |
| `${{ vars.<key> }}` | A static value from the config file's `variables:` block |
| `${{ env.NAME }}` | Process environment variable `NAME` — for secrets (`api_key`, tokens, DSNs) that must not live in the YAML |

For `std/http@v1` the stored output is
`{ "status": <int>, "body": <string>, "duration_ms": <float>, "headers": <object> }`
— header names lowercase, so a session/token header from response 1 can flow
into request 2:

```yaml
steps:
  - use: std/http@v1
    with: { url: "https://api.example.com/first" }
    outputs: r1
  - use: std/http@v1
    with:
      url: "https://api.example.com/second"
      headers:
        x-session: "${{ r1.headers.x-session }}"
```

```yaml
steps:
  - name: login
    use: std/http@v1
    with:
      method: POST
      url: "https://api.example.com/token"
      body: { user: demo }
    outputs: auth                          # ← stored as `auth`

  - name: fetch profile
    use: std/http@v1
    with:
      url: "https://api.example.com/me"
      headers:
        authorization: "Bearer ${{ auth.body }}"   # nested values work
    check:
      body_contains: "${{ auth.body }}"            # check values too

  - use: std/log@v1
    with:
      message: "login took ${{ auth.duration_ms }}ms → ${{ auth.status }}"
```

Rules and edge cases:

- Placeholders work in string values at **any depth** of `with`/`check` —
  nested objects (headers), array elements, bodies. Keys are never
  interpolated.
- Whitespace inside the braces is ignored: `${{auth.status}}` ≡
  `${{ auth.status }}`.
- A **missing variable or field resolves to an empty string** — the run does
  not fail. Gate on the value with `check:` when absence should be an error.
  **Exception:** `${{ env.NAME }}` with `NAME` unset **fails the step** with
  `env var 'NAME' is not set` before the action runs — a silently empty
  `api_key` would surface as a confusing 401 from the API instead.
- `${{ env.NAME }}` reads the process environment of the `perfscale` run
  (everything after `env.` is the variable name). `env` is a reserved
  prefix: a stored output literally named `env` is shadowed by it. Resolved
  values are substituted into step parameters only — the engine never
  writes them to logs or summaries itself. It also records every resolved
  `env.*` value into a run-scoped registry and **masks it in the run log**:
  values of 4+ characters are replaced with `***` wherever they occur
  (inside URLs and DSNs too), shorter values only as whole tokens — so a
  secret that slips into a request URL or a `std/log@v1` message still
  cannot leak into the output. This is why `env.*` is the right channel
  for secrets.
- Path segments descend one JSON object level per `.` — header names with
  dots in them cannot be addressed (rare; everything else works).
- Placeholders are resolved per virtual user, per iteration — each VU sees
  the outputs of its own step chain, never another VU's.
- Steps without any `${{` are executed as-is: the engine skips the
  interpolation pass entirely, so plain steps pay zero overhead for this
  feature.
- YAML quoting: both plain (`Bearer ${{ auth.body }}`) and quoted
  (`"${{ auth.body }}"`) scalars work; quote when the value starts with a
  character YAML treats specially (`{`, `[`, `*`, …).

## Config (`-c config.yaml`)

```yaml
vus: 10          # virtual users, default 1
duration: 5m     # "30s", "1m", "5m30s", "1h" — default "1m"

report:          # optional — forward metrics to a perfscale serve instance
  url: http://localhost:7999
  during_run: true      # also stream cumulative snapshots while running

gpu:             # optional — GPU metrics during the run (native engine)
  enabled: true
  interval_ms: 1000
  source: nvidia-smi
```

| Field | Default | Description |
|---|---|---|
| `vus` | `1` | Concurrent virtual users (fixed profile) |
| `duration` | `1m` | Wall-clock run length; bare numbers are seconds |
| `stages` | — | Ramping-VU profile (k6-style): list of `{ duration, target }` stages. Overrides `vus`/`duration`; mutually exclusive with `arrival`. Native engine only |
| `arrival` | — | Arrival-rate profile (open model): `{ max_vus, pre_allocated_vus?, stages: [{ duration, rate }] }` — hold an iterations/sec rate. Mutually exclusive with `stages`. Native engine only |
| `report.url` | — | A `perfscale serve` base URL; the CLI `--report` flag overrides it |
| `report.during_run` | `false` | Also stream cumulative metric snapshots during the run (batched POSTs to `<url>/api/v1/metrics`), not just the summary at the end — see [During-run metrics](core/metrics.md#during-run-metrics-reportduring_run). Native engine only |
| `report.interval_ms` | `5000` | During-run snapshot/flush interval, milliseconds (min 1000) |
| `report.batch_size` | `500` | Flush a batch once it holds this many samples |
| `report.max_cpu_percent` | `90` | CPU gate: while host CPU% ≥ this, batches are held (no POSTs) and snapshots queued — never dropped — until the gate opens; `0` disables; inert off-Linux |
| `report.max_pending` | `24` | Soft cap on undelivered batches — crossing it warns, but batches are kept and delivered in order, never dropped mid-run |
| `gpu` | — | GPU metrics collection — see [GPU metrics](#gpu-metrics-gpu). Native engine only |
| `before` | `[]` | One-time setup steps — see [Setup and variables](#setup-and-variables) |
| `after` | `[]` | One-time teardown steps — see [Teardown](#teardown-after) |
| `variables` | `{}` | Static values exposed to steps as `${{ vars.* }}`. Keep secrets out of this block — it is plain YAML, checked into repos and shown in diffs; use `${{ env.NAME }}` (process environment, masked in run logs) for those |
| `shared_variables` | `{}` | Mutable cross-VU shared state for `std/set_shared_variable@v1` / `std/get_shared_variable@v1`: a map of name → initial JSON value (the type is inferred from it). Declaring is mandatory — a step referencing an undeclared name, or an `op` incompatible with the declared type, fails validation before the run starts. See [Shared variables guide](core/shared-variables.md). Native engine only |
| `allow_process_actions` | `false` | Let steps spawn/signal OS processes (`std/child_process@v1`, `std/kill_process@v1`). Fail-closed: a step list from an untrusted source cannot touch processes until you opt in |
| `allow_library_capabilities` | `false` | Let `libraries:` entries carry non-empty `capabilities:` grants. Fail-closed, same pattern as `allow_file_actions` |
| `seed` | — | Run-level determinism seed: every `${…}` generator and library instance derives its seed as `hash(seed, vu_id, conn_seq)`, so a seeded run reproduces the same generated values. Wall-clock time stays non-deterministic |
| `libraries` | — | Value-generator libraries for `${alias.fn(...)}` tokens — see [Libraries](#libraries-libraries) |
| `import` | — | Base document to inherit from — see [Composing documents](#composing-documents-import) |

### GPU metrics (`gpu:`)

Sample the host's GPUs (utilization, VRAM, temperature, power) for the whole
run and land a `gpu` section in the run summary — primarily to correlate
[`std/llm@v1`](core/llm.md) load with GPU state. Off by default; collection
failures (no GPU, missing tooling) log one warning and never fail the run.

```yaml
gpu:
  enabled: true
  interval_ms: 1000                          # default 1000 (min 10)
  source: nvidia-smi                         # nvidia-smi (default) | dcgm
  dcgm_url: http://127.0.0.1:9400/metrics    # for source: dcgm
  devices: [0, 1]                            # optional; default — all GPUs
```

Full guide: [core/gpu.md](core/gpu.md).

### Load profiles

The native engine supports three load profiles. `stages`/`arrival` override
`vus`/`duration` (lint warns if both are set) and are mutually exclusive;
the run length is the sum of the stage durations.

**Fixed** (the default): `vus` workers loop the steps for `duration`.

```yaml
vus: 10
duration: 5m
```

**Ramping VUs** (`stages:`, k6-style): the target VU count interpolates
linearly between stage targets — the first stage ramps from 0, each next one
from the previous stage's `target`. Scale-down is graceful: a VU being
ramped away finishes its in-flight step and exits at the next step boundary.

```yaml
stages:
  - { duration: 30s, target: 10 }  # ramp 0→10 VUs
  - { duration: 1m,  target: 50 }  # ramp 10→50 VUs
  - { duration: 30s, target: 0 }   # ramp down, drain to zero
```

**Arrival-rate** (`arrival:`, open model): the engine holds an
iterations-per-second rate profile and scales a worker pool to keep up —
new iterations start on schedule even when the system under test slows down
(where a fixed-VU loop would stretch). `rate` ramps linearly between stages
(fractions allowed: `0.5` = one iteration every 2s).

```yaml
arrival:
  max_vus: 100              # worker pool cap — required, ≥ 1
  pre_allocated_vus: 10     # workers spawned up front (default 1); the pool grows lazily to max_vus
  stages:
    - { duration: 30s, rate: 5 }    # ramp 0→5 iterations/sec
    - { duration: 1m,  rate: 20 }   # ramp 5→20 iterations/sec
```

A permit that arrives while all `max_vus` workers are busy is dropped and
counted in the `dropped_iterations` summary metric (plus a warning logged at
most once per 5s) — raise `max_vus` or lower the rate when it grows.

For `stages`/`arrival` runs the summary's `vus` line reports the observed
concurrency (`vus....................: <last> min=<min> max=<max>`), the
periodic `[stats]` line gains a trailing `vus=N` field, and summary exports
(`--summary-export`) report `vus: null` with `duration` set to the summed
stage length.

Stage durations use the same `"30s"`/`"1m30s"`/`"1h"` grammar as `duration`
but are validated strictly: an unparseable or zero stage duration fails the
run (and `perfscale lint`) with a clear error. `stages` and `arrival` are
native-engine only — with `--locust` they're rejected (use `vus`/`duration`),
with `--k6` the config file is ignored anyway. See
[examples/ramping.config.yaml](../examples/ramping.config.yaml),
[examples/spike.config.yaml](../examples/spike.config.yaml), and
[examples/arrival-rate.config.yaml](../examples/arrival-rate.config.yaml).

### Libraries (`libraries:`)

Libraries provide value-generating functions for `${...}` generator tokens
(RFC 005) — real UUIDs, ULIDs, faker-style data, dates — beyond the
hardcoded `${seq}`/`${rand(a,b)}`/`${uuid}`/`${now}` built-ins. Declared in
the config file, the test file, or both (declarations concatenate; a
duplicate alias is a validation error):

```yaml
libraries:
  - use: '@std/random@v1'          # built-in; default alias: random
  - use: '@std/random@v1'
    as: ids                        # alias → token prefix ${ids.fn(...)}
  - use: ./libs/fixer-ids.wasm     # local WASM component (path relative to this file)
  - use: 'https://vendor.example.com/fixer-ids.wasm'   # remote: perfscale install
    sha256: '9f2c…64 hex…'
  - use: 'git+https://github.com/org/repo.git@v1.2.3#libs/fixer-ids.wasm'  # git: perfscale install
```

Payloads in actions that expand `${...}` (http, ws, gRPC, GraphQL, tcp, udp,
llm, db parameters, pubsub, file-write — everything except std/log) can then
call the library's functions:

```yaml
with:
  send: '{ "id": "${random.ulid()}", "who": "${random.email()}", "n": "${ids.int(1000,9999)}" }'
```

Rules:

- Built-in tokens (`${seq}`, `${rand}`, …) match first and are unchanged.
  An alias may not shadow their names.
- Unknown alias → token left verbatim. Known alias + unknown function (or
  bad arguments) → the step **fails** — a typo must not ship silently into
  a payload.
- Arguments map text → JSON by a fixed contract: split on commas not inside
  double quotes, trim, strip one pair of surrounding quotes, parse as JSON
  if possible, else treat as a string. So `pick(a|b|c)` passes one string
  `"a|b|c"` and `int(1,100)` passes two numbers.
- Any function's optional trailing `key` argument memoizes the result
  **within one message**: `${random.uuid4(order)}` appearing twice in one
  message yields one id; the next message generates a fresh one.
- `seed: 42` in the config makes a run reproducible: per-instance seeds
  derive as `hash(seed, vu_id, conn_seq)`.
- `capabilities:` grants (`fs`, `clock`, or `{ net: [hosts] }`) require
  `allow_library_capabilities: true` (fail-closed, same pattern as
  `allow_file_actions`). `@std/random@v1` declares no capabilities —
  granting it any is a validation error.
- Local paths (`use: ./libs/fixer-ids.wasm`) load WASM component libraries
  (WASI Preview 2, `perfscale:library@0.1.0` WIT — see
  [RFC 005](../rfcs/005-libraries.md)). Paths resolve **relative to the
  declaring file's directory**, like `import:` paths. The perfscale CLI
  binary ships WASM support; embedders of `perfscale-core` need the
  `wasm-libs` cargo feature.

Remote sources (`https://…` and `git+…`) are **distribution refs** — they
are never fetched at run time. `perfscale install <files>` fetches each
remote library once, verifies its digest, stores it in the
content-addressed cache (`<cache>/libraries/<sha256>.wasm`), and writes
`perfscale.lock` next to the declaring file. `perfscale run` and
`perfscale lint` then resolve every remote ref through lock + cache,
**fully offline**: a missing lock, a missing entry, or a missing cache
artifact is a hard error that says to run `perfscale install`.

```yaml
# HTTPS: sha256 is required and pinned — a re-published artifact with a
# different digest is a hard error, never a silent swap.
- use: 'https://vendor.example.com/fixer-ids.wasm'
  sha256: '9f2c…(64 hex)…'

# git: repo URL + ref (tag/branch/commit) + artifact path inside the repo.
# The ref is resolved to a commit at install time and pinned with the
# artifact digest; `perfscale install --refresh` re-resolves it.
- use: 'git+https://github.com/org/repo.git@v1.2.3#libs/fixer-ids.wasm'
```

`perfscale.lock` is TOML, keyed by the exact `use:` string, and meant to be
committed:

```toml
version = 1

[[libraries]]
use = "https://vendor.example.com/fixer-ids.wasm"
sha256 = "…"

[[libraries]]
use = "git+https://github.com/org/repo.git@v1.2.3#libs/fixer-ids.wasm"
commit = "…resolved sha…"
sha256 = "…artifact digest…"
```

Notes:

- The cache honors `PERFSCALE_CACHE_DIR` (then `XDG_CACHE_HOME/perfscale`,
  default `~/.cache/perfscale`) — the same root as the `import:` git clone
  cache.
- Libraries declared in a git-imported document (`import: { git: … }`) are
  pinned by the `perfscale.lock` **inside that repository** (at its root);
  run `perfscale install` on the importing file and it is written there.
  Install follows `import:` chains (including remote ones) for exactly
  this reason.
- For `git+` refs a YAML `sha256:` is optional: the commit pin is the
  integrity anchor. When present, install verifies it and a mismatch is a
  hard error.

WASM library rules:

- The sandbox is fail-closed: the component's WIT imports are inspected at
  load, and anything beyond the YAML `capabilities:` grant is a hard load
  error naming the needed vs granted capabilities. `fs` is a read-only
  preopen confined to `fs_root` (default: the directory containing the
  `.wasm`); `clock` is `wasi:clocks`; `net: [hosts]` is **not yet
  supported** (a `net` grant fails the load with a clear error);
  `wasi:random` and raw sockets are never provided — libraries draw
  randomness from the seeded PRNG (`ctx.seed`) in their SDK.
- `with:` is passed to the component's `init()` as JSON; an init failure is
  fatal to the run.
- A trapping or runaway call fails the step: every call runs under a
  per-call fuel budget (~50M) and each instance is capped at 64 MiB of
  memory.
- Author libraries in Rust with
  [`perfscale-library-sdk`](../crates/perfscale-library-sdk/README.md).

`@std/random@v1` functions:

| Function | Returns |
|---|---|
| `uuid4([key])` | Random RFC 4122 v4 UUID |
| `uuid7([key])` | Time-ordered RFC 9562 v7 UUID (48-bit unix-ms prefix) |
| `ulid([key])` | 26-char Crockford-base32 ULID (unix-ms prefix) |
| `nanoid([len], [key])` | URL-safe id from `A-Za-z0-9_-` (default len 21) |
| `int(a, b, [key])` | Random integer in `[a, b]` inclusive |
| `float(a, b, [dp], [key])` | Random float in `[a, b]`, `dp` decimals (default 2) |
| `pick(a\|b\|c, [key])` | Random pick among `\|`-separated options |
| `weighted(a:10\|b:90, [key])` | Weighted random pick among `value:weight` pairs |
| `seq(name)` | Named monotonic counter per generator instance, starting at 1 |
| `pattern("ORD-####-????")` | Template fill: `#`→digit, `?`→a-z, `^`→A-Z, `*`→alphanumeric |
| `first_name()` / `last_name()` / `name()` | Random names (small embedded corpora) |
| `username()` / `email()` / `company()` | Random username / email / company |
| `lorem([words])` | Lorem-ipsum words (default 5) |
| `phone()` | Random phone number (`+1-NNN-NNN-NNNN`) |
| `date(a, b)` | Random date between `YYYY-MM-DD` a and b inclusive, `YYYY-MM-DD` out |
| `timestamp(a, b)` | Random unix-ms timestamp in `[a, b]` |
| `datetime(a, b)` | Random RFC 3339 datetime between dates a and b (whole end day included) |

### Composing documents: `import`

Both test definitions and configs accept a top-level `import:` naming a base
document. The base loads first (it may import its own base, recursively),
then the current document deep-merges on top: **objects merge key-by-key,
scalars and arrays (including `steps:`) are replaced** by the importing
side. The one exception is `libraries:`, which concatenates across the
chain — a duplicate alias is a validation error.

```yaml
# team config — inherits the org-wide base, overrides one variable
import: ../shared/_base.yaml
variables:
  region: us
```

Three source forms:

```yaml
# relative filesystem path (resolved against the importing file's directory)
import: ../shared/_base.yaml

# raw HTTP(S) URL — pin the ref in the path
import: "https://raw.githubusercontent.com/org/repo/v1.2.0/perf/config/_base.yaml"

# any git host (SSH or HTTPS remotes, self-hosted included)
import:
  git: git@gitlab.example.com:group/repo.git
  ref: v1.2.0          # tag, branch, or commit SHA
  file: perf/config/_base.yaml
```

Remote imports (URL and git) are **fail-closed**: they run only when the
caller passes `--allow-remote-import`. The permission belongs to the caller
because import resolution happens before the `allow_file_actions` /
`allow_process_actions` gates — a remote base could otherwise grant itself
those rights and pull in a `std/child_process@v1` step. A document can never
opt itself into the network.

Origins stay confined: a document fetched from a URL resolves relative
imports against its own URL; a document from a git repo may only import
files inside that same clone (`../` escapes are rejected). A remote document
can never read your local filesystem. Import cycles fail with the chain
printed.

Git imports clone with `--depth 1` through your system `git` (SSH keys and
credential helpers apply) and cache under `~/.cache/perfscale/imports/`.
Tags and commit SHAs are immutable — cached forever. Branches revalidate
against the remote after a short TTL, so `ref: main` follows the branch;
`--refresh-imports` forces a refetch. The full guide lives in
[docs/core/imports.md](core/imports.md).

### Setup and variables

`before:` steps run **once**, in order, before any VU starts — for one-time
setup like fetching a token or building a connection profile. Each `before`
step is a normal step (`use`/`with`/`outputs`); its `outputs` name is exposed
to **every** test step under the `config` namespace. `variables:` holds static
values, exposed under `vars`.

```yaml
vus: 50
variables:
  region: eu-west
before:
  - uses: std/http@v1
    with:
      method: POST
      url: "https://api.example.com/token"
      body: { user: demo, region: "${{ vars.region }}" }
    outputs: auth            # ← exposed to test steps as config.auth
```

```yaml
# test.yaml
steps:
  - uses: std/http@v1
    with:
      url: "https://api.example.com/me"
      headers:
        authorization: "Bearer ${{ config.auth.body }}"   # from before step
        x-region: "${{ vars.region }}"                      # from variables
```

- `before` runs regardless of `--quiet` (its failures always print). If any
  setup step fails, the run **aborts before spawning VUs** — a broken setup
  would make every iteration fail identically.
- `before` steps see `${{ vars.* }}` and earlier setup outputs (under their own
  `outputs` name). Test steps see `config.*` and `vars.*` but not each other's.
- Interpolation always yields a **string**, so a numeric config value like
  `${{ config.fix_config.port }}` reaches the action as `"1111"`. Actions that
  take numbers accept the string form.

For the full lifecycle (including background processes started in `before:`),
see [Setup and teardown](core/setup-teardown.md).

### Teardown (`after:`)

`after:` steps run **once** after the load stops — on a normal finish, on a
failed run, on a failed `before:`, and on Ctrl-C/SIGTERM alike. They see the
same `${{ config.* }}` and `${{ vars.* }}` as test steps. Unlike `before:`, a
failing teardown step is logged but does not abort the remaining ones
(best-effort cleanup). The typical `after:` step is a `std/kill_process@v1`
for a server `before:` started:

```yaml
allow_process_actions: true

before:
  - name: web
    uses: std/child_process@v1
    with:
      command: python3
      args: ["-m", "http.server", "8080"]
      port: 8080
      waitUntil: { port_open: 8080, timeout: 10s }
    outputs: web

after:
  - name: stop web
    uses: std/kill_process@v1
    with: { name: web, signal: TERM }
```

Processes still alive after the `after:` steps are stopped automatically, so
the explicit kill is about a clean, timely stop rather than leak prevention.
See [`std/child_process@v1`](core/actions.md#stdchild_processv1) for the full
parameter list (restart policy, output capture, `waitUntil`),
[examples/with-processes.config.yaml](../examples/with-processes.config.yaml)
for a runnable setup, and [Setup and teardown](core/setup-teardown.md) for the
whole lifecycle (interrupts, auto-kill, data flow).

`after:` is also the home of run-level SLO gates: `std/thresholds@v1`
evaluates k6-style expressions (`p95<500`, `rate<0.05`, `count==0`) against
the metrics the whole run collected, once, and fails the run (non-zero exit)
when a `severity: fail` gate is violated:

```yaml
after:
  - name: slo gate
    use: std/thresholds@v1
    with:
      db_query_duration: ["p95<500", "max<2000"]
      db_query_failed: ["rate<0.05"]
      db_errors: ["count==0"]
    severity: fail            # fail (default) | warn | info
    message: "checkout SLO"   # optional, interpolated
```

See [`std/thresholds@v1`](core/actions.md#stdthresholdsv1) for the expression
grammar, metric kinds, and the `thresholds` field in the run summary JSON.

With `--locust`, the same config maps to locust's `--users`/`--spawn-rate`/`--run-time`.
With `--k6`, load config lives in the script's own `options` block and the
config file is ignored.

## Validating without running: `perfscale lint`

Check files ahead of time — in CI, pre-commit hooks, or while writing them:

```sh
perfscale lint test.yaml config.yaml
```

Beyond schema validation, `lint` flags unknown and typo'd field names with
did-you-mean suggestions (`chek` → `check`, `vsu` → `vus`, `std/htp@v1` →
`std/http@v1`), including per-action `with:` parameters. See
[CLI commands → lint](cli/commands.md#perfscale-lint).

## Validation errors

perfscale validates before running. Examples of what you'll see:

```text
error: schema validation failed:
  /steps/0 — every step must name an action: `use: std/http@v1` (or the `uses:` alias)
```

```text
error: invalid YAML: found unexpected end of stream
```

Regenerate the schemas after changing the Rust types:

```sh
cargo run -p perfscale-core --example gen_schema
```

(CI's `shipped_schemas_match_generated_ones` test fails if `schema/` goes stale.)
