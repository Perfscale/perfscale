//! `perfscale lint` — validate test/config YAML files without running them.
//!
//! Output per file:
//!
//! ```text
//! ✗ test.yaml (test definition) — 2 problems
//!   1. /steps/0: "use" is a required property
//!      fix: every step must name an action: `use: std/http@v1`, ...
//!   2. /steps/1: unknown field 'chek'
//!      fix: did you mean 'check'?
//! ✓ config.yaml (config) — ok
//!
//! docs: https://github.com/Perfscale/perfscale/blob/main/docs/yaml-reference.md
//! ```
//!
//! Exit code: 0 when every file is clean, 1 otherwise.

use std::path::Path;

use perfscale_core::lint::{detect_kind, lint, DocKind, LintIssue};

use crate::cli::{LintArgs, SchemaKind};
use crate::error::{CliError, DOCS_BASE};

pub async fn run(args: LintArgs) -> Result<(), CliError> {
    let mut any_problems = false;

    for path in &args.files {
        match lint_file(path, args.schema) {
            Ok(issues) if issues.is_empty() => {
                // The offline pass is clean; the GraphQL network pass may
                // still find schema-level problems.
                let (remote, notes) = graphql_remote_pass(path, args.schema, args.offline).await;
                for note in &notes {
                    println!("  note: {note}");
                }
                if remote.is_empty() {
                    println!(
                        "✓ {} ({}) — ok",
                        path.display(),
                        kind_label(effective_kind(path, args.schema))
                    );
                } else {
                    any_problems = true;
                    print_issues(path, effective_kind(path, args.schema), &remote);
                }
            }
            Ok(issues) => {
                any_problems = true;
                print_issues(path, effective_kind(path, args.schema), &issues);
            }
            Err(e) => return Err(e),
        }
    }

    if any_problems {
        println!("\ndocs: {DOCS_BASE}/yaml-reference.md");
        // Distinct from CliError: lint findings were already printed above in
        // their own format, so exit directly instead of stacking a second error.
        std::process::exit(1);
    }

    Ok(())
}

fn lint_file(path: &Path, schema: SchemaKind) -> Result<Vec<LintIssue>, CliError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CliError::new(format!("failed to read '{}'", path.display()))
            .cause(e.to_string())
            .hint("`perfscale lint` expects YAML test-definition or config files")
            .docs("yaml-reference.md")
    })?;
    let kind = match schema {
        SchemaKind::Auto => detect_kind(&text),
        SchemaKind::Test => DocKind::Test,
        SchemaKind::Config => DocKind::Config,
    };
    Ok(lint(&text, kind))
}

/// The kind actually used for a file (mirrors `lint_file`'s choice, for labels).
fn effective_kind(path: &Path, schema: SchemaKind) -> DocKind {
    match schema {
        SchemaKind::Auto => std::fs::read_to_string(path)
            .map(|t| detect_kind(&t))
            .unwrap_or(DocKind::Config),
        SchemaKind::Test => DocKind::Test,
        SchemaKind::Config => DocKind::Config,
    }
}

/// The GraphQL schema pass: steps with a `schema_file` validate against the
/// local SDL (offline by nature); the rest introspect the endpoint, unless
/// `--offline` is given. Skipped for config documents and unreadable files
/// (the sync pass already reported those). Never fails the command by
/// itself — findings come back as issues.
async fn graphql_remote_pass(
    path: &Path,
    schema: SchemaKind,
    offline: bool,
) -> (Vec<LintIssue>, Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (Vec::new(), Vec::new());
    };
    let kind = match schema {
        SchemaKind::Auto => detect_kind(&text),
        SchemaKind::Test => DocKind::Test,
        SchemaKind::Config => DocKind::Config,
    };
    if kind != DocKind::Test {
        return (Vec::new(), Vec::new());
    }
    perfscale_core::lint::lint_graphql_remote(&text, offline).await
}

fn kind_label(kind: DocKind) -> &'static str {
    match kind {
        DocKind::Test => "test definition",
        DocKind::Config => "config",
    }
}

fn print_issues(path: &Path, kind: DocKind, issues: &[LintIssue]) {
    let plural = if issues.len() == 1 {
        "problem"
    } else {
        "problems"
    };
    println!(
        "✗ {} ({}) — {} {plural}",
        path.display(),
        kind_label(kind),
        issues.len()
    );
    for (i, issue) in issues.iter().enumerate() {
        println!("  {}. {}: {}", i + 1, issue.location, issue.problem);
        if let Some(fix) = &issue.suggestion {
            println!("     fix: {fix}");
        }
    }
}
