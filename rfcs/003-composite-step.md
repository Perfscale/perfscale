# RFC 003: Composite step

- **Status**: Draft
- **Author**: Perfscale Team
- **Created**: 2026-07-09
- **Requires**: none (enables RFC 002)
- **Required by**: RFC 002 (Marketplace) — composition actions build on this

## Summary

A composite step groups several steps into one reusable unit. Instead of
copy-pasting the same login → fetch-token → set-header sequence into every
test, a test defines it once as a named composite and invokes it like any
other step: `use: composite` (inline) or, later, `use: acme/login@v1` (a
published composite — RFC 002). This RFC specifies the local, in-file
composite; the marketplace-distributed composite is the same mechanism with
a resolver in front.

## Motivation

- Real tests repeat sequences: authenticate, then N authenticated requests;
  create a resource, use it, delete it; poll until ready. Today each step is
  flat in `steps:`, so these sequences are copy-pasted, and a change means
  editing every copy.
- RFC 002 chose "composition actions" as the first, sandbox-free marketplace
  mechanism — "an action is a parameterized bundle of built-in steps". That
  bundle *is* a composite step. RFC 002 cannot ship its safe first phase
  until composites exist; this RFC is the prerequisite it referenced.
- The step model is deliberately flat (`TestDef { steps: Vec<Step> }`, each
  `Step` names one action). Nesting is the natural next expressivity step,
  and the interpolation/`outputs` machinery to make it useful already
  exists.

## Goals

- Define a reusable, parameterized group of steps in one place, invoke it in
  many.
- Parameters in, outputs out: a composite takes inputs and exposes a chosen
  result, so callers treat it as a black box.
- Composites compose (a composite may invoke another) with a bounded depth.
- Zero new execution surface — a composite expands into the existing step
  engine; no sandbox, no new action ABI. (This is exactly why RFC 002 picked
  it as phase one.)

## Non-goals

- Control flow (branching, loops, retries) inside composites — RFC 003 is
  *sequencing and reuse* only. Conditionals are a separate, harder RFC.
- Cross-VU or cross-iteration state. A composite runs within one VU
  iteration, like any step sequence today.
- Distribution/versioning/signing — that is RFC 002. This RFC is the local
  mechanism only.

## Detailed design

### Shape

Two ways a composite enters a test:

**Inline definition + reference** (top-level `composites:` map, invoked by
name):

```yaml
composites:
  login:
    inputs: [base_url, user, password]        # declared parameters
    steps:
      - use: std/http@v1
        with:
          method: POST
          url: "${{ inputs.base_url }}/login"
          body: { user: "${{ inputs.user }}", password: "${{ inputs.password }}" }
        check: { status: 200 }
        outputs: auth
      - use: std/log@v1
        with: { message: "logged in as ${{ inputs.user }}" }
    outputs: "${{ auth.body }}"               # what the composite returns

steps:
  - use: composite
    with:
      name: login
      inputs: { base_url: "https://api.example.com", user: demo, password: demo }
    outputs: token                            # composite result → `token`

  - use: std/http@v1
    with:
      url: "https://api.example.com/me"
      headers: { authorization: "Bearer ${{ token }}" }
```

A published composite (RFC 002) replaces `name: login` with a versioned ID
`use: acme/login@v1` — identical semantics, a resolver supplies the
definition.

### Scoping — the core design decision

A composite runs in a **child context**, not the caller's context. Inside:

- Only `${{ inputs.* }}` and the composite's own step outputs are visible.
- The caller's variables are **not** visible (no accidental capture).
- The caller sees **only** the composite's declared `outputs`, bound to the
  step's `outputs:` name.

This is the opposite of a macro/textual-inline, and it's deliberate: it
makes a composite a black box with a contract, so a composite authored
elsewhere (marketplace) can't read or clobber caller state, and a caller
can't depend on a composite's internal step names. `__last__` inside a
composite refers to the composite's last step; after the composite returns,
the caller's `__last__` is the composite's result.

### Execution

Expansion, not interpretation: at load time (or first use) a composite
invocation is resolved to its step list; at run time the engine executes
those steps in a child `Context` seeded with `inputs`, then extracts
`outputs` into the caller. No new hot-path action type — `execute_step`
gains a "this step is a composite" arm that recurses with a fresh context.

Metrics: a composite's inner `std/http` steps record into the run's
histogram exactly as if they were flat. A composite is a *source-level*
grouping, invisible to `http_req_duration`. (Optional future: per-composite
timing as a separate metric — explicitly out of scope here.)

### Recursion bound

Composites may invoke composites. A fixed depth limit (proposed: 8) with a
clear error prevents infinite expansion (`a` uses `b` uses `a`). Detected at
resolution time, not at run time under load.

## Benefits

- **DRY tests**: the login/CRUD/poll patterns live once. This is the
  single most-requested ergonomic gap in flat step lists.
- **Unblocks RFC 002 phase 1**: the safe, sandbox-free marketplace mechanism
  becomes buildable — composites are the artifact a "composition action"
  ships.
- **Black-box contract**: child-context scoping means composites are safe to
  share — no state capture, no reliance on internals — which is precisely
  the property a marketplace needs.
- **No new execution/security surface**: expands to existing steps; the
  engine's correctness and perf model are unchanged.
- **Readable tests**: `use: composite / name: checkout` documents intent
  better than fifteen inline steps.

## Drawbacks

- **The step model stops being flat.** Every tool that walks `steps:` (lint,
  the controlplane test editor's k6→steps preview, docs examples) now has a
  nesting case. The editor's parser especially will need work.
- **Error messages get harder.** "check failed at /steps/3" becomes "at
  /composites/login/steps/0, invoked from /steps/1" — locating failures
  across expansion is a real UX cost.
- **A second namespace** (`composites:`) in the test file, plus `inputs` as a
  reserved variable root. More to learn, more to document.
- **Debugging by expansion**: users will want to see the expanded step list;
  without a `perfscale expand` command, a misbehaving composite is opaque.

## Tradeoffs

- **Child context vs. shared context**: shared context (composite sees and
  writes caller vars) is simpler to implement and occasionally convenient,
  but destroys the black-box property and makes marketplace composites
  unsafe to share (silent variable capture/collision). Chosen: child
  context, despite the extra plumbing — it's the property that makes RFC 002
  viable.
- **Expansion vs. runtime interpretation**: expanding to a flat step list is
  simple and keeps the hot path unchanged, but makes recursion/params static
  (no runtime-computed composite name). Runtime interpretation allows
  dynamic dispatch but adds an execution mode. Chosen: expansion — dynamic
  composite selection is a control-flow feature, explicitly out of scope.
- **Inline `composites:` vs. separate files**: inline keeps a test
  self-contained (one file to run); separate files enable reuse across tests
  but reintroduce a resolution/pathing problem locally. Chosen: inline for
  this RFC; cross-file reuse is what RFC 002's registry provides.
- **Declared `inputs` vs. implicit capture**: declaring inputs is more
  verbose but is the contract that makes a composite a black box and enables
  lint to catch a missing/typo'd input. Chosen: declared inputs.

## Non-obvious pitfalls

1. **`outputs` is currently a name, not an expression.** Today `outputs:
   auth` stores the whole step value under `auth`. A composite's `outputs:
   "${{ auth.body }}"` is a *new* expression form — the field is overloaded
   (a bare name at step level, an interpolated expression at composite
   level). Either unify them (interpolate step `outputs` too) or use a
   distinct key (`returns:`) to avoid a confusing dual meaning. Leaning:
   distinct key.
2. **Interpolation cost regression.** RFC-era work added a fast path that
   skips interpolation for placeholder-free params. Composites are
   placeholder-dense by nature (`${{ inputs.* }}` everywhere), so every
   composite step takes the slow clone path. At high VU this is real; the
   composite's steps should be interpolated *once per invocation against the
   child context*, not re-scanned per inner step, or the fast-path win is
   quietly lost inside composites.
3. **Recursion detection must be static and pre-load.** A cyclic composite
   discovered at run time under 1000 VUs is 1000 blown stacks. Cycle
   detection belongs in lint and in load-time resolution, with the depth cap
   as a backstop, not the primary guard.
4. **`__last__` semantics across the boundary are a footgun.** Inside the
   composite, `__last__` is the composite's last step; the caller's inline
   `check:` after a composite step must see the composite's *declared
   result*, not its last internal step's output. Getting this wrong makes
   post-composite checks assert against the wrong thing silently.
5. **The k6→steps editor preview (controlplane) will silently drop
   composites.** It parses flat http/check/sleep. A composite in a test
   shows as nothing in the preview unless taught to expand — a "my test
   looks empty in the UI" bug waiting to happen. RFC 002/003 must budget the
   editor change.
6. **Schema evolution interaction (see RFC 001 pitfall 4).** An engine that
   predates composites, given a test with `composites:` + `use: composite`,
   would hit "unknown action 'composite'" — which fails loudly (good) —
   *but* would also ignore the top-level `composites:` key (lint warns, does
   not error). The `schemaVersion` gate from RFC 001 is what makes this a
   clean rejection instead of a confusing partial run.
7. **Input defaulting and missing inputs.** A composite invoked without a
   declared input: does it error, or does the missing `${{ inputs.x }}`
   resolve to empty string (current interpolation behavior)? Empty-string
   silent-default inside a shared composite produces a wrong request that
   looks fine — inputs should be validated at resolution, not left to
   interpolation's lenient miss.
8. **Metrics attribution invisibility.** Because inner steps record as flat
   http samples, a slow composite is undiagnosable from the summary — you
   see elevated p95 but not *which composite*. Fine for v1, but users will
   ask; the "per-composite timing" future is load-bearing for real
   debugging.

## Alternatives considered

- **No composites; rely on the SDK (RFC 001) to generate repeated steps.**
  Programmatic tests get DRY-ness for free via functions. Real, but leaves
  YAML-only users (the majority of quick tests) with copy-paste, and gives
  RFC 002 nothing to distribute. Composites serve both audiences.
- **Textual include/anchor (YAML `<<` merge or `!include`)**: reuse via YAML
  features. Zero engine change, but merges are value-level not
  sequence-level, have no parameters, no scoping, and no output contract —
  the exact properties composites need. Rejected.
- **Full sub-test invocation (a step runs another `TestDef`)**: heavier,
  reuses the file/config model, but conflates "load shape" (config) with
  "sequence" (steps) and drags in VU/duration semantics that make no sense
  nested. Composites are steps-only on purpose.
- **Macro/textual inline (caller-context expansion)**: simplest, but see the
  child-context tradeoff — it forecloses safe sharing. Rejected for the same
  reason.

## Rollout plan

1. `returns:`/output-expression decision (pitfall 1) and child-context
   `Context` scoping in the engine.
2. `execute_step` composite arm: seed child context from `inputs`, run inner
   steps, extract declared output. Depth cap + cycle detection.
3. Lint: validate `composites:` (declared inputs used, no cycles, invoked
   names exist), and did-you-mean for composite names.
4. `perfscale expand test.yaml` — print the flattened step list, for
   debugging opacity (drawback 4).
5. Docs + tests (interpolation-in-child-context, scoping isolation, cycle
   rejection, output contract).
6. **Then** RFC 002 phase 1 layers version resolution over the same
   mechanism.

## Open questions

- `returns:` (distinct key) vs. interpolated `outputs:` — which reads better
  and breaks less? (Leaning `returns:`.)
- Can a composite declare its own `check:` on its result, or does the caller
  check the composite's output? (Leaning: caller checks; keeps composites
  pure sequences.)
- Should `perfscale expand` be a subcommand or a `--expand` flag on `run`
  (dry-run the expansion)?
- Depth cap value — 8 is a guess; what's a realistic legitimate nesting?

## Success metrics

- The login/auth pattern in existing example tests collapses to one
  composite invocation.
- RFC 002 phase 1 ships a composition action that is literally a published
  composite, with no new execution mechanism.
- Lint catches a cyclic/typo'd composite before run time in testing.
