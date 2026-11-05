# Coding agents

Two commands wire perfscale into coding agents (Claude Code, Codex, Copilot,
opencode, VS Code). They mirror Playwright's `init-skills` / `init-agents`.

## `perfscale init-skills`

Installs the skill folders that teach an agent the perfscale formats. Skills
ship embedded in the binary and are copied as-is — re-running overwrites, so
skills stay in sync with the installed perfscale version.

```sh
perfscale init-skills                 # ./.claude/skills/<skill>   (default loop)
perfscale init-skills --loop agents   # ./.agents/skills/<skill>
perfscale init-skills -g              # ~/.claude/skills/<skill> (all projects)
```

Shipped skills:

- **perfscale-cli** — the test/config YAML pair, `lint`/`run`/`schema`, plus
  references: YAML, the `std/*` action catalog, load shapes, thresholds,
  metrics, reporters, protocols.
- **perfscale-library-sdk** — extending perfscale with the official SDK
  (`@perfscale/library-sdk`, `perfscale-library-sdk`): value generators
  (`${alias.fn()}`), custom steps (`lib/<alias>/<step>`) with their own
  outputs, checks and metrics, trusted process steps, config extensions,
  WASM reporters, install/lock/burn.
- **perfscale-migration-k6** — the k6 → perfscale guide and metric mapping.
- **perfscale-run-analysis** — reading a run: summary, `[stats]`,
  `--summary-export` / `report@v1`, open-model latency and dropped
  iterations, target vs generator bottlenecks, `abort_on_fail` +
  `http_debug`, threshold verdicts and fair run comparison.

## `perfscale init-agents`

Generates agent definitions for one loop, registers the perfscale MCP server,
and initializes the repo scaffolding:

```sh
perfscale init-agents                            # claude (default loop)
perfscale init-agents --loop vscode              # one of: claude, codex,
                                                 # copilot, opencode, vscode,
                                                 # vscode-legacy
perfscale init-agents -c staging.config.yaml     # seed against an existing config
perfscale init-agents --prompts                  # also install prompt templates
```

Re-run after every perfscale upgrade — there is no version check.

### What gets generated

| Loop | Agent files | MCP config |
|---|---|---|
| `claude` | `.claude/agents/perfscale-test-*.md` | `.mcp.json` |
| `codex` | `.codex/agents/perfscale-test-*.toml` | `.codex/config.toml` |
| `copilot` | `.github/agents/perfscale-test-*.agent.md` | `mcp-servers` frontmatter + `.github/workflows/copilot-setup-steps.yml` |
| `opencode` | `.opencode/prompts/perfscale-test-*.md` | `opencode.json` |
| `vscode` | `.github/agents/perfscale-test-*.agent.md` | `.vscode/mcp.json` |
| `vscode-legacy` | `.github/chatmodes/perfscale-test-*.chatmode.md` | `.vscode/mcp.json` |

Every MCP config registers the [`@perfscale/mcp` server](mcp.md) as
`npx -y @perfscale/mcp` (`cmd /c npx …` on Windows). Existing config files are
merged, not replaced. Switching to `--loop copilot` deletes legacy
`perfscale-test-*.chatmode.md` and old-format `perfscale-test-*.md` agent
files; unrelated files are preserved.

### The three agents

- **perfscale-test-planner** — turns a request (plus the seed test and
  optional OpenAPI/HAR/PRD files) into `specs/<name>.md`: step flow, load
  shape, test data, SLO thresholds.
- **perfscale-test-generator** — turns a plan into `tests/<name>.yaml` via
  the MCP write tools (every write is linted), live-checked with a small run.
- **perfscale-test-healer** — reproduces a failure small, patches the test,
  re-runs; marks the test `skip:` with a reason when the target itself is
  broken.

### Repo scaffolding (always)

- `specs/README.md` — the directory for test plans.
- `tests/seed.yaml` — a minimal 1-VU probe test agents model new tests on.
  Found-or-created: an existing seed is never overwritten.
- `config.yaml` — created only when no config exists and `-c` was not given;
  `-c <config>` selects an existing config instead (missing file = error).

### Prompts (`--prompts`)

`perfscale-{plan,generate,heal,threshold}.prompt.md` land in the loop's
prompt directory (`.claude/commands`, `.codex/prompts`, `.opencode/prompts`,
`.github/prompts`). Playwright's `coverage` prompt is replaced by
**threshold**: "will the target hold this load?" — a stepped `run_test` up to
the target load, ending in a HOLD / NOT HOLD verdict per SLO threshold.

### Load-testing safety rules (baked into the agents)

- Generator and healer runs default to **1 VU and a short duration**. Full
  load runs happen only through the `threshold` prompt — which states the
  target load and asks for explicit confirmation first — or an explicit user
  request.
- The healer **never edits `thresholds:`**: loosening an SLO is not a fix.
