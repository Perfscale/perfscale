use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand};

use crate::error::DOCS_BASE;

fn top_level_after_help() -> String {
    format!(
        "Examples:\n  \
         perfscale run --k6 script.js                        run an existing k6 script\n  \
         perfscale run --locust locustfile.py --host <url>   run a locustfile headless\n  \
         perfscale run -f test.yaml -c config.yaml           run with the built-in engine\n  \
         perfscale serve                                     collect summaries from `run --report`\n\n\
         Documentation: {DOCS_BASE}/README.md"
    )
}

fn run_after_help() -> String {
    format!(
        "Exactly one of --k6 / --locust / -f selects the engine.\n\n\
         Examples:\n  \
         perfscale run --k6 script.js\n  \
         perfscale run --locust locustfile.py --host https://target.example.com -c load.yaml\n  \
         perfscale run -f test.yaml -c config.yaml\n  \
         perfscale run -f test.yaml -c config.yaml --quiet\n  \
         perfscale run -f test.yaml -c config.yaml --report http://localhost:7999\n  \
         perfscale run -f test.yaml -c config.yaml --summary-export result.json\n  \
         perfscale run --k6 script.js --summary-export \"$GITHUB_STEP_SUMMARY\" --summary-format md\n\n\
         The run exits 0 even when checks fail (that's load-test feedback, not a CLI error);\n\
         it exits 1 when the run itself can't execute (bad file, engine missing, invalid YAML).\n\n\
         Documentation: {DOCS_BASE}/cli/commands.md\n\
         YAML reference: {DOCS_BASE}/yaml-reference.md"
    )
}

fn serve_after_help() -> String {
    format!(
        "Endpoints:\n  \
         GET  /health           liveness probe, returns `ok`\n  \
         POST /api/v1/metrics   accepts {{\"lines\": [\"...\"]}} and prints the batch\n\n\
         Examples:\n  \
         perfscale serve                 listen on the default port 7999\n  \
         perfscale serve --port 9000     listen on a specific port\n  \
         perfscale serve --port 0        let the OS pick a free port (printed at startup)\n  \
         perfscale serve --tls           serve HTTPS with a self-signed certificate\n\n\
         Documentation: {DOCS_BASE}/cli/commands.md#perfscale-serve"
    )
}

#[derive(Parser)]
#[command(
    name = "perfscale",
    version,
    about = "Run k6, locust, or native load tests from one CLI",
    after_help = top_level_after_help()
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run a load test with k6, locust, or the native step engine.
    Run(RunArgs),
    /// Start a local dev server that receives metrics from `perfscale run --report`.
    Serve(ServeArgs),
    /// Validate test/config YAML files without running them.
    Lint(LintArgs),
    /// Update perfscale to the latest release for this platform.
    #[command(name = "self-update")]
    SelfUpdate(SelfUpdateArgs),
}

fn self_update_after_help() -> String {
    format!(
        "Downloads the release asset for this platform from GitHub Releases, verifies its\n\
         sha256 against the release's sha256sums.txt, and atomically replaces the current\n\
         executable.\n\n\
         Examples:\n  \
         perfscale self-update              update to the latest release\n  \
         perfscale self-update --check      only check; exit 10 if an update exists\n  \
         perfscale self-update --force      reinstall even if already up to date\n\n\
         The passive \"update available\" hint printed by other commands checks at most\n\
         once per 24h, only in interactive terminals, and can be disabled with\n\
         PERFSCALE_NO_UPDATE_CHECK=1.\n\n\
         Documentation: {DOCS_BASE}/cli/commands.md#perfscale-self-update"
    )
}

#[derive(Args)]
#[command(after_help = self_update_after_help())]
pub struct SelfUpdateArgs {
    /// Only check whether an update exists (exit code 10 = update available).
    #[arg(long)]
    pub check: bool,

    /// Reinstall the latest release even if this binary is already up to date.
    #[arg(long, conflicts_with = "check")]
    pub force: bool,
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .args(["k6", "locust", "file"]),
), after_help = run_after_help())]
pub struct RunArgs {
    /// Run a k6 script (requires `k6` on PATH; load config lives in the script's `options`).
    #[arg(long, value_name = "FILE.js")]
    pub k6: Option<PathBuf>,

    /// Run a locustfile headless (requires `locust` on PATH; combine with --host and -c).
    #[arg(long, value_name = "FILE.py")]
    pub locust: Option<PathBuf>,

    /// Run a native perfscale test definition (YAML with a `steps:` list; requires -c).
    #[arg(
        short = 'f',
        long = "file",
        value_name = "TEST.yaml",
        requires = "config"
    )]
    pub file: Option<PathBuf>,

    /// Load config: `vus`, `duration`, optional `report.url`. Required with -f,
    /// optional load hint for --locust, ignored by --k6.
    #[arg(short = 'c', long = "config", value_name = "CONFIG.yaml")]
    pub config: Option<PathBuf>,

    /// Target base URL for --locust runs (passed through as locust's --host).
    #[arg(long, value_name = "URL")]
    pub host: Option<String>,

    /// After the run, POST the metric summary to a `perfscale serve` instance,
    /// e.g. http://localhost:7999. Overrides `report.url` from the config file.
    #[arg(long, value_name = "URL")]
    pub report: Option<String>,

    /// Suppress per-request output; errors and the final metric summary still
    /// print. For the native engine the lines are dropped at the source, which
    /// also removes their formatting/IO cost under high load.
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// After the run, write the parsed metric summary plus run metadata
    /// (engine, VUs, duration, timestamp) to this file. Repeatable — one run
    /// can produce several exports. A `.md`/`.json` extension picks the
    /// format per file; otherwise `--summary-format` (default: JSON).
    #[arg(long, value_name = "FILE")]
    pub summary_export: Vec<PathBuf>,

    /// Format for --summary-export files without a recognized extension.
    /// `md` renders a Markdown table — handy for CI job summaries
    /// (e.g. `--summary-export "$GITHUB_STEP_SUMMARY" --summary-format md`).
    #[arg(long, value_enum, requires = "summary_export")]
    pub summary_format: Option<SummaryFormat>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SummaryFormat {
    Json,
    Md,
}

#[derive(Args)]
#[command(after_help = serve_after_help())]
pub struct ServeArgs {
    /// Port to listen on (0 = let the OS pick a free port).
    #[arg(long, default_value_t = 7999, value_name = "PORT")]
    pub port: u16,

    /// Serve over HTTPS with a self-signed certificate generated at startup.
    /// Clients must skip verification (`insecure: true` in std/http@v1,
    /// k6's `insecureSkipTLSVerify`, locust's `verify=False`).
    #[arg(long)]
    pub tls: bool,
}

fn lint_after_help() -> String {
    format!(
        "Validates against the generated JSON Schemas plus extra checks the schema\n\
         can't express: unknown/typo'd field names (with did-you-mean suggestions),\n\
         unknown action IDs, and per-action `with:` parameter names.\n\n\
         File kind is detected automatically (a top-level `steps:` key means test\n\
         definition, anything else is a config) — override with --schema.\n\n\
         Examples:\n  \
         perfscale lint test.yaml config.yaml\n  \
         perfscale lint --schema config load.yaml\n  \
         perfscale lint examples/*.yaml\n\n\
         Exit code: 0 when every file is valid, 1 otherwise.\n\n\
         YAML reference: {DOCS_BASE}/yaml-reference.md"
    )
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SchemaKind {
    /// Detect per file: top-level `steps:` → test, otherwise config.
    Auto,
    /// Force the test-definition schema.
    Test,
    /// Force the config schema.
    Config,
}

#[derive(Args)]
#[command(after_help = lint_after_help())]
pub struct LintArgs {
    /// YAML files to validate.
    #[arg(required = true, value_name = "FILE.yaml")]
    pub files: Vec<PathBuf>,

    /// Which schema to validate against.
    #[arg(long, value_enum, default_value_t = SchemaKind::Auto)]
    pub schema: SchemaKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("perfscale").chain(args.iter().copied()))
    }

    #[test]
    fn lint_requires_at_least_one_file() {
        assert!(parse(&["lint"]).is_err());
    }

    #[test]
    fn lint_accepts_multiple_files_and_schema_override() {
        let cli = parse(&["lint", "a.yaml", "b.yaml", "--schema", "config"]).unwrap();
        match cli.command {
            Commands::Lint(args) => {
                assert_eq!(args.files.len(), 2);
                assert!(matches!(args.schema, SchemaKind::Config));
            }
            _ => panic!("expected Lint"),
        }
    }

    #[test]
    fn lint_default_schema_is_auto() {
        let cli = parse(&["lint", "a.yaml"]).unwrap();
        match cli.command {
            Commands::Lint(args) => assert!(matches!(args.schema, SchemaKind::Auto)),
            _ => panic!("expected Lint"),
        }
    }

    #[test]
    fn run_k6_alone_parses() {
        let cli = parse(&["run", "--k6", "a.js"]).unwrap();
        match cli.command {
            Commands::Run(args) => {
                assert_eq!(args.k6, Some(PathBuf::from("a.js")));
                assert!(args.locust.is_none());
                assert!(args.file.is_none());
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn run_locust_with_host_and_config_parses() {
        let cli = parse(&[
            "run",
            "--locust",
            "b.py",
            "--host",
            "https://example.com",
            "-c",
            "cfg.yaml",
        ])
        .unwrap();
        match cli.command {
            Commands::Run(args) => {
                assert_eq!(args.locust, Some(PathBuf::from("b.py")));
                assert_eq!(args.host.as_deref(), Some("https://example.com"));
                assert_eq!(args.config, Some(PathBuf::from("cfg.yaml")));
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn run_native_file_with_config_parses() {
        let cli = parse(&["run", "-f", "t.yaml", "-c", "cfg.yaml"]).unwrap();
        match cli.command {
            Commands::Run(args) => {
                assert_eq!(args.file, Some(PathBuf::from("t.yaml")));
                assert_eq!(args.config, Some(PathBuf::from("cfg.yaml")));
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn run_report_flag_parses() {
        let cli = parse(&["run", "--k6", "a.js", "--report", "http://localhost:7999"]).unwrap();
        match cli.command {
            Commands::Run(args) => {
                assert_eq!(args.report.as_deref(), Some("http://localhost:7999"))
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn run_quiet_flag_parses_long_and_short() {
        for flags in [
            &["run", "--k6", "a.js", "--quiet"],
            &["run", "--k6", "a.js", "-q"],
        ] {
            let cli = parse(flags).unwrap();
            match cli.command {
                Commands::Run(args) => assert!(args.quiet),
                _ => panic!("expected Run"),
            }
        }
    }

    #[test]
    fn run_quiet_defaults_to_false() {
        let cli = parse(&["run", "--k6", "a.js"]).unwrap();
        match cli.command {
            Commands::Run(args) => assert!(!args.quiet),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn run_summary_export_parses_with_optional_format() {
        let cli = parse(&["run", "--k6", "a.js", "--summary-export", "out.json"]).unwrap();
        match cli.command {
            Commands::Run(args) => {
                assert_eq!(args.summary_export, vec![PathBuf::from("out.json")]);
                assert!(args.summary_format.is_none());
            }
            _ => panic!("expected Run"),
        }

        let cli = parse(&[
            "run",
            "--k6",
            "a.js",
            "--summary-export",
            "summary",
            "--summary-format",
            "md",
        ])
        .unwrap();
        match cli.command {
            Commands::Run(args) => assert_eq!(args.summary_format, Some(SummaryFormat::Md)),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn run_summary_format_without_export_is_rejected() {
        assert!(parse(&["run", "--k6", "a.js", "--summary-format", "md"]).is_err());
    }

    #[test]
    fn run_with_no_target_flag_is_rejected() {
        assert!(parse(&["run"]).is_err());
    }

    #[test]
    fn run_with_two_target_flags_is_rejected() {
        assert!(parse(&["run", "--k6", "a.js", "--locust", "b.py"]).is_err());
        assert!(parse(&["run", "--k6", "a.js", "-f", "t.yaml", "-c", "c.yaml"]).is_err());
    }

    #[test]
    fn run_native_file_without_config_is_rejected() {
        assert!(parse(&["run", "-f", "t.yaml"]).is_err());
    }

    #[test]
    fn run_k6_without_config_is_allowed() {
        assert!(parse(&["run", "--k6", "a.js"]).is_ok());
    }

    #[test]
    fn serve_default_port_is_7999() {
        let cli = parse(&["serve"]).unwrap();
        match cli.command {
            Commands::Serve(args) => assert_eq!(args.port, 7999),
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn serve_custom_port_parses() {
        let cli = parse(&["serve", "--port", "9000"]).unwrap();
        match cli.command {
            Commands::Serve(args) => assert_eq!(args.port, 9000),
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn serve_invalid_port_is_rejected() {
        assert!(parse(&["serve", "--port", "not-a-port"]).is_err());
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(parse(&["frobnicate"]).is_err());
    }
}
