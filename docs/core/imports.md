# Composing documents: `import`

Both `-f test.yaml` and `-c config.yaml` accept a top-level `import:` key
naming a **base document**. The base loads first — recursively, since it may
import its own base — then the importing document deep-merges on top. Teams
share one blessed base (load shape, variables, thresholds) and each service
overrides only what differs.

```yaml
# perf/config/_base.yaml — owned by the platform team
vus: 50
duration: 5m
variables:
  region: eu-west
  base_url: https://staging.example.com

# services/checkout/perf.yaml — owned by the checkout team
import: ../../perf/config/_base.yaml
variables:
  base_url: https://checkout.staging.example.com   # region inherited
```

## Merge semantics

- **Objects merge key-by-key, recursively** — `variables:` from the base and
  the importing file combine; same-name keys are won by the importing side.
- **Everything else is replaced** — scalars, and arrays *including `steps:`*.
  A test document that defines `steps:` replaces the base's list outright
  (there is no per-step splicing); one that omits `steps:` inherits the
  base's list unchanged.
- Chains work: A imports B imports C → C loads first, B merges over it, A
  merges over the result. Cycles fail with the chain printed.

Validation runs on the **merged** document — `perfscale lint` and
`perfscale run` both resolve imports first, so an error points at what would
actually execute.

## Source forms

```yaml
# 1. relative filesystem path — resolved against the importing file's dir
import: ../shared/_base.yaml

# 2. raw HTTP(S) URL — the ref is already pinned in the path
import: "https://raw.githubusercontent.com/org/repo/v1.2.0/perf/config/_base.yaml"

# 3. a file at a ref of any git remote (SSH or HTTPS, self-hosted included)
import:
  git: git@gitlab.example.com:group/repo.git
  ref: v1.2.0          # tag, branch, or commit SHA
  file: perf/config/_base.yaml
```

Git imports shell out to your system `git` (`clone --depth 1`), so SSH keys,
credential helpers, and proxy settings all apply — private repos work with
whatever auth `git clone` already has. There is no libgit2 dependency.

## Security: remote imports are opt-in

Import resolution happens at config-load time — **before** the
`allow_file_actions` / `allow_process_actions` gates ever run. If the config
itself comes from an untrusted place (say, a file in a PR), a network
`import:` inside it is an SSRF primitive and a supply-chain vector: a remote
base could carry `allow_process_actions: true` plus a
`std/child_process@v1` step.

Permission to touch the network therefore comes from the **caller**, never
from the document:

```sh
perfscale run -f t.yaml -c c.yaml --allow-remote-import
perfscale lint c.yaml --allow-remote-import
```

Without the flag, any URL or git import fails closed with an explanation.
There is deliberately no YAML field that enables it — a document cannot
grant itself network access.

Origins stay confined:

- A document fetched from a **URL** resolves relative imports against its
  own URL. It cannot name a local filesystem path.
- A document from a **git repo** may only import files inside that same
  clone; `../` escapes past the repository root are rejected.
- Only local documents on your disk may start a chain into the network (and
  only under the flag).

## Caching

Git clones land under `~/.cache/perfscale/imports/<hash(git+ref)>`
(`$XDG_CACHE_HOME` respected; `$PERFSCALE_CACHE_DIR` overrides the root).

| Ref kind | Behaviour |
|---|---|
| Commit SHA | Immutable — cached forever, never refetched |
| Tag | Treated as immutable — cached forever; `--refresh-imports` refetches |
| Branch | Mutable — after a 5-minute TTL the remote is revalidated with `git ls-remote`; a moved branch refetches, an unreachable remote falls back to the cached copy with a `[sys]` warning |

The branch rule matters: without it a floating `ref: main` would silently
freeze at whatever the first fetch saw, and every subsequent run would claim
to test against "latest" while using a stale base. `--refresh-imports`
forces the check immediately.

HTTP imports are fetched per run (pin a tag or SHA in the URL for
reproducibility).

## Interaction with the rest of the engine

- `import` composes the *document*; it is not a step and consumes no
  iteration time. After the merge the result is validated against the same
  JSON Schema as any plain file.
- `${{ vars.* }}` interpolation happens at run time, after the merge — a
  base can reference variables the importing file supplies, and vice versa.
- `perfscale lint` reports resolution problems (missing base, cycle, remote
  blocked) as regular findings; with imports resolved it validates the
  merged document, including the GraphQL schema pass over imported steps.

## Limits

- One `import` per document (chain bases instead of listing several).
- No per-step merge surgery: `steps:` replaces wholesale.
- Import chains are capped at 16 levels; HTTP bodies at 10 MB.

A runnable pair lives in [`examples/import/`](../../examples/import/).
