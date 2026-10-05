# RFC 008: Pro step families — architecture, catalog, and registration contract

- **Status**: Draft
- **Author**: Perfscale Team
- **Created**: 2026-10-05
- **Requires**: the pro action seam (`crates/perfscale-core/src/step/actions.rs`),
  RFC 004 (setup/teardown — pro `*-config` actions run in config `before:`),
  RFC 005 (libraries — custom signaling for `pro/webrtc-*`)
- **Required by**: RFC 007 (pro/webrtc — the newest family, built on this seam)

## Summary

Define the **pro step family architecture** as it exists today and the
contract every future family must satisfy. A pro step family is a **closed
crate** living in the private controlplane repo (`controlplane/packages/*`)
that plugs into the OSS engine's runtime seams — process-global registries in
`perfscale-core` — via one `register()` call made by the paid agent
(`perfscaled`) at startup. The OSS repo knows nothing about any family beyond
optional config-block schemas gated with a clear "pro module required" error,
so the same YAML either runs (paid build) or fails loudly (OSS build).

This RFC catalogs the six families that exist, documents the four integration
seams they use (action registry, connection/driver registries, config gates,
BFF billing), records a concrete wiring gap found while writing it —
**`perfscale-fix` is shipped and documented but never registered in
`perfscaled`** — and defines the registration checklist plus a general
mechanism for tiered (base vs pro) limits *within* a family.

## Motivation

- The pro catalog grew organically (pubsub drivers → soap → llm-pro →
  shared-redis → webrtc → fix). Each family re-discovered the same four
  seams; nothing wrote them down. The fix gap (below) is the direct cost:
  a whole family shipped end-to-end — crate, docs on the site, workspace
  wiring in controlplane — except the one-line `register()` call in the
  agent, and nothing in CI caught it.
- Product now wants **tiered shapes inside one family** (e.g. webrtc
  publish with 1 audio + 1 video track in the base tier, multi-track
  pro-only). Without a shared mechanism every family will invent its own
  gating, and the BFF 402 story will drift.
- Adding the next family (database protocol steps are the obvious candidate)
  should be a checklist, not an archaeology dig through five existing crates.

## Non-goals

- No change to the OSS engine's step shape, pacing/VU model, or metrics
  model — pro families are *consumers* of those, never forks of them.
- No new billing system: tier gating reuses the existing 402 machinery
  (`controlplane/src/billing_gate.rs`) rather than adding a second gate.
- No per-family reference documentation here — each family keeps its own
  RFC/docs (RFC 007 for webrtc); this RFC is about the *family pattern*.
- `packages/database` in controlplane is **not** a pro step family — it is
  the controlplane BFF's own Postgres crate (sqlx pool, migrations,
  queries). It is mentioned only to keep it out of the catalog.

## Guide-level explanation

### What a pro step family is

A closed-source Rust crate in `controlplane/packages/<family>` that:

1. Implements one or more engine seams — most commonly
   `perfscale_core::step::actions::ActionHandler` for `pro/<family>*@v1`
   action ids, but also driver registries (pubsub, shared variables, GPU
   collectors, LLM observers).
2. Exposes a single `pub fn register()` that pushes its handlers/drivers
   into the engine's process-global registries.
3. Is consumed by the paid agent as a **git dependency**
   (`perfscaled/Cargo.toml` → `github.com/Perfscale/controlplane.git`); for
   local development the gitignored `perfscaled/.cargo/config.toml` patches
   each pro crate to the sibling checkout (`../controlplane/packages/*`), so
   pro changes are testable without a push. CI fetches via `GH_PAT`.
4. Is registered once in `perfscaled/src/pro.rs` (`pro::register()`, called
   from `main` before the HTTP server accepts runs).

The same YAML either works or fails honestly:

- On a **paid agent**, `pro/soap@v1` executes.
- On the **OSS engine**, `pro/soap@v1` fails with the engine's "unknown
  action" error, and a pro-only *config block* (e.g. `webrtc:`) fails at
  load time with a "requires the pro … module — this build does not include
  it" error.

### Catalog of existing families

| Crate (controlplane/packages) | Registered in perfscaled? | Provides | Seam used |
|---|---|---|---|
| `perfscale-pubsub` (`pubsub`) | ✅ | `redis` / `mqtt` / `kafka` drivers for `std/pubsub@v1` | `register_pubsub_driver` |
| `perfscale-soap` (`soap`) | ✅ | `pro/soap@v1`, `pro/soap-config@v1` | `ActionHandler` |
| `perfscale-webrtc` (`webrtc`) | ✅ | `pro/webrtc-connect@v1`, `-publish`, `-subscribe`, `-stats`, `-close`, `-call@v1` + unblocks the `webrtc:` config gate | `ActionHandler` + `Context::extensions()` connection registry |
| `perfscale-llm-pro` (`llm`) | ✅ | `pro_llm_*` metrics observers on `std/llm@v1` + `nvidia-smi-pro` GPU collector | `register_llm_observer` / `register_llm_metrics_observer` / `register_gpu_collector` |
| `perfscale-shared-redis` (`shared-redis`) | ✅ (conditionally) | `redis` driver for `std/set_shared_variable@v1` / `std/get_shared_variable@v1` — registered only when `PERFSCALE_SHARED_REDIS_URL` is set | `SharedVariableDriver` |
| `perfscale-fix` (`fix`) | ❌ **not registered** | `pro/fix@v1`, `pro/fix-config@v1` | `ActionHandler` (implemented, never called) |

### The four integration seams

1. **Action registry** (`perfscale/crates/perfscale-core/src/step/actions.rs`):
   the object-safe `ActionHandler` trait and `register_action()` /
   `action_registered()` over a process-global registry. Handlers are
   consulted in registration order; OSS built-ins and pro handlers share the
   one dispatch path, so `pro/*` steps get the same pacing, `check:`
   assertions, and metrics folding as `std/*` steps.
2. **Extension connection registries** (`Context::extensions()`,
   `crates/perfscale-core/src/step/context.rs:148`): how a family parks live
   connections across steps of one VU iteration — same insert/take/put_back
   semantics and iteration-end drain as the built-in ws/grpc/db families.
   Also the run-metrics accumulator for background samplers that outlive a
   single step (e.g. periodic webrtc getStats).
3. **Config gates** (`crates/perfscale-core/src/yaml.rs:216`,
   `require_webrtc_module`): the OSS schema carries the optional pro config
   block (`webrtc:`), and validation checks `action_registered(...)`; without
   the family the error names the missing module. The same check doubles as
   the registration probe — the config gate and the action registry can never
   disagree.
4. **BFF gating** (`controlplane` repo): save-time validation runs the
   document through `test_from_value_off_agent` /
   `config_from_value_off_agent` (`controlplane/src/testdef.rs:53`) so a
   pro step on an OSS-shaped validation path fails as **422** before anything
   is scheduled; the API-wide billing middleware
   (`controlplane/src/billing_gate.rs`, enabled with `BILLING_ENFORCE=true`)
   returns **402 PAYMENT_REQUIRED** for unpaid workspaces on non-exempt
   `/api/v1/` paths, and `openapi.rs`'s `BillingGateAddon` marks exactly the
   gated paths. Machine (M2M) tokens are out of scope of the 402 gate —
   agent traffic is governed by `machine_auth` alone.

### Registration checklist — adding a new family

Every step exists because some existing family needed it; skipping any is a
real failure mode (see the fix gap).

1. **Crate**: `controlplane/packages/<family>` — `perfscale-<family>`
   package name, `ActionHandler` (or driver) impls, one `pub fn register()`,
   crate-level docs with the YAML surface, own tests against the OSS engine
   as a dev-dependency.
2. **Workspace**: add to `members` in `controlplane/Cargo.toml`.
3. **Agent dep**: git dependency in `perfscaled/Cargo.toml` + path patch in
   `perfscaled/.cargo/config.toml` (local dev; CI uses GH_PAT).
4. **Agent registration**: call `<family>::register()` in
   `perfscaled/src/pro.rs`, extend the doc header and the
   `info!("Pro features registered: …")` line, and **add a wiring test**
   mirroring the existing ones (execute the action with deliberately invalid
   params; assert the failure is the family's own validation error, not the
   engine's "unknown action" / "unknown driver"). This test is what would
   have caught the fix gap.
5. **Config gate** (only if the family adds a top-level config block): OSS
   schema field + `require_<family>_module` check in
   `crates/perfscale-core/src/yaml.rs`, following `require_webrtc_module`.
6. **BFF Dockerfile**: `controlplane/docker/prod.Dockerfile` — copy the new
   package's `Cargo.toml` (and stub `src/`) so the workspace resolves during
   the dependency-cache layer.
7. **Billing/validation**: confirm the family's actions flow through
   save-time validation (422) and that the gated operations sit behind the
   402 middleware on enforced deployments; extend the OpenAPI gate list if a
   new endpoint is added.
8. **Docs**: pro-feature page in `site/content/{en,ru}/pro-features/` and the
   OSS docs noting the "pro module required" behavior; README of the crate
   itself.

### Tiered limits within a family (product requirement)

Product wants simple shapes usable in a base tier and advanced shapes
pro-only — the concrete case is `pro/webrtc-publish@v1`: today a connection
carries at most one track per kind (1 audio + 1 video); multi-track publish
(and SVC/simulcast `layers:`) should be pro-tier-only.

Mechanism — deliberately small, consistent with the existing 402 gating:

- **The family declares its limit shapes in YAML validation, not in new
  middleware.** Each pro family validates its own `with:` params already;
  tier validation is one more rule in the same place: the family exposes
  `validate(params, tier) -> Result<(), TierError>` where `TierError`
  renders as "shape X requires the pro tier of the `<family>` module".
- **The tier reaches the agent through the existing run dispatch.** The BFF
  already decides paid/unpaid (`tenant_paid`, the same decision the 402 gate
  caches); it stamps the run payload with the tenant tier, and the agent
  passes it into the context (a `Context` extension, alongside
  `extensions()`). An OSS engine simply reports tier `oss`, so OSS
  behavior is one special case of the same rule.
- **Save-time validation enforces the same rule with the base tier**, so a
  base-tier user gets a 422 at save with the same message the agent would
  produce — no failed runs to discover a config that was never legal.
  Upgrading to pro unlocks the shape with no document change.
- Limits are **declared as data per family** (a small table: shape → minimum
  tier), not hardcoded `if paid` branches, so the pricing page, the docs,
  and the validator read one source of truth.

Explicitly rejected as over-engineering: a generic limit-expression DSL, and
per-shape metering/billing — the platform's billing unit stays the tenant
subscription; tiered limits only *gate* shapes.

### Immediate action item: wire `perfscale-fix` into perfscaled

Found while cataloging: `perfscale-fix` is complete and actively maintained
in controlplane (`pro/fix@v1` + `pro/fix-config@v1`, schema/session/tls/wire
modules, its own run_native tests tracking engine API changes) and documented
on the site (`site/content/{en,ru}/pro-features/fix.mdx`) — but **no
`perfscaled` commit has ever referenced it**: `git log -S perfscale-fix` over
`perfscaled/Cargo.toml` is empty, the crate is absent from `Cargo.lock`, and
`src/pro.rs` never calls `perfscale_fix::register()`. There is no doc or
commit stating the exclusion is deliberate, so this is an **oversight**, not
a decision.

Fix (small, follows the checklist above): add the git dep + local path patch,
call `perfscale_fix::register()` in `pro::register()`, update the doc header
and registration log line, and add two wiring tests (`pro/fix@v1` and
`pro/fix-config@v1` resolve on the seam — failure must be the family's own
validation error, not "unknown action"). Until then, any test YAML using
`pro/fix*` fails on the paid agent with the engine's "unknown action" — the
paid build silently behaves like OSS for this family.

## Reference-level explanation

### The action seam

`perfscale/crates/perfscale-core/src/step/actions.rs:273` defines:

```rust
pub trait ActionHandler: Send + Sync {
    fn matches(&self, action_id: &str) -> bool;
    fn call<'a>(&'a self, action_id: &'a str, params: &'a Value,
                ctx: &'a Context, step_name: &'a str) -> ActionFuture<'a>;
}
```

`register_action(Arc<dyn ActionHandler>)` (actions.rs:295) appends to a
process-global `RwLock<Vec<Arc<dyn ActionHandler>>>`; `execute_action` walks
built-ins first, then registered handlers in registration order.
`action_registered(id)` (actions.rs:299) is the probe the config gates use.
A family handler conventionally matches both the canonical id
(`pro/fix@v1`) and the short alias (`fix`).

### The registration call site

`perfscaled/src/pro.rs` — `pro::register()` is invoked once from `main`
before the HTTP server accepts runs. Order does not matter (registries are
independent), but the file is the single audit point: its doc header lists
every family and its wiring tests (`mod tests`) assert each family actually
landed on its seam. `perfscale-shared-redis` is the one conditional
registration: `register(&url)` only runs when `PERFSCALE_SHARED_REDIS_URL`
is configured, so a paid agent without Redis still starts, and
`driver: redis` then fails with the engine's unknown-driver error listing
registered drivers — the same fail-loudly posture.

### Dependency wiring

- `perfscaled/Cargo.toml:31-35`: five pro crates as git deps on the private
  controlplane repo, `branch = "main"`; `Cargo.lock` is committed in
  git-deps form so `--locked` CI resolves.
- `perfscaled/.cargo/config.toml` (gitignored): `[patch]` section mapping
  each pro crate to `../controlplane/packages/*` for local development;
  the perfscale-core patch lives in the workspace-root `.cargo/config.toml`
  (cargo merges up the tree).
- `controlplane/Cargo.toml:2`: all seven packages are workspace members —
  including `packages/database`, which is the BFF's own Postgres access
  crate and **not** a pro step family (no `register()`, no engine dep).
- `controlplane/docker/prod.Dockerfile:32-45`: every pro package's
  `Cargo.toml` is copied into the dependency-cache layer so the workspace
  resolves; a new package must be added there.

### BFF validation and billing

- Save time: `controlplane/src/testdef.rs:53` runs the steps half through
  `perfscale_core::yaml::test_from_value_off_agent` and the config half
  through `config_from_value_off_agent` — "off agent" means validation runs
  without pro handlers registered, so unknown pro actions surface as 422 at
  save (unless the family is known to the validation schema).
- Run time / API-wide: `controlplane/src/billing_gate.rs` —
  `BILLING_ENFORCE=true` (paid-only contour) turns on middleware that
  resolves the caller's tenant and applies the `tenant_paid` decision
  (cached per user+tenant, invalidated on payment/plan change); unpaid →
  402 `PAYMENT_REQUIRED`. An exempt list keeps the pay-wall itself reachable
  (auth, provisioning, billing status, payments, subscription). M2M tokens
  (`machine_auth`) bypass the gate by design — agent traffic is governed by
  machine auth, not user billing.
- OpenAPI: `controlplane/src/openapi.rs:112` `BillingGateAddon` annotates
  the same gated paths so the 402 is part of the published contract.

### Tier plumbing (proposed)

The tier stamp rides the existing BFF→agent run-dispatch payload (the agent
already authenticates with M2M tokens and receives the run definition); the
agent stores it in the `Context` extension bag next to
`ExtensionRegistries`. Families read it in validation only — execution paths
stay tier-free, so a mid-run tier flip cannot corrupt a live connection.
`oss` is a tier value, not a separate code path: the OSS engine validates
every pro shape as above its tier, which collapses to today's behavior.

## Drawbacks

- **Two repos, one binary**: the agent's feature set depends on git deps on
  a private repo; a breaking engine change breaks the pro crates in a repo
  CI doesn't see until the next `perfscaled` lock bump (the `chore: bump pro
  deps` commits in perfscaled history are exactly this friction).
- **Process-global registries** are order-independent but also
  isolation-free: a buggy pro `register()` can wedge startup for every
  family, and tests must serialize registration (`Once`).
- **Tier stamping trusts the BFF**: a misconfigured contour that forgets to
  stamp silently downgrades paying tenants to base shapes (fail-closed, but
  still a support ticket). The agent cannot verify tier claims itself.
- The wiring tests assert *registration*, not behavior — they would not
  catch a family whose handler is registered but broken.

## Alternatives considered

- **Move the pro crates into the OSS repo behind cargo features** —
  rejected: the crates are the paid product; a `--features pro` build of a
  public repo makes the paywall a compile-time flag anyone can flip, and the
  private-repo boundary (license, roadmap, pricing experiments) is
  deliberate. The current split keeps the OSS engine genuinely usable while
  the seam keeps pro integration to one `register()` line.
- **Dynamic loading (cdylib plugins)** — rejected: ABI fragility across
  Rust versions, and the agent is a single static binary for air-gapped
  installs; plugins would add a distribution channel for zero benefit at
  five families.
- **Per-family licensing keys verified by the agent** — rejected for now:
  tier stamping via the trusted BFF is fail-closed and simpler; crypto
  license verification matters only if agents must run fully disconnected
  from a contour that makes paid decisions (revisit with RFC 006-style
  offline-posture work if that becomes a product requirement).
- **A generic limit DSL for tier rules** — rejected as over-engineering:
  the catalog has one concrete tiered shape today (multi-track publish);
  a data table per family plus the shared `validate(params, tier)`
  convention covers the known cases and stays grep-able.

## Resolved during review

- **`packages/database` is not a pro step family** — it is the controlplane
  BFF's Postgres crate; the catalog above excludes it explicitly.
- **`perfscale-fix` gap classification**: oversight, not a deliberate
  exclusion — no commit or doc states an intent, and the crate + site docs
  are maintained as if live. Fix as an immediate action item (registration
  + wiring tests), not a separate RFC.
- **Tier enforcement point**: family-side YAML validation at save (422) and
  at run (step error), reusing the BFF's paid decision — no new middleware,
  no per-shape metering.
- **OSS behavior under tiering**: `oss` is just the lowest tier value in the
  same validation rule, so the tier mechanism does not fork the OSS path.
