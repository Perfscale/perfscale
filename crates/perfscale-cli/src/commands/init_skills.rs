//! `perfscale init-skills` — install the embedded agent skill folders.
//!
//! Mirrors Playwright's `init-skills`: the skills ship inside the binary
//! (embedded with `include_str!`, the same pattern the man page uses) and
//! are copied as-is to `.<loop>/skills/<skill>/` under the cwd, or under the
//! home directory with `--global`. Re-running overwrites.

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use crate::cli::InitSkillsArgs;
use crate::error::CliError;

/// One embedded file: path relative to the skill root, plus its contents.
struct EmbeddedFile {
    path: &'static str,
    contents: &'static str,
}

/// The shipped skill set (names become `.<loop>/skills/<name>/`).
const SKILLS: &[(&str, &[EmbeddedFile])] = &[
    (
        "perfscale-cli",
        &[
            EmbeddedFile {
                path: "SKILL.md",
                contents: include_str!("../../../../skills/perfscale-cli/SKILL.md"),
            },
            EmbeddedFile {
                path: "references/yaml-reference.md",
                contents: include_str!(
                    "../../../../skills/perfscale-cli/references/yaml-reference.md"
                ),
            },
            EmbeddedFile {
                path: "references/actions.md",
                contents: include_str!("../../../../skills/perfscale-cli/references/actions.md"),
            },
            EmbeddedFile {
                path: "references/load-shapes.md",
                contents: include_str!(
                    "../../../../skills/perfscale-cli/references/load-shapes.md"
                ),
            },
            EmbeddedFile {
                path: "references/thresholds.md",
                contents: include_str!("../../../../skills/perfscale-cli/references/thresholds.md"),
            },
            EmbeddedFile {
                path: "references/metrics.md",
                contents: include_str!("../../../../skills/perfscale-cli/references/metrics.md"),
            },
            EmbeddedFile {
                path: "references/reporters.md",
                contents: include_str!("../../../../skills/perfscale-cli/references/reporters.md"),
            },
            EmbeddedFile {
                path: "references/protocols.md",
                contents: include_str!("../../../../skills/perfscale-cli/references/protocols.md"),
            },
        ],
    ),
    (
        "perfscale-library-sdk",
        &[
            EmbeddedFile {
                path: "SKILL.md",
                contents: include_str!("../../../../skills/perfscale-library-sdk/SKILL.md"),
            },
            EmbeddedFile {
                path: "references/steps.md",
                contents: include_str!(
                    "../../../../skills/perfscale-library-sdk/references/steps.md"
                ),
            },
            EmbeddedFile {
                path: "references/libraries.md",
                contents: include_str!(
                    "../../../../skills/perfscale-library-sdk/references/libraries.md"
                ),
            },
            EmbeddedFile {
                path: "references/reporters.md",
                contents: include_str!(
                    "../../../../skills/perfscale-library-sdk/references/reporters.md"
                ),
            },
            EmbeddedFile {
                path: "references/distribution.md",
                contents: include_str!(
                    "../../../../skills/perfscale-library-sdk/references/distribution.md"
                ),
            },
        ],
    ),
    (
        "perfscale-migration-k6",
        &[EmbeddedFile {
            path: "SKILL.md",
            contents: include_str!("../../../../skills/perfscale-migration-k6/SKILL.md"),
        }],
    ),
    (
        "perfscale-run-analysis",
        &[
            EmbeddedFile {
                path: "SKILL.md",
                contents: include_str!("../../../../skills/perfscale-run-analysis/SKILL.md"),
            },
            EmbeddedFile {
                path: "references/diagnosis.md",
                contents: include_str!(
                    "../../../../skills/perfscale-run-analysis/references/diagnosis.md"
                ),
            },
            EmbeddedFile {
                path: "references/artifacts.md",
                contents: include_str!(
                    "../../../../skills/perfscale-run-analysis/references/artifacts.md"
                ),
            },
        ],
    ),
];

pub async fn run(args: InitSkillsArgs) -> Result<(), CliError> {
    let base = if args.global {
        dirs::home_dir().ok_or_else(|| {
            CliError::new("cannot locate the home directory for --global")
                .hint("set $HOME (Unix) or %USERPROFILE% (Windows)")
        })?
    } else {
        std::env::current_dir()
            .map_err(|e| CliError::new("cannot read the current directory").cause(e.to_string()))?
    };
    let target_dir = args.r#loop.loop_dir();
    let written = install(&base, target_dir)?;
    println!(
        "installed {} skills into {}/{target_dir}/skills:",
        SKILLS.len(),
        base.display()
    );
    for name in written {
        println!("  - {name}");
    }
    Ok(())
}

/// Copy every embedded skill under `<base>/<loop_dir>/skills/<name>/`.
/// Existing files are overwritten; returns the installed skill names.
fn install(base: &Path, loop_dir: &str) -> Result<Vec<&'static str>, CliError> {
    let skills_root = base.join(loop_dir).join("skills");
    let mut names = Vec::new();
    for (name, files) in SKILLS {
        for file in *files {
            let dest = skills_root.join(name).join(file.path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    CliError::new(format!("cannot create {}", parent.display()))
                        .cause(e.to_string())
                })?;
            }
            std::fs::write(&dest, file.contents).map_err(|e| {
                CliError::new(format!("cannot write {}", dest.display())).cause(e.to_string())
            })?;
        }
        names.push(*name);
    }
    Ok(names)
}

/// Exposed for tests.
#[cfg(test)]
pub fn skills_root(base: &Path, loop_dir: &str) -> PathBuf {
    base.join(loop_dir).join("skills")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_all_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let names = install(tmp.path(), ".claude").unwrap();
        assert_eq!(
            names,
            [
                "perfscale-cli",
                "perfscale-library-sdk",
                "perfscale-migration-k6",
                "perfscale-run-analysis"
            ]
        );
        let root = skills_root(tmp.path(), ".claude");
        assert!(root.join("perfscale-cli/SKILL.md").exists());
        assert!(root.join("perfscale-cli/references/actions.md").exists());
        assert!(root.join("perfscale-library-sdk/SKILL.md").exists());
        assert!(root.join("perfscale-migration-k6/SKILL.md").exists());
        assert!(root
            .join("perfscale-library-sdk/references/steps.md")
            .exists());
        assert!(root.join("perfscale-run-analysis/SKILL.md").exists());
        assert!(root
            .join("perfscale-run-analysis/references/diagnosis.md")
            .exists());
    }

    #[test]
    fn rerun_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        install(tmp.path(), ".agents").unwrap();
        let edited = skills_root(tmp.path(), ".agents").join("perfscale-cli/SKILL.md");
        std::fs::write(&edited, "drifted").unwrap();
        install(tmp.path(), ".agents").unwrap();
        assert_ne!(std::fs::read_to_string(&edited).unwrap(), "drifted");
    }
}
