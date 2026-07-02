//! Execution engines: k6, locust, and the native step engine — unified
//! behind a single [`LogLine`] stream so callers (CLI, `perfscale serve`)
//! don't need to care which engine produced the output.

pub mod k6;
pub mod locust;

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::step::{RunConfig, TestDef};

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
    NativeSteps { test: TestDef, config: RunConfig },
}

/// Run `plan` and return a channel streaming its output as it happens.
pub async fn execute(plan: ExecutionPlan) -> Result<mpsc::Receiver<LogLine>, String> {
    match plan {
        ExecutionPlan::K6Script(path) => {
            let script = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            k6::run_streaming(script).await
        }
        ExecutionPlan::LocustScript { path, opts } => locust::run_streaming(path, opts).await,
        ExecutionPlan::NativeSteps { test, config } => {
            let (tx, rx) = mpsc::channel(512);
            tokio::spawn(crate::step::runner::run_steps(test.steps, config, tx));
            Ok(rx)
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
        };

        let mut rx = execute(ExecutionPlan::NativeSteps { test, config })
            .await
            .unwrap();
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line.text);
        }
        assert!(lines.iter().any(|l| l == "via dispatcher"));
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

        let mut rx = execute(ExecutionPlan::K6Script(script_path)).await.unwrap();
        let mut saw_any_line = false;
        while rx.recv().await.is_some() {
            saw_any_line = true;
        }
        assert!(saw_any_line);
    }

    #[tokio::test]
    async fn execute_k6_script_missing_file_errors_before_spawning() {
        let missing = std::env::temp_dir().join("perfscale-does-not-exist.js");
        let err = execute(ExecutionPlan::K6Script(missing)).await.unwrap_err();
        assert!(err.contains("failed to read"));
    }
}
