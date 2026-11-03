//! `perfscale init-agents` — generate loop-specific agent definitions, the
//! MCP server config, and the shared repo scaffolding (specs/, seed test,
//! optional prompt templates).
//!
//! Mirrors Playwright's `init-agents` (`generateAgents.ts`): loop-agnostic
//! `agents/*.agent.md` sources with abstract tools (`search`, `edit`,
//! `perfscale-test/<tool>`) are translated per loop; each loop also gets an
//! MCP config registering `npx -y @perfscale/mcp` (`cmd /c` on Windows).
//! Differences from Playwright, per issue #15: the default loop is `claude`
//! (not `copilot`) and the `coverage` prompt is replaced by `threshold`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::cli::{AgentLoop, InitAgentsArgs};
use crate::error::CliError;

/// MCP server name every loop registers; `perfscale-test/<tool>` maps to it.
const MCP_SERVER: &str = "perfscale-test";

const AGENT_SOURCES: &[&str] = &[
    include_str!("../../../../agents/perfscale-test-planner.agent.md"),
    include_str!("../../../../agents/perfscale-test-generator.agent.md"),
    include_str!("../../../../agents/perfscale-test-healer.agent.md"),
];

const PROMPT_SOURCES: &[(&str, &str)] = &[
    (
        "perfscale-plan.prompt.md",
        include_str!("../../../../prompts/perfscale-plan.prompt.md"),
    ),
    (
        "perfscale-generate.prompt.md",
        include_str!("../../../../prompts/perfscale-generate.prompt.md"),
    ),
    (
        "perfscale-heal.prompt.md",
        include_str!("../../../../prompts/perfscale-heal.prompt.md"),
    ),
    (
        "perfscale-threshold.prompt.md",
        include_str!("../../../../prompts/perfscale-threshold.prompt.md"),
    ),
];

const SPECS_README: &str = "# Test plans\n\n\
    This directory holds scenario plans written by the `perfscale-test-planner`\n\
    agent (`specs/<name>.md`: step flow, load shape, test data, SLO thresholds).\n\
    The `perfscale-test-generator` agent turns a plan here into a test under\n\
    `tests/`.\n";

const SEED_TEST: &str = "# Seed test — the minimal scenario agents model new tests on.\n\
    # Set BASE_URL to your target (or edit the url below).\n\
    # Run: perfscale run -f tests/seed.yaml -c config.yaml\n\
    steps:\n  \
    - name: health probe\n    \
    use: std/http@v1\n    \
    with:\n      \
    method: GET\n      \
    url: ${{ env.BASE_URL }}\n";

const SEED_CONFIG: &str = "# Seed run config — small by default; agents verify with 1 VU\n\
    # and a short duration unless you explicitly ask for a full-load run.\n\
    vus: 1\n\
    duration: 10s\n";

const COPILOT_SETUP_WORKFLOW: &str =
    "# Lets the Copilot coding agent run perfscale (binary + MCP server).\n\
    name: Copilot Setup Steps\n\
    on: workflow_dispatch\n\
    \n\
    jobs:\n  \
    copilot-setup-steps:\n    \
    runs-on: ubuntu-latest\n    \
    steps:\n      \
    - uses: actions/checkout@v4\n      \
    - uses: actions/setup-node@v4\n        \
    with:\n          \
    node-version: 20\n      \
    - name: Install perfscale\n        \
    run: npm install -g @perfscale/exe\n";

/// Parsed loop-agnostic agent source (`agents/*.agent.md`).
struct Agent {
    name: String,
    description: String,
    model: String,
    color: String,
    tools: Vec<String>,
    body: String,
}

/// What `initRepo` found or created, as repo-relative paths for substitution.
#[derive(Debug)]
struct RepoLayout {
    seed_file: String,
    config_file: String,
}

pub async fn run(args: InitAgentsArgs) -> Result<(), CliError> {
    let base = std::env::current_dir()
        .map_err(|e| CliError::new("cannot read the current directory").cause(e.to_string()))?;
    let layout = init_repo(&base, args.config.as_deref())?;
    let agents = parse_agents(&layout)?;
    let mut written = generate(&base, args.r#loop, &agents)?;
    if args.prompts {
        written.extend(write_prompts(&base, args.r#loop, &layout)?);
    }
    println!("loop: {}", args.r#loop);
    println!(
        "seed: {} (config: {})",
        layout.seed_file, layout.config_file
    );
    for path in written {
        println!("  wrote {}", path.display());
    }
    Ok(())
}

/// Shared repo init: `specs/README.md`, a seed test, and a config.
///
/// The seed is find-or-create: an existing `tests/seed.yaml` is kept
/// untouched. `-c <config>` selects the config file (it must exist);
/// without it an existing `config.yaml`/`perfscale.config.yaml` is reused,
/// else a minimal 1-VU `config.yaml` is created.
fn init_repo(base: &Path, config: Option<&Path>) -> Result<RepoLayout, CliError> {
    write_if_missing(base, Path::new("specs/README.md"), SPECS_README)?;

    let seed_rel = Path::new("tests/seed.yaml");
    if !base.join(seed_rel).exists() {
        write_file(base, seed_rel, SEED_TEST)?;
    }

    let config_rel = match config {
        Some(path) => {
            let abs = if path.is_absolute() {
                path.to_path_buf()
            } else {
                base.join(path)
            };
            if !abs.exists() {
                return Err(CliError::new(format!(
                    "config file '{}' does not exist",
                    path.display()
                ))
                .hint("pass an existing config, or omit -c to seed a minimal config.yaml")
                .docs("agents.md"));
            }
            path.to_path_buf()
        }
        None => {
            let candidates = [
                PathBuf::from("config.yaml"),
                PathBuf::from("perfscale.config.yaml"),
            ];
            match candidates.iter().find(|p| base.join(p).exists()) {
                Some(p) => p.clone(),
                None => {
                    write_file(base, Path::new("config.yaml"), SEED_CONFIG)?;
                    PathBuf::from("config.yaml")
                }
            }
        }
    };

    Ok(RepoLayout {
        seed_file: seed_rel.to_string_lossy().replace('\\', "/"),
        config_file: config_rel.to_string_lossy().replace('\\', "/"),
    })
}

fn parse_agents(layout: &RepoLayout) -> Result<Vec<Agent>, CliError> {
    AGENT_SOURCES
        .iter()
        .map(|src| parse_agent(src, layout))
        .collect()
}

/// Split `---` frontmatter and substitute `${seedFile}` / `${configFile}`.
fn parse_agent(source: &str, layout: &RepoLayout) -> Result<Agent, CliError> {
    let source = substitute(source, layout);
    let mut lines = source.lines();
    if lines.next() != Some("---") {
        return Err(CliError::new("embedded agent source misses frontmatter"));
    }
    let (mut name, mut description, mut model, mut color) = (None, None, None, None);
    let mut tools = Vec::new();
    let mut in_tools = false;
    let mut body_lines = Vec::new();
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter {
            if line == "---" {
                in_frontmatter = false;
                continue;
            }
            if let Some(item) = line.strip_prefix("  - ") {
                if in_tools {
                    tools.push(item.trim().to_string());
                }
                continue;
            }
            in_tools = false;
            if let Some((key, value)) = line.split_once(':') {
                let value = value.trim().to_string();
                match key.trim() {
                    "name" => name = Some(value),
                    "description" => description = Some(value),
                    "model" => model = Some(value),
                    "color" => color = Some(value),
                    "tools" => in_tools = true,
                    _ => {}
                }
            }
        } else {
            body_lines.push(line);
        }
    }
    Ok(Agent {
        name: name.ok_or_else(|| CliError::new("embedded agent misses `name`"))?,
        description: description
            .ok_or_else(|| CliError::new("embedded agent misses `description`"))?,
        model: model.unwrap_or_else(|| "inherit".into()),
        color: color.unwrap_or_else(|| "blue".into()),
        tools,
        body: body_lines.join("\n").trim().to_string() + "\n",
    })
}

fn substitute(text: &str, layout: &RepoLayout) -> String {
    text.replace("${seedFile}", &layout.seed_file)
        .replace("${configFile}", &layout.config_file)
}

/// Claude Code tool mapping (`search` → `Glob, Grep, Read, LS`, …).
fn map_tools_claude(tools: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for tool in tools {
        match tool.as_str() {
            "search" => out.extend(["Glob", "Grep", "Read", "LS"].map(String::from)),
            "edit" => out.extend(["Edit", "MultiEdit", "Write"].map(String::from)),
            other => out.push(format!("mcp__{}__{}", MCP_SERVER, tool_name(other))),
        }
    }
    out
}

/// `perfscale-test/run_test` → `run_test`.
fn tool_name(abstract_tool: &str) -> &str {
    abstract_tool
        .strip_prefix("perfscale-test/")
        .unwrap_or(abstract_tool)
}

/// MCP command tuple: plain `npx` on Unix, `cmd /c npx` on Windows.
fn mcp_command() -> (&'static str, Vec<&'static str>) {
    if cfg!(windows) {
        ("cmd", vec!["/c", "npx", "-y", "@perfscale/mcp"])
    } else {
        ("npx", vec!["-y", "@perfscale/mcp"])
    }
}

fn mcp_server_json() -> Value {
    let (command, args) = mcp_command();
    json!({ "command": command, "args": args })
}

/// Merge `server_json` into the JSON MCP config at `rel` (created if
/// missing), preserving other servers.
fn write_json_mcp_config(
    base: &Path,
    rel: &Path,
    top_key: &str,
    written: &mut Vec<PathBuf>,
) -> Result<(), CliError> {
    let abs = base.join(rel);
    let mut root: Value = match std::fs::read_to_string(&abs) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            CliError::new(format!("cannot parse {}", rel.display()))
                .cause(e.to_string())
                .hint("fix the JSON, or remove the file and re-run init-agents")
        })?,
        Err(_) => json!({}),
    };
    root.as_object_mut()
        .ok_or_else(|| CliError::new(format!("{} is not a JSON object", rel.display())))?
        .entry(top_key)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            CliError::new(format!(
                "{}: `{}` is not a JSON object",
                rel.display(),
                top_key
            ))
        })?
        .insert(MCP_SERVER.to_string(), mcp_server_json());
    let text = serde_json::to_string_pretty(&root).expect("json serializes") + "\n";
    write_file(base, rel, &text)?;
    written.push(rel.to_path_buf());
    Ok(())
}

fn generate(
    base: &Path,
    agent_loop: AgentLoop,
    agents: &[Agent],
) -> Result<Vec<PathBuf>, CliError> {
    let mut written = Vec::new();
    match agent_loop {
        AgentLoop::Claude => {
            for agent in agents {
                let tools = map_tools_claude(&agent.tools).join(", ");
                let out = format!(
                    "---\nname: {}\ndescription: {}\nmodel: {}\ncolor: {}\ntools: {tools}\n---\n\n{}",
                    agent.name, agent.description, agent.model, agent.color, agent.body
                );
                let rel = Path::new(".claude/agents").join(format!("{}.md", agent.name));
                write_file(base, &rel, &out)?;
                written.push(rel);
            }
            write_json_mcp_config(base, Path::new(".mcp.json"), "mcpServers", &mut written)?;
        }
        AgentLoop::Codex => {
            for agent in agents {
                let tools = agent
                    .tools
                    .iter()
                    .map(|t| format!("\"{}\"", t.replace("perfscale-test/", "perfscale-test_")))
                    .collect::<Vec<_>>()
                    .join(", ");
                let out = format!(
                    "name = \"{}\"\ndescription = \"{}\"\nmodel = \"{}\"\ntools = [{tools}]\n\n\
                     instructions = '''\n{}\n'''\n",
                    agent.name, agent.description, agent.model, agent.body
                );
                let rel = Path::new(".codex/agents").join(format!("{}.toml", agent.name));
                write_file(base, &rel, &out)?;
                written.push(rel);
            }
            write_codex_mcp_config(base, &mut written)?;
        }
        AgentLoop::Opencode => {
            for agent in agents {
                let out = format!(
                    "---\ndescription: {}\n---\n\n{}",
                    agent.description, agent.body
                );
                let rel = Path::new(".opencode/prompts").join(format!("{}.md", agent.name));
                write_file(base, &rel, &out)?;
                written.push(rel);
            }
            let server = json!({ "type": "local", "command": mcp_command_vec(), "enabled": true });
            write_opencode_config(base, server, &mut written)?;
        }
        AgentLoop::Copilot => {
            // Modern copilot agents; supersede legacy chatmode and old-format
            // agent files, but leave unrelated user files alone.
            delete_legacy(
                base,
                Path::new(".github/chatmodes"),
                ".chatmode.md",
                &mut written,
            )?;
            delete_legacy(base, Path::new(".github/agents"), ".md", &mut written)?;
            for agent in agents {
                write_github_agent(
                    base,
                    agent,
                    true,
                    ".agent.md",
                    ".github/agents",
                    &mut written,
                )?;
            }
            write_if_missing(
                base,
                Path::new(".github/workflows/copilot-setup-steps.yml"),
                COPILOT_SETUP_WORKFLOW,
            )?;
            written.push(PathBuf::from(".github/workflows/copilot-setup-steps.yml"));
        }
        AgentLoop::Vscode => {
            for agent in agents {
                write_github_agent(
                    base,
                    agent,
                    false,
                    ".agent.md",
                    ".github/agents",
                    &mut written,
                )?;
            }
            write_json_mcp_config(base, Path::new(".vscode/mcp.json"), "servers", &mut written)?;
        }
        AgentLoop::VscodeLegacy => {
            for agent in agents {
                write_github_agent(
                    base,
                    agent,
                    false,
                    ".chatmode.md",
                    ".github/chatmodes",
                    &mut written,
                )?;
            }
            write_json_mcp_config(base, Path::new(".vscode/mcp.json"), "servers", &mut written)?;
        }
    }
    Ok(written)
}

/// `.github/agents/*.agent.md` / `.github/chatmodes/*.chatmode.md` —
/// frontmatter carries the tools (MCP tools as `server/tool`) and, for
/// copilot, the `mcp-servers` list.
fn write_github_agent(
    base: &Path,
    agent: &Agent,
    mcp_servers: bool,
    suffix: &str,
    dir: &str,
    written: &mut Vec<PathBuf>,
) -> Result<(), CliError> {
    let tools = agent.tools.join(", ");
    let mcp = if mcp_servers {
        format!("mcp-servers: [{MCP_SERVER}]\n")
    } else {
        String::new()
    };
    let out = format!(
        "---\nname: {}\ndescription: {}\n{mcp}tools: {tools}\n---\n\n{}",
        agent.name, agent.description, agent.body
    );
    let rel = Path::new(dir).join(format!("{}{}", agent.name, suffix));
    write_file(base, &rel, &out)?;
    written.push(rel);
    Ok(())
}

/// Copilot upgrade path: remove legacy `perfscale-test-*.chatmode.md` and
/// old-format agent files so only the new `.agent.md` files remain.
fn delete_legacy(
    base: &Path,
    dir: &Path,
    suffix: &str,
    written: &mut Vec<PathBuf>,
) -> Result<(), CliError> {
    let abs = base.join(dir);
    let Ok(entries) = std::fs::read_dir(&abs) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("perfscale-test-") && name.ends_with(suffix) {
            std::fs::remove_file(entry.path()).map_err(|e| {
                CliError::new(format!("cannot remove {}", entry.path().display()))
                    .cause(e.to_string())
            })?;
            written.push(dir.join(name));
        }
    }
    Ok(())
}

/// Codex registers MCP servers in TOML (`[mcp_servers.<name>]`); append the
/// perfscale block unless already present.
fn write_codex_mcp_config(base: &Path, written: &mut Vec<PathBuf>) -> Result<(), CliError> {
    let rel = Path::new(".codex/config.toml");
    let existing = std::fs::read_to_string(base.join(rel)).unwrap_or_default();
    if existing.contains("[mcp_servers.perfscale-test]") {
        return Ok(());
    }
    let (command, args) = mcp_command();
    let args = args
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let block = format!("\n[mcp_servers.{MCP_SERVER}]\ncommand = \"{command}\"\nargs = [{args}]\n");
    write_file(base, rel, &(existing + &block))?;
    written.push(rel.to_path_buf());
    Ok(())
}

/// opencode.json: `{ "mcp": { "<name>": { type: local, command: [...] } } }`.
fn write_opencode_config(
    base: &Path,
    server: Value,
    written: &mut Vec<PathBuf>,
) -> Result<(), CliError> {
    let rel = Path::new("opencode.json");
    let abs = base.join(rel);
    let mut root: Value = match std::fs::read_to_string(&abs) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| CliError::new("cannot parse opencode.json").cause(e.to_string()))?,
        Err(_) => json!({ "$schema": "https://opencode.ai/config.json" }),
    };
    root.as_object_mut()
        .ok_or_else(|| CliError::new("opencode.json is not a JSON object"))?
        .entry("mcp")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| CliError::new("opencode.json: `mcp` is not a JSON object"))?
        .insert(MCP_SERVER.to_string(), server);
    let text = serde_json::to_string_pretty(&root).expect("json serializes") + "\n";
    write_file(base, rel, &text)?;
    written.push(rel.to_path_buf());
    Ok(())
}

fn mcp_command_vec() -> Vec<String> {
    let (command, args) = mcp_command();
    std::iter::once(command)
        .chain(args)
        .map(String::from)
        .collect()
}

/// `--prompts`: copy the prompt templates into the loop's prompt directory
/// with `${seedFile}` / `${configFile}` substituted. There is no `coverage`
/// prompt — `threshold` takes its place (issue #15).
fn write_prompts(
    base: &Path,
    agent_loop: AgentLoop,
    layout: &RepoLayout,
) -> Result<Vec<PathBuf>, CliError> {
    let dir = match agent_loop {
        AgentLoop::Claude => ".claude/commands",
        AgentLoop::Codex => ".codex/prompts",
        AgentLoop::Opencode => ".opencode/prompts",
        AgentLoop::Copilot | AgentLoop::Vscode | AgentLoop::VscodeLegacy => ".github/prompts",
    };
    let mut written = Vec::new();
    for (name, source) in PROMPT_SOURCES {
        let rel = Path::new(dir).join(name);
        write_file(base, &rel, &substitute(source, layout))?;
        written.push(rel);
    }
    Ok(written)
}

fn write_if_missing(base: &Path, rel: &Path, contents: &str) -> Result<(), CliError> {
    if base.join(rel).exists() {
        return Ok(());
    }
    write_file(base, rel, contents)
}

fn write_file(base: &Path, rel: &Path, contents: &str) -> Result<(), CliError> {
    let abs = base.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CliError::new(format!("cannot create {}", parent.display())).cause(e.to_string())
        })?;
    }
    std::fs::write(&abs, contents)
        .map_err(|e| CliError::new(format!("cannot write {}", abs.display())).cause(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> RepoLayout {
        RepoLayout {
            seed_file: "tests/seed.yaml".into(),
            config_file: "config.yaml".into(),
        }
    }

    #[test]
    fn agent_sources_parse() {
        let agents = parse_agents(&layout()).unwrap();
        assert_eq!(agents.len(), 3);
        assert_eq!(agents[0].name, "perfscale-test-planner");
        assert!(agents[0].tools.contains(&"search".to_string()));
        assert!(agents[0]
            .tools
            .contains(&"perfscale-test/list_actions".to_string()));
        assert!(agents[0].body.contains("tests/seed.yaml"));
    }

    #[test]
    fn claude_tool_mapping() {
        let tools = vec![
            "search".to_string(),
            "edit".to_string(),
            "perfscale-test/run_test".to_string(),
        ];
        let mapped = map_tools_claude(&tools);
        assert!(mapped.contains(&"Glob".to_string()));
        assert!(mapped.contains(&"Edit".to_string()));
        assert!(mapped.contains(&"mcp__perfscale-test__run_test".to_string()));
    }

    #[test]
    fn init_repo_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let first = init_repo(tmp.path(), None).unwrap();
        assert_eq!(first.seed_file, "tests/seed.yaml");
        assert_eq!(first.config_file, "config.yaml");
        // Drift the seed; a second run must not touch it.
        std::fs::write(tmp.path().join("tests/seed.yaml"), "custom").unwrap();
        let second = init_repo(tmp.path(), None).unwrap();
        assert_eq!(second.seed_file, "tests/seed.yaml");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("tests/seed.yaml")).unwrap(),
            "custom"
        );
        assert!(tmp.path().join("specs/README.md").exists());
    }

    #[test]
    fn init_repo_rejects_missing_config() {
        let tmp = tempfile::tempdir().unwrap();
        let err = init_repo(tmp.path(), Some(Path::new("nope.yaml"))).unwrap_err();
        assert!(err.to_string().contains("nope.yaml"));
    }

    #[test]
    fn mcp_config_merges_existing_servers() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".mcp.json"),
            r#"{ "mcpServers": { "other": { "command": "x" } } }"#,
        )
        .unwrap();
        let mut written = Vec::new();
        write_json_mcp_config(
            tmp.path(),
            Path::new(".mcp.json"),
            "mcpServers",
            &mut written,
        )
        .unwrap();
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert!(parsed["mcpServers"]["other"].is_object());
        assert!(parsed["mcpServers"]["perfscale-test"]["args"]
            .as_array()
            .unwrap()
            .contains(&json!("@perfscale/mcp")));
    }

    #[test]
    fn every_loop_generates() {
        for agent_loop in [
            AgentLoop::Claude,
            AgentLoop::Codex,
            AgentLoop::Copilot,
            AgentLoop::Opencode,
            AgentLoop::Vscode,
            AgentLoop::VscodeLegacy,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let agents = parse_agents(&layout()).unwrap();
            let written = generate(tmp.path(), agent_loop, &agents).unwrap();
            assert!(written.len() >= 3, "{agent_loop}");
            for rel in &written {
                assert!(
                    tmp.path().join(rel).exists(),
                    "{} {}",
                    agent_loop,
                    rel.display()
                );
            }
        }
    }
}
