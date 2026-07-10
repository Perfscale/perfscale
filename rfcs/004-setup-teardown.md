# RFC 004: Setup and teardown

- **Status**: Draft
- **Author**: Perfscale Team
- **Created**: 2026-07-09
- **Requires**: none
- **Related**: RFC 003 (composite step) — setup/teardown are step lists too

## Summary

Two optional step lists that run **once per test run**, not once per virtual
user and not once per iteration: `setup` before any VU starts, `teardown`
after every VU has stopped. Setup prepares shared state (create a fixture,
mint a service token, seed a database) and hands its outputs to every VU as
read-only inputs; teardown cleans up (delete the fixture, revoke the token)
and is guaranteed to run even when the test fails or is interrupted.

## Motivation

- Load tests routinely need one-time work: create the account all VUs will
  hammer, obtain an admin token, warm a cache, snapshot a baseline. Today
  this is impossible in a native test — the only place to put steps is
  `steps:`, which runs *per VU per iteration*, so "create the account" would
  run thousands of times and race itself.
- The symmetric need is cleanup: a test that creates data must delete it, or
  repeated runs poison the target. There is nowhere to put that today.
- Both k6 (`setup()` / `teardown()`) and locust (`on_start` / `on_stop`,
  test-level events) have this; a native engine without it forces users back
  to the wrapped engines for a basic need.
- The execution model already has the exact seam: `run_steps` spawns N VU
  tasks and awaits them all. Setup is "before the spawn loop", teardown is
  "after the join" — the structure is already there.

## Goals

- `setup` runs once, before any VU; its declared outputs are visible to
  every VU (and to `teardown`) as read-only inputs.
- `teardown` runs once, after all VUs finish — **and** on failure and on
  interruption (Ctrl-C / duration elapsed / a VU panicking).
- Setup failure aborts the run before load starts (fail fast — no point
  hammering with a broken fixture).
- Setup/teardown work does **not** pollute the load metrics
  (`http_req_duration`, RPS): they are not load.

## Non-goals

- Per-VU setup/teardown (each VU initializing its own session). That is a
  distinct feature — call it `vu_setup` — and is out of scope; this RFC is
  strictly *once per run*.
- Per-iteration hooks (that is just the existing `steps:`).
- Parallelism within setup (setup steps run sequentially — it's a
  preparation script, not a load phase).

## Detailed design

### Shape

```yaml
setup:
  steps:
    - use: std/http@v1
      with:
        method: POST
        url: "${{ env.BASE_URL }}/admin/tokens"
        body: { scope: load-test }
      check: { status: 201 }
      outputs: admin
  outputs:                       # what setup exposes to VUs + teardown
    token: "${{ admin.body }}"
    account_id: "${{ admin.body }}"

steps:                           # per VU, per iteration — the load
  - use: std/http@v1
    with:
      url: "${{ env.BASE_URL }}/accounts/${{ setup.account_id }}"
      headers: { authorization: "Bearer ${{ setup.token }}" }

teardown:
  steps:
    - use: std/http@v1
      with:
        method: DELETE
        url: "${{ env.BASE_URL }}/admin/tokens/${{ setup.token }}"
```

`setup.*` is a new read-only variable root inside VU steps and teardown,
mirroring how `inputs.*` works inside a composite (RFC 003). Setup and
teardown are themselves step lists — the same execution machinery, run with
a VU count of one, no loop.

### Execution order and lifecycle

```
run:
  1. run setup steps once (sequential, single context)
     ├─ any step fails a check / errors → ABORT, run teardown? (see below), exit non-zero
     └─ collect declared setup.outputs
  2. spawn N VUs; each sees setup.* as read-only inputs; loop until deadline
  3. join all VUs (normal end, duration elapsed, or a VU dies)
  4. run teardown steps once, ALWAYS (see guarantee below)
  5. emit summary
```

### The teardown guarantee — the hard part

Teardown must run on:

- normal completion,
- setup succeeding but the load phase erroring,
- **interruption**: SIGINT/SIGTERM (Ctrl-C), which today just kills the
  process.

This requires a signal handler and running teardown during shutdown, with a
bounded teardown timeout (a teardown that hangs must not wedge shutdown
forever). It does **not** try to survive `SIGKILL` or a power loss — those
are unrecoverable and documented as such.

Setup-failure case: if setup fails, teardown runs only for the setup steps
that *did* complete and produced cleanup-relevant outputs — but that's
fragile (see pitfalls). Proposed default: on setup failure, **do not** run
teardown (nothing was created yet in the common case), and document that
setup should be idempotent/self-cleaning. This is a genuine open question.

### Metrics isolation

Setup/teardown `std/http` steps must **not** record into the run histogram.
The load metrics describe the load phase only; a setup POST that takes 2s to
create a fixture would otherwise wreck p99. Implementation: setup/teardown
run with metrics recording disabled (or into a separate, unreported
accumulator). This is a real behavioral contract, not a nicety — it's why
you can't just prepend setup to `steps:`.

### Scoping

- `setup` context: its own steps' outputs, plus `env.*` (environment). No
  VU context (VUs don't exist yet).
- VU `steps`: `setup.*` (read-only) + their own per-iteration outputs, as
  today. A VU cannot mutate `setup.*` — it's shared immutable state; making
  it mutable would reintroduce cross-VU races.
- `teardown`: `setup.*` (read-only) + its own steps' outputs. Teardown does
  **not** see per-VU state (there were N of them; which one?).

## Benefits

- **Closes a basic capability gap**: create-fixture / use / clean-up is the
  shape of most realistic load tests; native tests currently can't express
  it at all.
- **Correctness**: one-time work runs once instead of racing across VUs; the
  target isn't left polluted after a run.
- **Metrics honesty**: fixture-creation latency stays out of the load
  numbers — the reason "just prepend it to steps" is wrong.
- **Engine parity**: removes a reason users fall back to k6/locust for
  native-expressible tests.
- **Reuses RFC 003 machinery**: setup/teardown are step lists with a scoped
  context and declared outputs — the same primitives as composites.

## Drawbacks

- **Signal handling is new surface.** The engine currently has no graceful
  shutdown path; teardown-on-interrupt means a signal handler, a shutdown
  channel to VUs, and a teardown timeout — non-trivial and easy to get
  subtly wrong (double-fire, teardown running twice, teardown racing a
  still-draining VU).
- **The teardown guarantee is a promise you can partially break.** SIGKILL,
  OOM, and crashes skip teardown; users *will* assume "guaranteed cleanup"
  means always. The gap between the promise and reality is a support/trust
  liability that must be documented bluntly.
- **More top-level keys** (`setup`, `teardown`) and a new `setup.*` variable
  root — more schema, more docs, more lint.
- **Distributed runs break the "once" promise.** On a fleet (controlplane
  driving many agents), "once per run" becomes "once per agent" unless setup
  is centralized — see pitfall 5. The single-binary semantics don't
  automatically scale to the fleet, and that mismatch is easy to miss.

## Tradeoffs

- **Teardown-always vs. teardown-on-success-only**: always-run is what users
  expect and what makes cleanup trustworthy, but requires signal handling
  and raises "teardown ran but setup half-failed" edge cases. Success-only
  is trivial but useless for the main use case (cleanup after a failed run
  is exactly when you need it). Chosen: always-run, with a bounded timeout
  and honest docs about SIGKILL.
- **Shared immutable `setup.*` vs. per-VU copies**: immutable shared state
  is memory-cheap and race-free but VUs can't personalize it (e.g. each VU
  wanting its own sub-account). Per-VU setup is a *different* feature
  (`vu_setup`, non-goal). Chosen: immutable shared; point users needing
  per-VU init at the future `vu_setup`.
- **Separate keys vs. a "phase" field on steps**: tagging steps with
  `phase: setup|load|teardown` in one list is more uniform but muddles the
  common case and complicates "run the load N times". Chosen: separate
  top-level lists — the phases are genuinely different lifecycles.
- **Metrics: disabled vs. separately reported**: fully disabling setup
  metrics is simplest; reporting them under a separate name is more
  informative (how long did setup take?) but adds summary surface. Chosen:
  disabled for v1, separate-metric as a documented future.

## Non-obvious pitfalls

1. **Teardown-on-interrupt fights the current shutdown model.** VUs stop
   because `Instant::now() >= deadline`; there is no cancellation channel. A
   SIGINT today kills the process mid-iteration. Adding teardown means
   introducing cooperative cancellation (a shutdown flag VUs check) *and*
   ensuring teardown runs after VUs actually stop, not concurrently with
   still-in-flight requests. Get the ordering wrong and teardown deletes a
   fixture a lagging VU is still using.
2. **Teardown must be re-entrancy safe.** Duration-elapsed and SIGINT can
   race (test ends naturally the same instant the user hits Ctrl-C).
   Teardown running twice — deleting an already-deleted fixture — must be
   benign (idempotent) or guarded by a run-once latch. A latch is safer than
   trusting teardown authors to be idempotent.
3. **Setup output size × sharing.** `setup.*` is copied/visible to every VU.
   A setup step that outputs a 10MB response body, shared read-only across
   1000 VUs, is fine if it's an `Arc` but a memory bomb if each VU clones it.
   The `Context` currently stores `Value` by clone — setup outputs shared to
   VUs need `Arc`-backed sharing or a documented size caution (echoes the
   histogram/`std/file-read` memory lessons).
4. **Setup latency counts against wall clock but not duration.** A 30s test
   with 20s of setup takes 50s wall. Users will read "duration: 30s" and be
   surprised. And does the setup time count toward a `--summary-export`
   timestamp or the reported run window? Define which clock the summary
   reports.
5. **"Once per run" is a lie on a fleet.** The controlplane dispatches one
   task to many agents; each agent process runs setup → "create the account"
   runs once *per agent*, N accounts, races on the shared name. True
   once-per-run needs a coordinator (controlplane runs setup centrally, then
   dispatches setup.* to agents as inputs). This RFC's semantics are correct
   for a single process and *wrong* for a fleet unless that coordination is
   built — and the failure is silent (works in local testing, breaks at
   scale).
6. **Setup failure vs. teardown: the half-created state problem.** Setup
   creates A, then creating B fails. Was A created? Should teardown delete
   it? The engine can't know which setup outputs are cleanup-relevant. The
   proposed "don't run teardown on setup failure" is safe only if setup is
   idempotent/self-cleaning — which pushes correctness onto the user and
   must be loud in docs. There is no clean automatic answer here.
7. **Metrics-disabled setup still shares the connection pool.** Setup's
   `std/http` uses `shared_client()`; its connections warm the pool that VUs
   then use. Usually harmless (even helpful — warm keep-alive), but it means
   setup isn't fully isolated from the load phase, and a setup that opens
   many connections changes the load phase's starting conditions.
8. **`env.*` doesn't exist yet.** The examples use `${{ env.BASE_URL }}`;
   the engine has no environment-variable interpolation root today
   (interpolation resolves only stored step outputs). Setup is the natural
   place people will reach for env vars, so this RFC probably drags in an
   `env.*` root as a dependency — scope it explicitly or the examples don't
   run.

## Alternatives considered

- **Do it in `steps:` with a "first iteration only" guard.** No engine
  change, but there is no cross-VU "first iteration" — VU 1 iteration 1 and
  VU 2 iteration 1 race, and there's no shared latch. Fundamentally can't
  express once-per-run. Rejected.
- **A composite invoked once (RFC 003).** Composites still run inside
  `steps:` (per VU per iteration); "once" isn't expressible without the
  lifecycle hook this RFC adds. Composites and setup are complementary, not
  substitutes.
- **External orchestration (setup in a shell script / CI step before
  perfscale).** Works today and is the honest current answer. But it splits
  the test across files, can't share setup outputs into the test cleanly
  (must marshal via env/files), and can't clean up on interruption. Fine as
  a workaround, poor as the product answer.
- **Rely on k6/locust setup/teardown via the wrapped engines.** Available
  now, but only for users already writing k6/locust — defeats the native
  engine's reason to exist.

## Rollout plan

1. `env.*` interpolation root (pitfall 8) — small, unblocks realistic
   examples; arguably its own tiny RFC but needed here.
2. Setup: run a step list once before the VU spawn loop with metrics
   disabled; collect declared `outputs` into a shared `Arc`-backed
   `setup.*`; abort-on-failure.
3. VU context: inject read-only `setup.*`.
4. Teardown on the normal/error path (after VU join), with a run-once latch
   and bounded timeout.
5. Cooperative cancellation + SIGINT/SIGTERM handler so teardown runs on
   interrupt (the hard, separable increment).
6. Lint + docs (lifecycle diagram, the SIGKILL caveat, the fleet caveat),
   tests (order, metrics-isolation, teardown-on-failure, teardown-once
   under racing end+signal).
7. Fleet coordination (controlplane runs setup centrally) — separate RFC;
   until then, document "once per agent, not once per fleet".

## Open questions

- On setup failure, run teardown or not? (Leaning: no; require idempotent
  setup; revisit if real tests prove otherwise.)
- Should the summary report the load window only, or wall-clock including
  setup/teardown? (Leaning: load window for metrics, note setup/teardown
  durations separately.)
- Is `vu_setup` (per-VU init) close enough to fold in, or strictly a later
  RFC? (Leaning: later — the once-per-run and once-per-VU lifecycles have
  different context and metrics rules.)
- Does teardown get a separate, longer timeout than a normal step (cleanup
  can be slow), and is that configurable?

## Success metrics

- A create-fixture / load / delete-fixture test runs natively, leaves the
  target clean, and its fixture-creation latency is absent from
  `http_req_duration`.
- Ctrl-C during a run triggers teardown (fixture deleted) rather than a bare
  process kill, in testing.
- Teardown proven to run exactly once under a forced end+SIGINT race in
  tests.
