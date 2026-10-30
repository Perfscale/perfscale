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
        ..Default::default()
    };

    // Load and import-resolve every file FIRST: library declarations in one
    // document (e.g. the config) validate `lib/<alias>/<step>` steps and
    // `${alias.fn(...)}` tokens in another (the runtime concatenates config +
    // test libraries, RFC 005/009) — per-file linting would report false
    // unknown aliases.
    let mut prepared = Vec::new();
    for path in &args.files {
        prepared.push((path, prepare_file(path, args.schema, &import_opts).await?));
    }
    let docs: Vec<(String, DocKind)> = prepared
        .iter()
        .map(|(_, (kind, effective, _))| (effective.clone(), *kind))
        .collect();
    let results: Vec<(Vec<LintIssue>, Vec<String>)> = if docs.len() == 1 {
        vec![(
            lint(&docs[0].0, docs[0].1),
            perfscale_core::lint::lint_warnings(&docs[0].0, docs[0].1),
        )]
    } else {
        perfscale_core::lint::lint_multi(&docs)
    };

    for ((path, (kind, effective, pre_issues)), (mut issues, warnings)) in
        prepared.into_iter().zip(results)
    {
        issues.extend(pre_issues);
        // Advisory findings never affect the exit code.
        for warning in warnings {
            println!("  warning: {warning}");
        }
        if issues.is_empty() {
            // The offline pass is clean; the GraphQL network pass may
            // still find schema-level problems. It runs on the merged
            // text so imported steps are covered too.
            let (remote, notes) = graphql_remote_pass(&effective, kind, args.offline).await;
            for note in &notes {
                println!("  note: {note}");
            }
            if remote.is_empty() {
                println!("✓ {} ({}) — ok", path.display(), kind_label(kind));
                print_effective_inputs(&effective, kind, &args)?;
            } else {
                any_problems = true;
                print_issues(path, kind, &remote);
            }
        } else {
            any_problems = true;
            print_issues(path, kind, &issues);
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

/// Load one file and resolve its imports. Returns the effective document
/// kind, the effective (import-merged) text, and any findings that arose
/// before linting proper (import resolution failures).
async fn prepare_file(
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

    // Documents go through the import loader even without an `import:` key:
    // it anchors relative `libraries[].use` paths to the file's directory
    // (RFC 005) — without this, lint would resolve them against the cwd
    // while `run` resolves them against the declaring file. With imports,
    // the *merged* result is linted — that is what would actually run.
    // Resolution failures (cycle, missing base, remote blocked without
    // --allow-remote-import) surface as findings.
    let effective = match import::load_document(path, import_opts).await {
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
    };

    let kind = match schema {
        SchemaKind::Auto => detect_kind(&effective),
        SchemaKind::Test => DocKind::Test,
        SchemaKind::Config => DocKind::Config,
    };
    // Linting proper happens after every file is prepared, so library
    // declarations combine across documents (see `run`).
    Ok((kind, effective, Vec::new()))
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

/// Print the effective resolved inputs for a config document that declares
/// them (issue #11): `-i` overrides and `-e` env files apply, so lint shows
/// exactly what the run would see. Resolution errors fail the lint — they
/// are the run's fail-fast errors too.
fn print_effective_inputs(effective: &str, kind: DocKind, args: &LintArgs) -> Result<(), CliError> {
    if kind != DocKind::Config {
        return Ok(());
    }
    let value: serde_json::Value = match serde_yaml::from_str(effective) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let Some(inputs) = value.get("inputs") else {
        return Ok(());
    };
    let decls: std::collections::BTreeMap<String, perfscale_core::inputs::InputDecl> =
        match serde_json::from_value(inputs.clone()) {
            Ok(d) => d,
            Err(_) => return Ok(()), // the schema pass flagged the shape
        };
    let overrides = crate::commands::run::parse_input_overrides(&args.input)?;
    let env_values = crate::commands::run::load_env_files(&args.env_file)?;
    let resolved = perfscale_core::inputs::resolve_inputs(&decls, &overrides, &env_values)
        .map_err(|msg| CliError::new(format!("invalid inputs: {msg}")))?;
    let rendered = resolved
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    println!("  inputs: {rendered}");
    Ok(())
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
