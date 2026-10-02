# RFC 006: Library revocation — a signed revocation list for offline-capable agents

- **Status**: Draft
- **Author**: Perfscale Team
- **Created**: 2026-10-03
- **Requires**: RFC 005 (libraries; distribution shipped in v0.22.0), inherits the trust stance of RFC 002 ("warn loudly, never silently swap")
- **Required by**: none (RFC 002's marketplace will consume the same list format)

## Summary

Define how a **yank** (author withdrew a version) or a **revocation**
(security: a library must not run again) reaches a perfscale user — the CLI,
and especially the offline-capable perfscaled agent whose library cache may
sit on a fleet machine that has not talked to anyone in weeks. The answer is
a **signed, append-only revocation list**: a small JSON document keyed by
artifact digest, signed with Ed25519, hosted at a well-known URL, mirrored
and pushed by the controlplane, and cached locally by every consumer.
Enforcement is **fail-open with loud warnings when the cached list is
stale**, **hard-fail when it is fresh**, and always explicit — a revoked
digest is never silently swapped for another artifact, and historical run
reproducibility is never broken retroactively.

This RFC resolves the RFC 005 open question "the *channel* for revocation
notices on an offline-capable agent is undesigned".

## Motivation

- Third-party WASM libraries (RFC 005) make perfscale a supply chain. The
  first compromised or malicious library is a matter of when, not if — and
  RFC 002 already names a load-testing tool running third-party code as a
  weaponization vector (pitfall 1). A revocation mechanism designed *after*
  the first incident arrives too late and gets retrofitted onto an installed
  base, which is the worst way to build a security control.
- The current design makes the problem concrete: artifacts are
  content-addressed (`<cache>/libraries/<sha256>.wasm`), installs are
  explicit and offline thereafter, and the agent *never fetches at run
  time*. That is exactly right for integrity — and exactly wrong for
  revocation: nothing in the system can ever learn "digest X is bad" after
  install. The offline discipline that protects users also blinds them.
- `sha256` is now optional for installs (agent + CLI), so digests get
  computed from whatever the source serves. A re-published malicious
  artifact under a *new* digest is caught by pinning; a *known* digest that
  turns out to be malicious has no signaling path at all.
- RFC 002's pitfall 6 names the inherent conflict: digest pinning means a
  yanked-for-security artifact still runs from cache. The stance there —
  "warn loudly, let CI break, don't silently swap" — is kept; this RFC
  designs the missing channel so the warning can actually happen.

## Goals

- A revocation list format that is small, append-only, signed, verifiable
  offline, and keyed by artifact digest (the same key the cache, the
  lockfile, and the agent's digest allowlist already use).
- A delivery channel that works for three consumers with very different
  connectivity: the OSS CLI (online at install time, offline at run time),
  the perfscaled agent (arbitrarily long offline stretches), and the
  controlplane fleet view (always online).
- Explicit, distinct semantics for **yank** (authorial, advisory) and
  **revoke** (security, enforced).
- Degrade honestly: an agent offline for weeks must not brick its fleet the
  moment someone revokes a digest, and must not silently run known-bad code
  either. Staleness is visible, policy-tunable, and never silent.
- Zero changes to historical behavior: a run that executed last month stays
  reproducible bit-for-bit; revocation gates *future* runs and *new*
  installs, never rewrites the past.

## Non-goals (this RFC)

- **Artifact distribution changes.** The list carries digests and metadata,
  never replacement artifacts. No auto-upgrade, no "revoked → use digest Y
  instead" indirection — that is a silent-swap vector and is rejected on
  principle (RFC 002 stance).
- **A marketplace/registry service.** The list format is designed so RFC
  002's future registry adopts it wholesale, but this RFC stands up no
  registry.
- **Revoking `@std/*` built-ins.** Built-ins live inside the engine binary;
  their "revocation" is a security release and a changelog entry. The list
  may *name* a built-in in a notice for documentation purposes, but
  enforcement of built-ins is out of scope.
- **Per-tenant revocation lists.** v1 has one global list. Tenant-scoped
  lists (a company banning a digest fleet-wide) are a natural extension via
  the controlplane and are noted under Open questions, not designed here.

## Detailed design

### Threat model

| Threat | Mitigation |
|---|---|
| A cached library is discovered to be malicious (exfiltrates data, mines, attacks the SUT beyond the test plan) | Revocation entry keyed by digest; agents refuse future runs once they learn of it |
| An author withdraws a version (broken, relicensed, superseded) | Yank entry; advisory — warn loudly, still runs |
| An attacker injects fake revocations to DoS a fleet ("revoke everything") | List is Ed25519-signed; unsigned or badly-signed lists are rejected and the last good cached list is kept |
| An attacker *withholds* the list (MITM blocks updates, controlplane down) | Fail-open with loud staleness warnings; fleet policy can escalate to fail-closed. Withholding cannot *forge* a state — an attacker can at most keep a victim on an older genuine list |
| Signing key compromise | Two active keys; rotation signed by the retiring key; emergency rotation out-of-band via a release (see Key management) |
| A revoked digest quietly re-enters via a new install | `perfscale install` and `perfscaled library install` check the list before caching |
| Reproducibility theatre: "the CVE'd library never runs again" promised but undeliverable offline | Explicitly not promised. The guarantee is: *once an agent has received the notice, the digest never runs again without an operator override* |

Two things are deliberately **not** defended against: an attacker who
already controls the agent host (they can edit the cache, the list, and the
binary — out of scope), and retroactively proving past runs used only clean
code (impossible by construction).

### The revocation list format

One JSON document, served from a well-known URL (see Hosting):

```json
{
  "version": 1,
  "sequence": 42,
  "issued_at": "2026-10-03T12:00:00Z",
  "entries": [
    {
      "sha256": "9f2c…64-hex…",
      "action": "revoke",
      "reason": "RCE via crafted args-json — see GHSA-xxxx",
      "ref": "https://vendor.example.com/fixer-ids.wasm",
      "since": "2026-09-30T00:00:00Z"
    },
    {
      "sha256": "a1b2…",
      "action": "yank",
      "reason": "author withdrew 1.2.x — data corruption under memo()",
      "ref": "git+https://github.com/acme/perfscale-isin@v1.2.0#libs/isin.wasm",
      "since": "2026-10-01T00:00:00Z"
    }
  ],
  "signatures": [
    { "key_id": "pfs-rev-2026a", "sig": "<base64 Ed25519 over the canonical bytes>" }
  ]
}
```

Rules:

- **Append-only and monotonic.** Entries are never removed; `sequence`
  increases by exactly 1 per publish. A "mistaken revocation" is undone by
  a new entry `"action": "unrevoke"` (last action per digest wins), so the
  history of the list is itself auditable and replaying an old list can
  only make an agent *stricter than current truth*, never more lenient
  than a list it has already seen. Consumers keep the highest-sequence
  valid list they have ever verified; an older sequence is ignored.
- **Keyed by digest, `ref` is decorative.** `sha256` is the enforcement key
  — it is what the cache, `perfscale.lock`, and the agent allowlist all
  speak. `ref` exists for human readability and for CLI messages; matching
  by `ref` alone never enforces anything.
- **No expiry on the document; freshness is derived.** There is no
  `expires_at` — an expiring list that vanishes when unmaintained is a
  fail-closed-by-accident design. Instead, consumers compute *staleness*
  from `issued_at` against local policy (see Enforcement). Leaning: no
  expiry field, ever; staleness is consumer-side policy.
- **Canonical form**: entries sorted by `(sha256, since)`, signatures over
  the document with the `signatures` field removed, UTF-8, no insignificant
  whitespace. Boring on purpose — canonicalization bugs are where signature
  schemes die.

### Key management and rotation

- Two Ed25519 signing keys, `pfs-rev-YYYYx` ids. Both public keys ship
  **compiled into the CLI and agent binaries** (same discipline as the
  self-update verification path); a list valid under either is accepted.
- **Planned rotation**: the retiring key signs the announcement release
  notes; the new key is added to binaries first, the old key stops signing
  one release cycle later. Lists are dual-signed during the overlap window.
- **Emergency rotation** (key compromise): a new engine/agent release ships
  the new key and drops the old one; the release itself is the out-of-band
  trust anchor (users already trust release signing). The revocation list
  does not attempt in-band key rollover — self-referential key update
  schemes are complexity that fails exactly when needed most.
- The list is signed by the Perfscale project, not by individual library
  authors. Author yanks are *submitted* (PR against the list's source of
  truth) and countersigned into the list by a maintainer — a curated
  bottleneck, accepted deliberately: volume is tiny, and unauthenticated
  self-service yank is a DoS vector against your own dependencies.

### Hosting and delivery — the channel

One canonical document, three delivery paths, in priority order per
consumer:

1. **Canonical URL**: a static file at a well-known location
   (`https://perfscale.ru/.well-known/perfscale-revocations.json`, mirrored
   to a raw GitHub URL in the perfscale repo as bootstrap/fallback). Static
   hosting is deliberate: the list must survive every dynamic service being
   down, and a CDN-cached static file is the hardest thing in our stack to
   take offline. Update path is a CI job in the perfscale repo.
2. **Controlplane mirror + push**: the controlplane fetches the canonical
   list on a timer, verifies it, and piggybacks the newest verified copy
   onto traffic it already has with agents — heartbeat responses and the
   SSE/polling task channel. The agent stores every verified copy it
   receives. This is the workhorse for fleet machines: it requires zero new
   inbound connectivity from agents (they already talk to the
   controlplane), works through the same M2M auth, and turns "agent has
   been offline for weeks" into "agent's list is as fresh as its last
   heartbeat".
3. **Opportunistic pull**: whenever the CLI or agent performs *any* network
   operation anyway (`perfscale install`, `perfscaled library install`
   against https/git, `--refresh`), it also fetches the canonical list
   (best-effort, short timeout, failure is a debug log — never blocks the
   install on the revocation channel being down).

Rejected alternatives:

- **Push-only via controlplane** (no canonical URL): leaves CLI-only users
  with no channel at all, and makes the controlplane a single point of
  failure for a security signal.
- **Agent polls the canonical URL on a schedule**: adds outbound internet
  as a *requirement* for fleet machines that today need only controlplane
  reachability; many fleets are deliberately egress-restricted. Polling
  also gives you nothing the piggyback doesn't, for machines that
  heartbeat.
- **CRL/OCSP-style per-digest online checks**: requires online at
  enforcement time — the exact property the offline architecture forbids.
- **Embedding revocations in task configs**: the controlplane could refuse
  to *dispatch* tasks referencing revoked digests (and phase 3 does add
  this), but as the only mechanism it leaves CLI-driven and directly-run
  agent workloads unprotected.

### Enforcement semantics

Two verbs, two severities, three enforcement points.

**Yank** (authorial): advisory everywhere. Installs print a loud warning
and proceed (the author cannot un-publish bits you already verified; a yank
is information, not a block). Runs warn once per run per digest. Lint
emits a warning. Rationale: yanks have legitimate false-positive rates
(relicensing, cosmetic withdrawal) and blocking them trains users to
disable the mechanism.

**Revoke** (security):

| Enforcement point | Fresh list (≤ policy staleness) | Stale list (> policy staleness) |
|---|---|---|
| `perfscale install` / `perfscaled library install` of a revoked digest | **Hard error** (override: `--allow-revoked`, logged) | Same — install is online by definition, so a stale list here means the fetch failed; warn loudly and allow (install-time is the least harmful place to accept risk; the digest will be gated at run time) |
| Run start — CLI and agent validation | **Hard error**: the run does not start; the message names the digest, the reason, the `ref`, and the remedy | **Warn loudly, run proceeds**; the warning states both the revocation *and* the list's age |
| `perfscale lint` | Error for revoked, warning for yanked | Warning noting list staleness |

**Freshness policy**: the consumer records `fetched_at` (local receipt
time) alongside the verified list. A list is *fresh* when
`now - issued_at ≤ PERFSCALE_REVOCATION_MAX_AGE` (default 7 days). The knob
is fleet-tunable: `PERFSCALE_REVOCATION_MAX_AGE` on the agent, plus
`PERFSCALE_REVOCATION_ENFORCEMENT=enforce|warn|off` (default `enforce`)
for operators who need a fleet-wide escape hatch that is itself explicit
and greppable. Setting `off` prints a startup warning; silent disablement
is not offered.

Why **fail-open on stale** rather than fail-closed: fail-closed on a stale
list means "controlplane outage eventually bricks every load test in the
fleet" — an availability catastrophe bought with a security property
(protection against a revocation published in the last N days that the
agent hasn't heard about) that is narrow and time-bounded. RFC 002's
stance is "warn loudly, let CI break" — the operator, not the tool, picks
the failure mode. Fail-open-with-loud-warning keeps that stance; the
`enforce`/`warn`/`off` knob hands the choice to the fleet owner explicitly.
Leaning: default `enforce` with 7-day staleness, revisit with operational
data.

**Why runs, not installs, are the primary gate**: the dangerous moment is
execution, not possession. A revoked digest may sit in a cache harmlessly
forever (and keeping it aids forensics); what must not happen is a *new
run* starting against it once the agent knows better. This also composes
correctly with uninstall+deburn: revocation does not delete anything —
removal stays an explicit operator/controlplane action.

**Agent integration** (fits `src/library.rs::enforce`): the revocation
check is a third policy axis beside capability intersection and the digest
allowlist, evaluated in the same `enforce()` pass that already rewrites
refs to cached paths and already treats anomalies as loud hard errors
(cache poisoning precedent). The agent's cached list lives beside the
library cache (`<cache>/revocations.json` + `<cache>/revocations.meta`),
survives restarts, and is updated by delivery paths 2 and 3. Runs in
flight are never killed mid-execution — enforcement happens at validation
time, before the runner starts (same boundary as auto-burn).

**CLI / lockfile compatibility**: `perfscale.lock` stays exactly as it is —
a record of resolved digests, immutable by revocation. Revocation is
out-of-band state, not lockfile state: adding revocation entries into the
lock would either mutate a file users own (bad) or freeze revocation
verdicts into version control (worse — an un-revoke could never propagate).
`perfscale lint` is the surface that joins the two: lock digests × cached
revocation list → errors/warnings, so CI catches a revoked pin without any
lockfile format change.

**Controlplane**: the `machine_libraries` table (migration 037) gains a
surfaced status, not a new mechanism: rows whose digest appears in the
mirror's verified list render `revoked`/`yanked` badges in the fleet UI,
and task dispatch gains a pre-flight check rejecting tasks that reference
a revoked digest with a fresh list (mirroring agent-side enforcement so
the failure appears in the UI, not just in agent logs). Uninstall+deburn
remains the remediation path, now with a "revoked" reason to click it.

## Benefits

- Closes the last open supply-chain hole in RFC 005: the system can now
  *learn* that a digest is bad, on every consumer class, without giving up
  the offline-run discipline that makes the design safe in the first place.
- One signed static file serves CLI users, fleet agents, and the
  controlplane — no new service, no new protocol, no new auth.
- Digest-keyed entries slot into the existing content-addressed cache and
  lockfile with zero migration; enforcement reuses the agent's existing
  fail-closed `enforce()` seam.
- The fail-open-on-stale + explicit-knob design keeps a controlplane outage
  from becoming a fleet outage, while making every downgrade visible and
  greppable.
- The format is the future RFC 002 marketplace yank mechanism for free —
  same file, same signatures, same consumer code.

## Drawbacks

- **A curated list is a maintainer bottleneck.** Every yank/revocation
  needs a project signature. Fine at current volume; becomes a process
  problem if a marketplace succeeds wildly (mitigation: the format admits
  delegated namespaces later, per Open questions).
- **Two more trust anchors to protect.** The signing keys join release
  signing as crown jewels; compromise of both is a bad day.
- **Fail-open on stale lists is a real, accepted gap**: an agent isolated
  longer than the staleness window runs a revocation it never received.
  That is the price of not bricking offline fleets; the RFC says so out
  loud rather than pretending otherwise.
- **Another local state file** (`revocations.json`) whose absence/corruption
  must degrade sanely (absent = empty list + "never fetched" warning, not
  an error — the first run on a fresh machine must not fail).
- **List growth is unbounded** (append-only). At expected volume (a handful
  of entries per year) irrelevant; at marketplace scale it needs paging or
  digest-range sharding — a good problem, deferred.

## Tradeoffs

- **Signed static list vs. online revocation API**: an API (OCSP-style)
  gives fresher, per-digest answers but demands connectivity at enforcement
  time — architecturally impossible here. Chosen: signed list, cached,
  verified offline; freshness becomes explicit staleness rather than hidden
  unavailability.
- **Fail-open vs. fail-closed on staleness**: fail-closed maximizes the
  security property and converts every controlplane/CDN outage into a
  fleet-wide outage days later. Chosen: fail-open with loud warnings plus
  an operator knob, per RFC 002's "let the operator pick the failure
  mode". The default list-max-age of 7 days bounds the silent window.
- **Digest-keyed vs. name/version-keyed entries**: name+version is
  friendlier ("all of `acme/isin@1.2.x`") but nothing in the enforcement
  path — cache, lock, allowlist — speaks versions; they speak digests.
  Chosen: digest as the enforcement key, `ref` for humans. A name-scoped
  entry type can be added later as a compile-time expansion to digests at
  publish time, never as a run-time matching rule.
- **Curated (project-signed) vs. self-service author yank**: self-service
  is the ecosystem norm (crates.io) but requires accounts, auth, and abuse
  handling that don't exist yet, and a forged self-service yank DoSes your
  own dependency. Chosen: curated now; the format doesn't preclude
  additional signer classes later.
- **Enforcing at run start vs. mid-run kill**: killing a live run the
  moment a revocation arrives is theatrically appealing and operationally
  wrong (a load test aborted mid-measurement can be worse than one that
  finishes; the library is already in memory either way). Chosen: gate at
  validation/start; a received-during-run revocation is logged loudly for
  the operator.

## Non-obvious pitfalls

1. **Replayed old lists are only dangerous in one direction.** An attacker
   serving sequence 10 to a victim who has seen 42 is *weakening*
   enforcement — hence the highest-sequence-wins cache rule. But the
   mirror case matters: a victim whose *only* copy is old must not be told
   "you're fine, list is valid" — signature validity and freshness are
   different checks, and the UI/logs must surface staleness independently
   of signature validity.
2. **Clock skew breaks staleness math.** An agent with a wrong clock can
   consider a stale list fresh (or vice versa). `issued_at` far in the
   future is a signature-valid-but-absurd input: clamp — treat
   `issued_at > now + 1h` as suspicious and log it, don't trust it.
   Monotonic enforcement state (highest sequence seen) must never move
   backward even across clock weirdness.
3. **The first run on a fresh machine has no list.** Absent file must mean
   "unknown, warn once" — never "fail closed" (every new fleet machine
   would brick) and never "silently fine" (that trains people to ignore
   the state). The message should name how to fetch the list explicitly.
4. **Un-revoke must be as loud as revoke.** An `unrevoke` entry that
   silently re-enables a digest re-opens the exact window the revocation
   closed; operators who uninstalled during the incident get no signal.
   Un-revocations appear in the same warnings surfaced to operators.
5. **`--allow-revoked` overrides become habit-forming.** Every override is
   a permanent fixture in someone's CI within a week. Overrides must be
   per-invocation flags (never env-config-file settings that outlive the
   incident), must print the reason string they're bypassing, and lint
   should flag lockfiles whose only installs used overrides.
6. **The controlplane mirror is a trust *repeater*, not a trust source.**
   Agents must verify the signature on the mirrored copy exactly as if
   fetched from the canonical URL. A controlplane that re-signs or
   truncates the list (even benignly, e.g. "only this tenant's digests")
   breaks the threat model where the controlplane itself is the attacker
   or is compromised.
7. **Revocation checking must stay off the hot path.** All checks happen
   at install and at run-start validation (`enforce()`), never per-call —
   same boundary discipline as auto-burn. A per-`${...}` revocation lookup
   would be a self-inflicted pitfall-1-of-RFC-005.
8. **Dual-signature overlap windows are when mistakes happen.** During key
   rotation, an old binary knows only key A, a new binary knows A+B, the
   list signs with B: old binaries reject a perfectly valid list and — per
   fail-open — keep running with warnings. Acceptable, but the rotation
   runbook must dual-sign until the oldest supported binary knows B, and
   the changelog must say so loudly.

## Alternatives considered

- **Do nothing; rely on uninstall+deburn as the revocation mechanism.**
  That is the status quo: operators can remove a digest, but only if they
  *find out* out-of-band. A security mechanism that depends on operators
  reading CVE feeds is not a mechanism. Rejected.
- **Short-lived signed lists with mandatory refresh (fail-closed expiry).**
  Maximizes revocation propagation pressure; guarantees that an unmaintained
  or unreachable channel eventually bricks every offline fleet. Security
  posture purchased by converting availability into a hostage. Rejected as
  the default; available via `PERFSCALE_REVOCATION_ENFORCEMENT` for fleets
  that want it.
- **Per-library publisher revocation endpoints** (each `ref` host serves
  its own list). Distributes trust but multiplies failure modes, key
  management, and code paths by publisher count, and a malicious publisher
  simply never revokes itself. Rejected; the curated list is the single
  choke point that can act against a hostile publisher.
- **Bake revocations into engine releases only** (no channel). Correct but
  glacial: revocation latency becomes "when did you last upgrade", and it
  conflates two independent upgrade decisions. Rejected as the sole
  mechanism; releases remain the *key-rotation* vehicle.
- **Sigstore/Rekor-style transparency log for revocations.** Real audit
  value at real operational cost; the append-only signed list with
  monotonic sequence gets the auditable-history property without the
  infrastructure. Rejected for v1; the sequence field leaves the door open.

## Rollout plan

0. **This RFC** circulated; list format frozen before phase 1 code.
1. **Format + CLI only** (no platform): canonical list file + publish CI in
   the perfscale repo; Ed25519 verify in perfscale-core; `perfscale install`
   refuses revoked digests (`--allow-revoked` escape), warns on yanked;
   `perfscale lint` joins lock × cached list; `perfscale revocations`
   inspect/refresh command. Everything works with a hand-maintained list.
2. **Agent**: cached list beside the library cache, `enforce()` gains the
   revocation axis, `PERFSCALE_REVOCATION_*` knobs, `perfscaled library
   revocations` operator view, opportunistic fetch during installs.
3. **Controlplane**: mirror + verify + piggyback on heartbeat/SSE,
   `machine_libraries` revoked/yanked badges (migration 037 status
   extension), dispatch pre-flight rejection, admin UI surface for list
   freshness per machine.
4. **Registry adoption**: when RFC 002's marketplace exists, yank in the
   registry *is* an entry in this list — one mechanism, not two.

Phases 1–2 deliver the full security property for CLI and standalone-agent
users with zero platform dependency; phase 3 is fleet ergonomics.

## Open questions

- Exact staleness default: 7 days is a guess at the tradeoff between
  "isolated fleet runs known-bad code" and "noisy warnings". Leaning: ship
  7d, tune from controlplane-observed heartbeat gaps.
- Should yanked digests eventually *block* installs (like revoked) after a
  deprecation window? Ecosystems differ (crates.io: never; npm: deprecated
  is advisory forever). Leaning: advisory forever — blocking creep is how
  users learn to pass `--allow-*` reflexively.
- Tenant-scoped revocation lists (a company banning a digest fleet-wide
  independent of the global list): natural controlplane feature, needs a
  second signed-list slot with tenant keys. Leaning: design in the
  controlplane RFC that needs it first, not speculatively here.
- Delegated signer namespaces (publishers countersign their own entries
  into the list) if marketplace volume outgrows the curated bottleneck.
  Leaning: revisit only with real volume.
- Whether the agent should proactively *quarantine* (rename-aside) revoked
  cached artifacts to make accidental reuse impossible, vs. leaving bytes
  in place for forensics. Leaning: leave in place; uninstall is explicit.

## Success metrics

- A revoked digest installed yesterday is refused at run start on every
  online consumer within one heartbeat/install cycle, with a message naming
  the reason — demonstrated in an end-to-end drill, not inferred.
- An agent isolated for 30 days runs with a loud staleness warning and
  zero hard failures attributable to the revocation channel.
- A controlplane outage of >7 days produces warnings, not a fleet outage
  (the fail-open property, tested by chaos-drill, not by accident).
- The first real-world revocation (or drill) reaches >90% of heartbeating
  fleet agents within 24h.
- Zero silent enforcement downgrades: every warn-mode decision appears in
  logs/UI with the reason and the list age.
