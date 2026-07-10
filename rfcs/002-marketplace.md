# RFC 002: perfscale Marketplace

- **Status**: Draft
- **Author**: Perfscale Team
- **Created**: 2026-07-09
- **Requires**: RFC 001 (SDK)
- **Required by**: none

## Summary

A marketplace for shareable, versioned perfscale **actions** (and, later,
whole test templates and dashboards). Today the engine ships four built-in
actions (`std/http`, `std/check`, `std/sleep`, `std/log`) plus `std/file-*`;
the action IDs already carry a namespace and version (`std/http@v1`) that
nobody but `std/` uses yet. The marketplace turns that latent namespace into
a distribution channel: `use: acme/grpc-call@v2` resolves to a published,
sandboxed action. This RFC covers the distribution and trust model; it does
**not** yet commit to an execution mechanism, because that choice (see
Tradeoffs) is the whole game.

## Motivation

- The `@v1` in every action ID is a promise the product hasn't kept — it
  implies an open action namespace but only `std/` exists.
- Real load tests need protocol and helper actions perfscale will never ship
  itself: gRPC, GraphQL, SQL, AWS SigV4 signing, JWT minting, Faker-style
  data generation. Every one of these is a "why doesn't perfscale support
  X" today.
- `perfscaled` already carries an `ActionRegistry` stub (currently empty) —
  the seam for third-party actions was anticipated in the architecture.
- A marketplace is a growth and (eventually) revenue surface: the same
  reason k6 has extensions and GitHub has Actions.

## Goals

- Publish, discover, version, and consume third-party actions by ID.
- A trust model: signed publishers, pinned versions, integrity-checked
  artifacts (the CLI already does sha256 verification for its own binary —
  reuse that discipline).
- Actions usable identically from YAML (`use: ns/name@vN`) and the SDK.
- Local development story: author an action, test it, publish it.

## Non-goals (this RFC)

- Paid/monetized listings. Design must not *preclude* it, but pricing,
  payouts, and tax are a separate effort.
- Sharing full test suites and dashboards. Natural follow-ons; deferred to
  keep the trust/execution problem tractable first.
- A GUI marketplace. CLI/SDK resolution first; a browsable web catalog on
  the site later.

## Detailed design

### Action identity and resolution

Extend the existing ID grammar `namespace/name@version`:

- `std/*` — built-in, in-tree, always available.
- `<publisher>/<name>@<major>` — marketplace action; resolves to a specific
  immutable artifact via a lockfile (`perfscale.lock`) pinning the exact
  version + digest, exactly like Cargo/npm.

Resolution order at load time: built-ins → lockfile → registry fetch (only
with `perfscale install`, never implicitly at run time — see Pitfalls).

### The registry

A thin index (artifact metadata + digests + signatures) plus artifact
storage. **This maps cleanly onto infrastructure that already exists**:
MinIO is in the prod stack and already serves the agent binary with
anonymous download + sha256 verification. The registry index can be a
served JSON document; artifacts are MinIO objects keyed by
`marketplace/<publisher>/<name>/<version>`.

### Execution model (the hard part — options, not a decision)

An action is "given resolved params + context, produce an ActionOutput".
Built-ins are Rust functions in `execute_action`. A third-party action needs
a boundary. Candidates, cheapest-to-safest:

1. **Declarative composition** — an action is a *parameterized bundle of
   existing built-in steps* (a macro). `acme/login@v1` expands to an http +
   check + outputs sequence. Zero new execution surface, zero sandbox
   problem, no arbitrary code. Covers a surprising fraction of real cases
   (auth flows, common request shapes). **Ships first.**
2. **WASM component actions** — action compiled to a WASM component;
   engine runs it in a wasmtime sandbox with a narrow host API (do one HTTP
   call, read a cached file, return output). Deterministic resource limits,
   language-agnostic authoring, real sandbox. The credible answer for
   *code* actions.
3. **Subprocess/native plugins** — dlopen or exec a binary. Maximal power,
   no meaningful sandbox, per-platform build matrix. Rejected for untrusted
   code; possible for first-party-only "verified" actions.

This RFC commits to **(1) now, (2) as the code-action target**, and
explicitly rejects (3) for third-party code.

### Trust and safety

- **Signing**: publishers sign artifacts; the CLI verifies signature +
  sha256 before caching (reuse the self-update verification path).
- **Pinning**: `perfscale.lock` pins version + digest; CI installs are
  reproducible; a re-published version with a different digest is a hard
  error, not a silent swap.
- **Capabilities**: WASM actions declare needed host capabilities
  (`net`, `read-file`); the engine enforces them. A "data generator" action
  requesting `net` is a visible red flag.
- **Namespacing**: `std/` is reserved; publisher namespaces are owned and
  verified (email/domain), preventing `std`-squatting and typo-domains.

## Benefits

- Closes the "perfscale doesn't support protocol X" gap without bloating the
  core engine — protocols become community/opt-in, core stays small.
- The `@vN` namespace finally means something; existing design debt becomes
  a feature.
- Reuses infra that exists (MinIO + sha256 + signing discipline), so the
  registry MVP is small.
- Composition actions (option 1) deliver value before any sandbox exists —
  ship early, learn, then invest in WASM.
- Ecosystem/growth flywheel; optional future revenue surface.

## Drawbacks

- **Sandboxing untrusted code is a security commitment that does not end.**
  Even WASM has escape/DoS history; a marketplace makes perfscale a supply
  chain, and supply chains get attacked. This is a permanent operational
  burden, not a feature you finish.
- **Curation cost**: spam, malicious, and abandoned actions need moderation
  and a deprecation/yank flow. That's people-time, ongoing.
- **Fragmentation**: five competing `*/grpc` actions of varying quality is a
  worse user experience than one official one. Marketplaces trend toward
  this.
- **Version-resolution complexity** leaks into the once-simple `use:` field:
  lockfiles, ranges, conflicts. The engine's YAML stops being fully
  self-contained.

## Tradeoffs

- **Composition-first vs. WASM-first**: composition ships in weeks with no
  sandbox risk but can't express new *behavior* (no gRPC via composition).
  WASM expresses anything but is a quarter+ of runtime and security work.
  Chosen: composition now, WASM when a concrete code-action need justifies
  the sandbox investment — don't build wasmtime integration speculatively.
- **Registry: build vs. reuse**: a real package registry (auth, search,
  yank, audit) is a product on its own. Reusing MinIO + a static index gets
  an MVP cheaply but lacks search/social features. Chosen: static index MVP,
  revisit when listing count justifies a real service.
- **Implicit vs. explicit fetch**: auto-fetching an unknown action at run
  time is convenient and a remote-code-execution footgun. Chosen: explicit
  `perfscale install` + lockfile; run time is offline against the cache.
- **Open vs. verified-only publishing**: open maximizes ecosystem and
  minimizes trust; verified-only inverts it. Chosen: open publishing with
  *verified-publisher* badges and capability disclosure, so trust is
  visible rather than gatekept.

## Non-obvious pitfalls

1. **A load-testing tool running third-party code is a weaponization risk.**
   The engine's entire job is "send many requests fast". A malicious action
   turns every perfscale user's agent fleet into a botnet node. Capability
   enforcement must gate *outbound targets*, not just "net: yes/no" — and
   that's genuinely hard. This is the single scariest item in either RFC.
2. **Determinism and metrics integrity.** A third-party action that does its
   own I/O outside the engine's timing path pollutes `http_req_duration`
   (or hides from it entirely). The host API must funnel all measurable work
   through the engine's instrumentation, or marketplace actions produce
   untrustworthy load-test numbers — defeating the product's purpose.
3. **Actions run per-iteration, per-VU, under load.** A marketplace action
   with a per-call allocation or a lock is invisible at 1 VU and fatal at
   1000. The just-added `has_placeholder` fast path and HDR histogram exist
   precisely because per-iteration cost compounds. Marketplace actions need
   a *performance contract* and ideally a bench gate at publish time.
4. **`std/` is not as reserved as it looks.** The engine matches action IDs
   by string in `execute_action`; nothing stops a lockfile from mapping
   `std/http@v1` to a malicious artifact unless built-ins are resolved
   *first and unconditionally*. Resolution order is a security boundary, not
   a convenience.
5. **The agent is long-lived and multi-tenant-adjacent.** perfscaled runs on
   shared machines across a fleet; a cached marketplace action persists
   across tasks and (in the controlplane model) across tenants. Cache
   poisoning or a stale/yanked action affects everyone on that agent until
   restart. Cache keying and yank-propagation need design.
6. **Yank/deprecation vs. reproducibility conflict.** Pinning by digest
   means a yanked-for-security action *still runs* from cache/lock. You
   cannot both guarantee reproducible pins and guarantee "the CVE'd action
   never runs again". Pick the failure mode consciously (we lean: warn
   loudly, let CI break, don't silently swap).
7. **WASM can't hold a connection pool.** The engine's whole performance
   model is keep-alive across iterations (`shared_client`). A per-call WASM
   sandbox that opens a fresh connection every iteration would be
   dramatically slower than a built-in — the sandbox boundary fights the
   perf model. Host-side connection pooling exposed to WASM is the fix and
   it's non-trivial.
8. **Licensing and liability.** Hosting third-party artifacts makes
   perfscale a distributor; licenses, DMCA, and "your marketplace action
   took down my prod" liability are real once money or scale appears.

## Alternatives considered

- **No marketplace; grow `std/` instead.** Every protocol becomes a core
  dependency; the engine bloats, build times grow, and niche protocols never
  clear the bar. Rejected — but it's the right answer *until* composition
  actions prove demand.
- **Git-based actions (like Go modules / GitHub Actions `uses:`).** No
  registry to run; `use: github.com/acme/grpc@sha`. Cheap, but no signing/
  capability story, no offline story, and pulls arbitrary repos at resolve
  time. Viable as an MVP behind explicit install; weaker trust model.
- **k6-extension-style: recompile the engine with chosen extensions.**
  What k6 does (xk6). Maximal performance, zero runtime sandbox problem, but
  every user needs a toolchain and a custom binary — kills the "one static
  binary" property that makes perfscale easy to deploy. Rejected for the
  general case; possible for self-hosted power users.

## Rollout plan

1. **Composition actions** (option 1): action = named bundle of built-in
   steps with parameters. No registry yet — ship the mechanism, dogfood by
   moving common patterns (auth flows) into shareable bundles in-repo.
2. **Static registry MVP** on MinIO: publish/install/lockfile, sha256 +
   signature verification, verified-publisher namespaces. Composition
   actions only — no code execution, so the sandbox problem is deferred.
3. **Capability model + WASM runtime** when a concrete code-action need
   (gRPC is the likely first) justifies the security investment. This is a
   quarter-scale effort and should have its own RFC.
4. **Web catalog** on the site once listings exist to browse.

## Open questions

- Does an action version to the engine version, the schema version, or
  independently? (A `std/http@v2` breaking change vs. a marketplace action's
  own semver are different clocks.)
- Composition actions: how much logic before they need real code? Can they
  branch/loop, or are they strictly linear step bundles? (Linear first.)
- Monetization shape if/when it comes — per-seat, per-listing, revenue
  share — changes the trust and identity requirements retroactively; worth a
  stance even while out of scope.
- Do marketplace actions run on the agent, in the CLI, or both? The agent is
  the scary multi-tenant case (pitfall 5); CLI-only would be a safer v1.

## Success metrics

- Three first-party composition actions replace copy-pasted patterns in
  real tests.
- One external publisher ships a verified action used by someone else.
- Zero marketplace-attributed security incidents (the metric that matters
  most and is only ever "so far so good").
