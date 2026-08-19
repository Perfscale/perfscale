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

use perfscale_core::import::{self, ImportOptions};
use perfscale_core::lint::{detect_kind, lint, DocKind, LintIssue};

use crate::cli::{LintArgs, SchemaKind};
use crate::error::{CliError, DOCS_BASE};

pub async fn run(args: LintArgs) -> Result<(), CliError> {
    let mut any_problems = false;
    let import_opts = ImportOptions {
        allow_remote: args.allow_remote_import,
        refresh: args.refresh_imports,
        cache_dir: None,
        remote_guard: None,
    };

    for path in &args.files {
        match lint_file(path, args.schema, &import_opts).await {
            Ok((kind, effective, issues)) if issues.is_empty() => {
                // Advisory findings never affect the exit code.
                for warning in perfscale_core::lint::lint_warnings(&effective, kind) {
                    println!("  warning: {warning}");
                }
                // The offline pass is clean; the GraphQL network pass may
                // still find schema-level problems. It runs on the merged
                // text so imported steps are covered too.
                let (remote, notes) = graphql_remote_pass(&effective, kind, args.offline).await;
                for note in &notes {
                    println!("  note: {note}");
                }
                if remote.is_empty() {
                    println!("✓ {} ({}) — ok", path.display(), kind_label(kind));
                } else {
                    any_problems = true;
                    print_issues(path, kind, &remote);
                }
            }
            Ok((kind, effective, issues)) => {
                for warning in perfscale_core::lint::lint_warnings(&effective, kind) {
                    println!("  warning: {warning}");
                }
                any_problems = true;
                print_issues(path, kind, &issues);
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

/// Lint one file. Returns the effective document kind, the effective
/// (import-merged) text, and any findings.
async fn lint_file(
    path: &Path,
    schema: SchemaKind,
    import_opts: &ImportOptions,
) -> Result<(DocKind, String, Vec<LintIssue>), CliError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CliError::new(format!("failed to read '{}'", path.display()))
            .cause(e.to_string())
            .hint("`perfscale lint` expects YAML test-definition or config files")
            .docs("yaml-reference.md")
    })?;

    // Documents with an `import:` key lint the *merged* result — that is
    // what would actually run. Resolution failures (cycle, missing base,
    // remote blocked without --allow-remote-import) surface as findings.
    let effective = if has_import_key(&text) {
        match import::load_document(path, import_opts).await {
            Ok((value, _)) => serde_yaml::to_string(&value).unwrap_or_else(|_| text.clone()),
            Err(e) => {
                let suggestion = if e.contains("--allow-remote-import") {
                    Some("network imports are opt-in: add --allow-remote-import".to_string())
                } else {
                    None
                };
                let kind = match schema {
                    SchemaKind::Auto => detect_kind(&text),
                    SchemaKind::Test => DocKind::Test,
                    SchemaKind::Config => DocKind::Config,
                };
                return Ok((
                    kind,
                    text,
                    vec![LintIssue {
                        location: "/import".into(),
                        problem: e,
                        suggestion,
                    }],
                ));
            }
        }
    } else {
        text
    };

    let kind = match schema {
        SchemaKind::Auto => detect_kind(&effective),
        SchemaKind::Test => DocKind::Test,
        SchemaKind::Config => DocKind::Config,
    };
    let issues = lint(&effective, kind);
    Ok((kind, effective, issues))
}

/// Cheap check whether a document has a top-level `import` key (avoids the
/// resolve path — with its canonicalize/network cost — for plain files).
fn has_import_key(text: &str) -> bool {
    serde_yaml::from_str::<serde_json::Value>(text)
        .map(|v| v.get("import").is_some())
        .unwrap_or(false)
}

/// The GraphQL schema pass: steps with a `schema_file` validate against the
/// local SDL (offline by nature); the rest introspect the endpoint, unless
/// `--offline` is given. Runs on the effective (import-merged) text so
/// imported steps are validated too. Skipped for config documents. Never
/// fails the command by itself — findings come back as issues.
async fn graphql_remote_pass(
    effective: &str,
    kind: DocKind,
    offline: bool,
) -> (Vec<LintIssue>, Vec<String>) {
    if kind != DocKind::Test {
        return (Vec::new(), Vec::new());
    }
    perfscale_core::lint::lint_graphql_remote(effective, offline).await
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
