# Setup and teardown

A native run is more than the VU loop. The config file can bracket the load
with one-time steps: `before:` prepares the world (fetch a token, start a
local server), `after:` tears it down — on every exit path.

## Run lifecycle

```text
before steps (once, in order, fail-fast)
      │
      ▼
VU loop (steps × iterations until duration expires)
      │
      ▼
after steps (once, best-effort)          ← runs on EVERY exit path:
      │                                    normal finish, failed run,
      ▼                                    failed before, Ctrl-C/SIGTERM
auto-kill of remaining managed processes
      │
      ▼
summary
```

- `before:` steps run **once**, in order, before any VU is spawned. If any of
  them fails, the run aborts before spawning VUs (`Setup failed, aborting
  run: …`) — a broken setup would make every iteration fail identically.
- `after:` steps run **always**: after a normal finish, after a failed run,
  after a failed `before:` (a setup step may already have started something
  the teardown exists to clean up), and after Ctrl-C/SIGTERM. Unlike
  `before:`, a failing teardown step is logged
  (`teardown step '<name>' failed (continuing)`) and the remaining steps
  still run — partial cleanup beats none.
- After the teardown, every managed process still alive in the run's
  registry is **stopped automatically** (SIGTERM, escalated to SIGKILL after
  a grace period, whole process group). A forgotten `kill_process` never
  leaks a server.
- Besides cleanup, `after:` is where run-level SLO gates live: a
  `std/thresholds@v1` step sees everything the run collected and gates the
  exit code on it (see [actions.md](actions.md#stdthresholdsv1)).
- The metric summary prints last.

## Data flow: `vars.*` and `config.*`

Two config blocks feed the steps:

| Block | Exposed as | Visible to |
|---|---|---|
| `variables:` | `${{ vars.<key> }}` | `before` steps, test steps, `after` steps |
| `before:` step `outputs` | `${{ config.<name>.<field> }}` | test steps and `after` steps |

```yaml
variables:
  region: eu-west

before:
  - uses: std/http@v1
    with:
      url: "https://api.example.com/token?region=${{ vars.region }}"
    outputs: auth            # → ${{ config.auth.status }} etc. later on
```

- A `before` step sees `${{ vars.* }}` and the outputs of **earlier** setup
  steps (each under its own `outputs` name).
- Test steps see `config.*` and `vars.*` — but never each other's outputs
  across VUs (per-VU contexts are independent).
- `after` steps see the same `config.*` and `vars.*` as test steps did, so a
  teardown step can reference exactly what the test referenced.
- Interpolation always yields a **string**: a numeric output like
  `${{ config.keeper.port }}` reaches the action as `"8080"`. Actions that
  take numbers accept the string form.

## What does not work in `before:`

Live connections (`std/ws-connect@v1`, `std/grpc-connect@v1` and the stream
family) are scoped to the context that created them. The setup context is
gone before the first VU starts, so a connection opened in `before:` is dead
by the time a test step asks for its id — open connections per VU iteration
instead (they are drained at iteration end).

Processes are the exception: `std/child_process@v1` registers its child in a
**run-scoped** registry, which is exactly why a server started in `before:`
stays alive and killable for the whole run.

## Background processes end to end

The canonical setup/teardown pair: start a local service in `before:`, hit
it from the test, stop it in `after:`. Here a "position keeper" mock for a
CFD-trading scenario:

```yaml
# config.yaml
vus: 20
duration: 5m
allow_process_actions: true        # fail-closed gate, default false

before:
  - name: position-keeper
    uses: std/child_process@v1
    with:
      command: python3
      args: ["-m", "http.server", "8080"]
      port: 8080                   # echoed to outputs; port: 0 auto-assigns
      waitUntil:                   # block the step until the server is ready
        port_open: 8080
        timeout: 15s
      restart: on-failure          # supervisor restarts crashes (max 3, 1s apart)
    outputs: keeper                # → ${{ config.keeper.port }} etc.

after:
  - name: stop-keeper
    uses: std/kill_process@v1
    with:
      name: keeper                 # registry lookup — always the current pid
      signal: TERM
```

```yaml
# test.yaml
steps:
  - uses: std/http@v1
    with:
      url: "http://127.0.0.1:${{ config.keeper.port }}/positions"
    check:
      status: 200
```

What happens underneath:

- **Streaming**: the child's stdout/stderr stream into the run log with a
  `position-keeper: ` prefix (same shape as the k6 runner) and accumulate in
  bounded tail buffers (64 KiB per stream by default, `buffer_kb` to tune).
- **Supervision**: with `restart: on-failure` (or `always`), a supervisor
  task watches the child and respawns it after `backoff_ms` (default 1000),
  up to `max_restarts` (default 3) times, logging each restart.
- **Outputs are a snapshot**:
  `{ pid, ppid, pgid, port, stdout, stderr, restart_count }`. `ppid` is
  perfscale itself; `pgid` is the child's process group. After a restart the
  stored `pid` is stale — which is why `std/kill_process@v1` should address
  the process by `name` (the step name or the `outputs` name): the registry
  lookup always resolves the *current* pid. `pid:` (raw OS pid) exists as a
  best-effort escape hatch for processes outside the registry.
- **Auto-kill**: even without the `after:` step, the keeper would be stopped
  when the run ends. The explicit `kill_process` is about a clean, timely
  stop — and about being able to assert on it.

See [`std/child_process@v1`](actions.md#stdchild_processv1) and
[`std/kill_process@v1`](actions.md#stdkill_processv1) for the full parameter
references, and
[examples/with-processes.config.yaml](../examples/with-processes.config.yaml)
for a runnable variant.

## waitUntil — readiness gates

A `child_process` step blocks until the process is ready (or the gate times
out). Two forms:

```yaml
# Object form — all listed matchers must hold:
waitUntil:
  stdout_contains: "Serving HTTP"    # substring in captured stdout
  stderr_contains: "listening"       # substring in captured stderr
  stdout_matches: "on port \\d+"     # regex against captured stdout
  stderr_matches: "err\\d+"          # regex against captured stderr
  port_open: 8080                    # TCP connect to 127.0.0.1:<port> works;
                                     # 0 = the step's own `port`
  timeout: 15s                       # duration string, default 30s
  on_timeout: fail                   # fail (default) | continue

# String form — one matcher, defaults for the rest:
waitUntil: 'contains(stdout, "Serving HTTP")'
waitUntil: 'matches(stderr, "err\\d+")'
waitUntil: 'port_open(8080)'
```

- With `on_timeout: fail` an unmet gate fails the step (and therefore the
  run when used in `before:`); `continue` logs the miss and proceeds anyway.
- A process that **exits before becoming ready** fails the step immediately
  with its exit code and an stderr tail — no waiting out the timeout.

## Interrupts (SIGINT / SIGTERM)

Two-stage semantics:

- **First** SIGINT (Ctrl-C) or SIGTERM: the VU loop stops between steps, and
  the run proceeds through the normal teardown — `after:` steps, process
  auto-kill, summary. A log line announces it:
  `Interrupt received — stopping load, running teardown (interrupt again to
  force-quit)`.
- **Second** signal: immediate `exit(130)` — teardown itself may be wedged,
  and the operator clearly wants out. Anything not yet stopped by then may
  leak, so prefer waiting out the first stage.

## Safety and portability

- **Fail-closed gate**: both process actions fail with
  `process actions disabled (allow_process_actions is false)` unless the
  config opts in with `allow_process_actions: true` — a step list from an
  untrusted source cannot spawn or signal OS processes. The same pattern as
  `allow_file_actions` for the file actions.
- **Process groups (unix)**: every child leads its own process group
  (`pgid == pid`), so a `tree: true` kill signals exactly the child's group —
  it can never hit perfscale or its parent.
- **Non-unix**: there are no POSIX signals or process groups.
  `std/kill_process@v1` by `name` terminates the direct child only (`tree`
  has no effect); `pid:` is unsupported. Everything else — capture, restart
  supervision, `waitUntil`, auto-kill — works the same.
