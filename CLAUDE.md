# perfscale — opensource repo rules

This repo is open-source. External contributors read commit history — keep it clean.

## Commit messages

- Do NOT add "🤖 Generated with Claude Code" / "Co-Authored-By: Anthropic Claude" footers, or any AI-attribution footer, to commit messages in this repo.
- Plain Conventional Commits style: `type(scope): summary`, optional body explaining why.

## serve dashboard UI

`perfscale serve` embeds a dashboard built from `crates/perfscale-cli/ui/` (React + TypeScript + Vite + Tailwind + shadcn/ui, single-file build via vite-plugin-singlefile). The built `crates/perfscale-cli/src/commands/serve_ui.html` is committed — the Rust build has no Node. After editing the UI: `pnpm install && pnpm build` in `crates/perfscale-cli/ui/` and commit the regenerated `serve_ui.html`; `pnpm check-built` (also a CI job) verifies freshness, `pnpm test` runs the Vitest suite.

## graphify


This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
