# MCP server

`@perfscale/mcp` is a [Model Context Protocol](https://modelcontextprotocol.io)
server that exposes the perfscale CLI to AI agents (Claude Code, Claude
Desktop, and any other MCP client). The agent can author test YAML with
schema-validated writes, run load tests, and read back structured metrics —
without shell access.

Source: [Perfscale/mcp](https://github.com/Perfscale/mcp). For the hosted
platform (machines, runs, dashboards on perfscale.su/.ru) see the
`@perfscale/controlplane-mcp` server in the same repository.

## Setup

Requires the `perfscale` binary on `PATH` (override with `PERFSCALE_BIN`) and
Node.js 20+.

```json
{
  "mcpServers": {
    "perfscale": {
      "command": "npx",
      "args": ["-y", "@perfscale/mcp"]
    }
  }
}
```

With Claude Code:

```sh
claude mcp add perfscale -- npx -y @perfscale/mcp
```

## Tools

| Tool | What it does |
|---|---|
| `run_test` | Run a k6/locust/native test (`perfscale run`), return exit code + parsed [summary export](cli/commands.md#summary-export) |
| `lint` | Validate YAML files (`perfscale lint`), including typo and action-ID checks |
| `get_schema` | JSON Schema for `test` or `config` YAML (`perfscale schema`) |
| `parse_summary` | Parse raw k6-compatible output into structured metrics |
| `list_actions` | Catalog of native `std/*` step actions |
| `list_configs` | Recursively list YAML files in a directory, classified test/config |
| `read_config` | Read one YAML file with its detected kind |
| `write_test` | Create/overwrite a test definition, then lint it against the test schema |
| `write_config` | Create/overwrite a run config, then lint it against the config schema |
| `update_config` | Overwrite an existing file only (fails when absent), then lint |
| `remove_config` | Delete a test/config YAML file |

Every write is linted immediately — the agent sees schema violations in the
same tool result, so invalid YAML never silently lands on disk.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `PERFSCALE_BIN` | `perfscale` | Path to the perfscale binary |

## Notes

- `get_schema` requires a perfscale build with the `schema` subcommand — run
  `perfscale self-update` if you see an error mentioning it.
- `run_test` executes locally with the same semantics as
  [`perfscale run`](cli/commands.md#perfscale-run): exactly one of
  k6/locust/file, and native tests require a config.
