---
name: perfscale-library-sdk
description: Author perfscale WASM libraries in Rust, TypeScript, or Go — custom step actions and metric definitions — and wire them into test.yaml via imports. Use when a std/* action does not cover the protocol or logic needed.
---

# perfscale Library SDK

When the built-in `std/*` actions are not enough, ship a WASM library and
import it from `test.yaml`.

## Imports

```yaml
imports:
  - ./libs/my-lib.wasm            # local
  - https://example.com/lib.wasm  # remote, pinned in perfscale.lock
```

Run `perfscale install` to fetch remote libraries and pin digests in
`perfscale.lock`; `perfscale burn` builds a standalone binary with the
libraries embedded.

## WASM steps

A library exports step actions with their own IDs
(`<namespace>/<action>@v1`, namespaced to the library). They appear in
`steps:` exactly like `std/*` actions — with `with:` inputs, `check:`
assertions, and `outputs:` bindings — and are validated by
`perfscale lint`.

## defineMetric

Libraries declare their own metrics with `defineMetric` (name, kind, unit,
description). Defined metrics:

- show up in `perfscale metrics` output and the run summary next to the
  built-in families,
- are usable in `std/thresholds@v1` expressions like any other metric,
- stream through `report.reporters` with the same labels and histogram
  handling.

## Config extensions

A library may extend the run config with its own section (validated against
the schema the library declares), so library behavior (endpoints, pool
sizes, auth) lives in `config.yaml` instead of being hard-coded in test
steps.

## SDKs

Libraries can be authored in Rust, TypeScript, or Go against the WIT
interface in the perfscale repo (`wit/`). See the repo's
`docs/library-sdk.md` for the full authoring guide and the
`library-template` example project.
