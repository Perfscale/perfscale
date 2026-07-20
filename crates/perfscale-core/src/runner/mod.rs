//! Execution engines: k6, locust, and the native step engine — unified
//! behind a single [`LogLine`] stream so callers (CLI, `perfscale serve`)
//! don't need to care which engine produced the output.

pub mod k6;
pub mod locust;

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::step::{RunConfig, Step, TestDef};

/// A single line of output from any runner.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    pub source: LogSource,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogSource {
    /// Standard output (progress, metrics summary).
    Stdout,
    /// Standard error (warnings, errors).
    Stderr,
    /// Internal messages (start/stop, spawn errors).
    System,
}

/// A running engine: its live output plus, once it finishes, the process
/// exit code.
///
/// `exit` resolves after `lines` closes. `Some(0)` means a clean exit; a
/// non-zero code by itself is NOT necessarily a crash — k6 exits non-zero on
/// failed thresholds and locust on failed requests, both of which are test
/// feedback. Combine the code with whether any metrics arrived to tell a
/// startup crash apart from a completed-but-failing run.
#[derive(Debug)]
pub struct RunOutput {
    pub lines: mpsc::Receiver<LogLine>,
    /// Engine process exit code. `None` if the process was killed by a
    /// signal; the native engine always reports `Some(0)`.
    pub exit: tokio::sync::oneshot::Receiver<Option<i32>>,
    /// OS process ID of the spawned engine binary, while it's running — lets
    /// callers sample its CPU/memory/IO usage. `None` for the native step
    /// engine, which runs in-process rather than as a subprocess.
    pub pid: Option<u32>,
}

/// What to run and with which engine, resolved from CLI flags.
pub enum ExecutionPlan {
    /// `perfscale run --k6 <file.js>`
    K6Script(PathBuf),
    /// `perfscale run --locust <file.py>`
    LocustScript {
        path: PathBuf,
        opts: locust::LocustOpts,
    },
    /// `perfscale run -f <test.yaml> -c <config.yaml>`
    NativeSteps {
        test: TestDef,
        config: RunConfig,
        /// One-time setup steps from the config file's `before:` block.
        before: Vec<Step>,
        /// Static variables from the config file's `variables:` block, exposed
        /// to steps as `${{ vars.* }}`.
        variables: serde_json::Map<String, serde_json::Value>,
        /// Drop per-iteration success output at the source (`--quiet`);
        /// errors and the final metric summary still stream.
        quiet: bool,
    },
}

/// Run `plan` and return its live output stream plus final exit code.
pub async fn execute(plan: ExecutionPlan) -> Result<RunOutput, String> {
    match plan {
        ExecutionPlan::K6Script(path) => {
            let script = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            k6::run_streaming(script).await
        }
        ExecutionPlan::LocustScript { path, opts } => locust::run_streaming(path, opts).await,
        ExecutionPlan::NativeSteps {
            test,
            config,
            before,
            variables,
            quiet,
        } => {
            let (tx, rx) = mpsc::channel(512);
            let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                crate::step::runner::run_native(test.steps, before, config, variables, quiet, tx)
                    .await;
                let _ = exit_tx.send(Some(0));
            });
            Ok(RunOutput {
                lines: rx,
                exit: exit_rx,
                pid: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::step::{RunConfig, Step, TestDef};

    use super::*;

    #[test]
    fn log_line_serde_round_trip() {
        let line = LogLine {
            source: LogSource::Stderr,
            text: "boom".into(),
        };
        let json = serde_json::to_string(&line).unwrap();
        assert_eq!(json, r#"{"source":"stderr","text":"boom"}"#);
        let back: LogLine = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, LogSource::Stderr);
        assert_eq!(back.text, "boom");
    }

    #[test]
    fn log_source_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&LogSource::Stdout).unwrap(),
            "\"stdout\""
        );
        assert_eq!(
            serde_json::to_string(&LogSource::Stderr).unwrap(),
            "\"stderr\""
        );
        assert_eq!(
            serde_json::to_string(&LogSource::System).unwrap(),
            "\"system\""
        );
    }

    #[tokio::test]
    async fn execute_native_steps_runs_the_step_engine() {
        let test = TestDef {
            steps: vec![Step {
                name: None,
                action: "std/log@v1".into(),
                with: Some(serde_json::json!({ "message": "via dispatcher" })),
                check: None,
                outputs: None,
            }],
        };
        let config = RunConfig {
            vus: 1,
            duration: "1s".into(),
            ..Default::default()
        };

        let RunOutput {
            mut lines, exit, ..
        } = execute(ExecutionPlan::NativeSteps {
            test,
            config,
            before: Vec::new(),
            variables: serde_json::Map::new(),
            quiet: false,
        })
        .await
        .unwrap();
        let mut collected = Vec::new();
        while let Some(line) = lines.recv().await {
            collected.push(line.text);
        }
        assert!(collected.iter().any(|l| l == "via dispatcher"));
        assert_eq!(exit.await.unwrap(), Some(0), "native engine always exits 0");
    }

    #[tokio::test]
    async fn execute_k6_script_reads_file_and_runs() {
        if std::process::Command::new("k6")
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("skipping: k6 not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("script.js");
        tokio::fs::write(&script_path, "export default function() {}")
            .await
            .unwrap();

        let RunOutput {
            mut lines,
            exit,
            pid,
        } = execute(ExecutionPlan::K6Script(script_path)).await.unwrap();
        assert!(pid.is_some(), "k6 runner must report a pid");
        let mut saw_any_line = false;
        while lines.recv().await.is_some() {
            saw_any_line = true;
        }
        assert!(saw_any_line);
        assert_eq!(exit.await.unwrap(), Some(0));
    }

    #[tokio::test]
    async fn execute_k6_invalid_script_reports_nonzero_exit() {
        if std::process::Command::new("k6")
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("skipping: k6 not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("broken.js");
        tokio::fs::write(&script_path, "this is not javascript {{{")
            .await
            .unwrap();

        let RunOutput {
            mut lines, exit, ..
        } = execute(ExecutionPlan::K6Script(script_path)).await.unwrap();
        while lines.recv().await.is_some() {}
        let code = exit.await.unwrap();
        assert!(
            matches!(code, Some(c) if c != 0),
            "expected non-zero exit, got {code:?}"
        );
    }

    #[tokio::test]
    async fn execute_k6_script_missing_file_errors_before_spawning() {
        let missing = std::env::temp_dir().join("perfscale-does-not-exist.js");
        let err = execute(ExecutionPlan::K6Script(missing)).await.unwrap_err();
        assert!(err.contains("failed to read"));
    }
}
