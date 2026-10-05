# perfscale RFCs

Design documents for substantial changes: what changes, why, the tradeoffs,
and the pitfalls — before code, so the hard decisions are argued on paper.

| # | Title | Status | Depends on |
|---|---|---|---|
| [001](001-sdk.md) | SDK | Draft | — |
| [002](002-marketplace.md) | Marketplace | Draft | 001, 003 |
| [003](003-composite-step.md) | Composite step | Draft | — |
| [004](004-setup-teardown.md) | Setup and teardown | Implemented | — |
| [005](005-libraries.md) | Libraries — WASM value generators | Draft | — |
| [006](006-library-revocation.md) | Library revocation — signed revocation list for offline agents | Draft | 005 |
| [007](007-webrtc.md) | pro/webrtc — WebRTC load testing (full media path) | Draft | 005 |
| [008](008-pro-steps.md) | Pro step families — architecture, catalog, registration contract | Draft | 004, 005 |

## Status values

`Draft` → `Under review` → `Accepted` / `Rejected` → `Implemented` /
`Superseded`.

## Process

1. Copy the shape of an existing RFC: Summary, Motivation, Goals/Non-goals,
   Detailed design, Benefits, Drawbacks, Tradeoffs, Non-obvious pitfalls,
   Alternatives, Rollout, Open questions, Success metrics.
2. Number sequentially (`NNN-slug.md`).
3. An RFC is a proposal, not a promise — `Draft` means "argue with it".
