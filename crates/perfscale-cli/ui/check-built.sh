#!/usr/bin/env bash
# Freshness check for the embedded dashboard: rebuild the UI from source and
# fail if the committed serve_ui.html differs. Mirrors the serve_docs.rs
# golden test — CI runs this so the Rust release build (which has no Node)
# can keep embedding a prebuilt page without it going stale.
set -euo pipefail
cd "$(dirname "$0")"

pnpm build >/dev/null
if ! git diff --exit-code --quiet -- ../src/commands/serve_ui.html; then
  echo "serve_ui.html is stale — rebuild with \`pnpm build\` in crates/perfscale-cli/ui and commit the result" >&2
  exit 1
fi
