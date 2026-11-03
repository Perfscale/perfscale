//! `perfscale init-skills` / `perfscale init-agents` integration tests.
//!
//! Everything runs in temp dirs; no network, no MCP server, no load run —
//! the tests only assert the files the commands write (agent definitions,
//! MCP configs, prompts, seed test) and their idempotency/overwrite rules.

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

fn cmd() -> Command {
    Command::cargo_bin("perfscale").unwrap()
}

fn cmd_in(dir: &Path) -> Command {
    let mut cmd = cmd();
    cmd.current_dir(dir);
    // No update checks against the real network, ever.
    cmd.env("PERFSCALE_NO_UPDATE_CHECK", "1");
    cmd
}

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

// ---------------------------------------------------------------------------
// init-skills
// ---------------------------------------------------------------------------

#[test]
fn init_skills_defaults_to_claude() {
    let tmp = tempfile::tempdir().unwrap();
    cmd_in(tmp.path()).arg("init-skills").assert().success();
    assert!(tmp
        .path()
        .join(".claude/skills/perfscale-cli/SKILL.md")
        .exists());
    assert!(tmp
        .path()
        .join(".claude/skills/perfscale-cli/references/yaml-reference.md")
        .exists());
    assert!(tmp
        .path()
        .join(".claude/skills/perfscale-library-sdk/SKILL.md")
        .exists());
    assert!(tmp
        .path()
        .join(".claude/skills/perfscale-migration-k6/SKILL.md")
        .exists());
    assert!(!tmp.path().join(".agents").exists());
}

#[test]
fn init_skills_agents_loop() {
    let tmp = tempfile::tempdir().unwrap();
    cmd_in(tmp.path())
        .args(["init-skills", "--loop", "agents"])
        .assert()
        .success();
    assert!(tmp
        .path()
        .join(".agents/skills/perfscale-cli/SKILL.md")
        .exists());
    assert!(!tmp.path().join(".claude").exists());
}

#[test]
fn init_skills_global_installs_under_home() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    cmd_in(tmp.path())
        .args(["init-skills", "-g"])
        .env("HOME", home.path())
        .assert()
        .success();
    assert!(home
        .path()
        .join(".claude/skills/perfscale-cli/SKILL.md")
        .exists());
    assert!(!tmp.path().join(".claude").exists());
}

#[test]
fn init_skills_rerun_overwrites() {
    let tmp = tempfile::tempdir().unwrap();
    cmd_in(tmp.path()).arg("init-skills").assert().success();
    let skill = tmp.path().join(".claude/skills/perfscale-cli/SKILL.md");
    std::fs::write(&skill, "drifted").unwrap();
    cmd_in(tmp.path()).arg("init-skills").assert().success();
    assert_ne!(std::fs::read_to_string(&skill).unwrap(), "drifted");
}

// ---------------------------------------------------------------------------
// init-agents
// ---------------------------------------------------------------------------

#[test]
fn init_agents_defaults_to_claude() {
    let tmp = tempfile::tempdir().unwrap();
    cmd_in(tmp.path()).arg("init-agents").assert().success();
    // Three agents with claude tool mapping.
    let planner = read(tmp.path(), ".claude/agents/perfscale-test-planner.md");
    assert!(planner.contains("name: perfscale-test-planner"));
    assert!(planner.contains("mcp__perfscale-test__run_test"));
    assert!(planner.contains("Glob, Grep, Read, LS"));
    assert!(tmp
        .path()
        .join(".claude/agents/perfscale-test-generator.md")
        .exists());
    assert!(tmp
        .path()
        .join(".claude/agents/perfscale-test-healer.md")
        .exists());
    // MCP config registers the server without invoking it.
    let mcp = read(tmp.path(), ".mcp.json");
    assert!(mcp.contains("\"perfscale-test\""));
    assert!(mcp.contains("@perfscale/mcp"));
    // Shared repo init.
    assert!(tmp.path().join("specs/README.md").exists());
    assert!(tmp.path().join("tests/seed.yaml").exists());
    assert!(tmp.path().join("config.yaml").exists());
    // Healer must be forbidden from loosening thresholds.
    let healer = read(tmp.path(), ".claude/agents/perfscale-test-healer.md");
    assert!(healer.contains("Thresholds are off-limits"));
}

#[test]
fn init_agents_seed_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    cmd_in(tmp.path()).arg("init-agents").assert().success();
    std::fs::write(tmp.path().join("tests/seed.yaml"), "custom seed").unwrap();
    cmd_in(tmp.path()).arg("init-agents").assert().success();
    assert_eq!(read(tmp.path(), "tests/seed.yaml"), "custom seed");
}

#[test]
fn init_agents_honors_config_flag() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("staging.config.yaml"),
        "vus: 5\nduration: 1m\n",
    )
    .unwrap();
    cmd_in(tmp.path())
        .args(["init-agents", "-c", "staging.config.yaml"])
        .assert()
        .success();
    // No default config.yaml is created when -c is given...
    assert!(!tmp.path().join("config.yaml").exists());
    // ...and prompts pick the config path up.
    cmd_in(tmp.path())
        .args(["init-agents", "-c", "staging.config.yaml", "--prompts"])
        .assert()
        .success();
    let prompt = read(tmp.path(), ".claude/commands/perfscale-plan.prompt.md");
    assert!(prompt.contains("staging.config.yaml"));
    assert!(prompt.contains("tests/seed.yaml"));
}

#[test]
fn init_agents_missing_config_fails() {
    let tmp = tempfile::tempdir().unwrap();
    cmd_in(tmp.path())
        .args(["init-agents", "-c", "nope.yaml"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nope.yaml"));
}

#[test]
fn init_agents_codex() {
    let tmp = tempfile::tempdir().unwrap();
    cmd_in(tmp.path())
        .args(["init-agents", "--loop", "codex"])
        .assert()
        .success();
    assert!(tmp
        .path()
        .join(".codex/agents/perfscale-test-planner.toml")
        .exists());
    let agent = read(tmp.path(), ".codex/agents/perfscale-test-planner.toml");
    assert!(agent.contains("instructions = '''"));
    assert!(agent.contains("You are the perfscale test planner"));
    let config = read(tmp.path(), ".codex/config.toml");
    assert!(config.contains("[mcp_servers.perfscale-test]"));
    assert!(config.contains("@perfscale/mcp"));
}

#[test]
fn init_agents_opencode() {
    let tmp = tempfile::tempdir().unwrap();
    cmd_in(tmp.path())
        .args(["init-agents", "--loop", "opencode"])
        .assert()
        .success();
    assert!(tmp
        .path()
        .join(".opencode/prompts/perfscale-test-planner.md")
        .exists());
    let config = read(tmp.path(), "opencode.json");
    assert!(config.contains("\"perfscale-test\""));
    assert!(config.contains("@perfscale/mcp"));
}

#[test]
fn init_agents_vscode_and_legacy() {
    let tmp = tempfile::tempdir().unwrap();
    cmd_in(tmp.path())
        .args(["init-agents", "--loop", "vscode"])
        .assert()
        .success();
    assert!(tmp
        .path()
        .join(".github/agents/perfscale-test-planner.agent.md")
        .exists());
    let mcp = read(tmp.path(), ".vscode/mcp.json");
    assert!(mcp.contains("\"servers\""));
    assert!(mcp.contains("@perfscale/mcp"));

    let legacy = tempfile::tempdir().unwrap();
    cmd_in(legacy.path())
        .args(["init-agents", "--loop", "vscode-legacy"])
        .assert()
        .success();
    assert!(legacy
        .path()
        .join(".github/chatmodes/perfscale-test-planner.chatmode.md")
        .exists());
    assert!(legacy.path().join(".vscode/mcp.json").exists());
}

#[test]
fn init_agents_copilot_setup_and_legacy_cleanup() {
    let tmp = tempfile::tempdir().unwrap();
    // Pre-existing legacy chatmode and old-format agent files get deleted.
    let legacy_dir = tmp.path().join(".github/chatmodes");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::write(legacy_dir.join("perfscale-test-planner.chatmode.md"), "old").unwrap();
    std::fs::write(legacy_dir.join("unrelated.chatmode.md"), "keep").unwrap();
    let agents_dir = tmp.path().join(".github/agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(agents_dir.join("perfscale-test-planner.md"), "old").unwrap();
    std::fs::write(agents_dir.join("unrelated.md"), "keep").unwrap();

    cmd_in(tmp.path())
        .args(["init-agents", "--loop", "copilot"])
        .assert()
        .success();

    assert!(tmp
        .path()
        .join(".github/agents/perfscale-test-planner.agent.md")
        .exists());
    let agent = read(tmp.path(), ".github/agents/perfscale-test-planner.agent.md");
    assert!(agent.contains("mcp-servers: [perfscale-test]"));
    assert!(tmp
        .path()
        .join(".github/workflows/copilot-setup-steps.yml")
        .exists());
    assert!(!legacy_dir
        .join("perfscale-test-planner.chatmode.md")
        .exists());
    assert!(legacy_dir.join("unrelated.chatmode.md").exists());
    assert!(!agents_dir.join("perfscale-test-planner.md").exists());
    assert!(agents_dir.join("unrelated.md").exists());
}

#[test]
fn init_agents_prompts_for_each_loop_family() {
    for (loop_name, dir) in [
        ("claude", ".claude/commands"),
        ("codex", ".codex/prompts"),
        ("opencode", ".opencode/prompts"),
        ("copilot", ".github/prompts"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        cmd_in(tmp.path())
            .args(["init-agents", "--loop", loop_name, "--prompts"])
            .assert()
            .success();
        for prompt in ["plan", "generate", "heal", "threshold"] {
            let rel = format!("{dir}/perfscale-{prompt}.prompt.md");
            assert!(tmp.path().join(&rel).exists(), "{loop_name}: {rel}");
        }
        // No `coverage` prompt — `threshold` replaces it (issue #15).
        assert!(!tmp
            .path()
            .join(format!("{dir}/perfscale-coverage.prompt.md"))
            .exists());
        // The threshold prompt demands explicit confirmation before full load.
        let threshold = read(tmp.path(), &format!("{dir}/perfscale-threshold.prompt.md"));
        assert!(threshold.contains("HOLD"));
        assert!(threshold.contains("confirm"));
    }
}

#[test]
fn init_agents_without_prompts_writes_none() {
    let tmp = tempfile::tempdir().unwrap();
    cmd_in(tmp.path()).arg("init-agents").assert().success();
    assert!(!tmp.path().join(".claude/commands").exists());
}
