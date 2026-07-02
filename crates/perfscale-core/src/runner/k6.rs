//! k6 process runner.
//!
//! Writes the given script to a temp file, spawns `k6 run`, and delivers
//! output via one of two delivery modes:
//!
//! | Function         | Returns       | Use for      |
//! |------------------|---------------|--------------|
//! | `run_streaming`  | [`RunOutput`] | live logs    |
//! | `run_oneshot`    | `RunResult`   | final result |

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::models::RunResult;
use crate::runner::{LogLine, LogSource, RunOutput};

// ---------------------------------------------------------------------------
// Streaming run
// ---------------------------------------------------------------------------

/// Spawn k6 and return its live output plus final exit code.
///
/// The line channel closes when the k6 process exits; `exit` resolves right
/// after with the process status code.
pub async fn run_streaming(script: String) -> Result<RunOutput, String> {
    let (script_path, run_id) = write_script(&script)?;

    let mut child = spawn_k6(&script_path)?;

    let (tx, rx) = mpsc::channel::<LogLine>(512);
    let tx_stdout = tx.clone();
    let tx_stderr = tx.clone();

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    // Stream stdout.
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx_stdout
                .send(LogLine {
                    source: LogSource::Stdout,
                    text: line,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Stream stderr.
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx_stderr
                .send(LogLine {
                    source: LogSource::Stderr,
                    text: line,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Wait for process, forward its exit code, then clean up the script file.
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let path_clone = script_path.clone();
    tokio::spawn(async move {
        let code = match child.wait().await {
            Ok(status) => {
                debug!(%run_id, ?status, "k6 exited");
                status.code()
            }
            Err(e) => {
                warn!(%run_id, error = %e, "k6 wait error");
                None
            }
        };
        // tx is dropped here → channel closes → stream ends.
        let _ = tokio::fs::remove_file(&path_clone).await;
        let _ = exit_tx.send(code);
    });

    Ok(RunOutput {
        lines: rx,
        exit: exit_rx,
    })
}

// ---------------------------------------------------------------------------
// Oneshot run
// ---------------------------------------------------------------------------

/// Run k6 to completion and return the full output.
pub async fn run_oneshot(script: String) -> Result<RunResult, String> {
    let (script_path, run_id) = write_script(&script)?;

    debug!(%run_id, "Starting k6 (oneshot)");

    let output = Command::new("k6")
        .arg("run")
        .arg("--no-color")
        .arg(&script_path)
        .output()
        .await
        .map_err(|e| k6_exec_error(&e))?;

    let _ = tokio::fs::remove_file(&script_path).await;

    let exit_code = output.status.code().unwrap_or(-1);
    let success = output.status.success();

    debug!(%run_id, exit_code, "k6 finished (oneshot)");

    Ok(RunResult {
        exit_code,
        success,
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        script,
    })
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn spawn_k6(script_path: &PathBuf) -> Result<tokio::process::Child, String> {
    Command::new("k6")
        .arg("run")
        .arg("--no-color")
        .arg(script_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| k6_exec_error(&e))
}

/// Write the script to a stable temp path (UUID-named to avoid collisions).
fn write_script(script: &str) -> Result<(PathBuf, String), String> {
    let run_id = Uuid::new_v4().to_string();
    let path = std::env::temp_dir().join(format!("perfscale-{run_id}.js"));

    std::fs::write(&path, script)
        .map_err(|e| format!("Failed to write k6 script to {}: {e}", path.display()))?;

    debug!(run_id, path = %path.display(), "Script written");
    Ok((path, run_id))
}

fn k6_exec_error(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "k6 not found in PATH — install from https://k6.io/docs/get-started/installation/".into()
    } else {
        format!("Failed to spawn k6: {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k6_available() -> bool {
        std::process::Command::new("k6")
            .arg("version")
            .output()
            .is_ok()
    }

    #[test]
    fn k6_exec_error_not_found_suggests_install() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let msg = k6_exec_error(&e);
        assert!(msg.contains("k6 not found in PATH"));
        assert!(msg.contains("k6.io"));
    }

    #[test]
    fn k6_exec_error_other_kind_reports_generic_failure() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let msg = k6_exec_error(&e);
        assert!(msg.contains("Failed to spawn k6"));
        assert!(msg.contains("denied"));
    }

    #[test]
    fn write_script_creates_readable_temp_file() {
        let (path, run_id) = write_script("export default function(){}").unwrap();
        assert!(path.exists());
        assert!(path.to_string_lossy().contains(&run_id));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "export default function(){}"
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn run_oneshot_success_reports_exit_code_zero() {
        if !k6_available() {
            eprintln!("skipping: k6 not installed");
            return;
        }
        let script = "export default function() {}".to_string();
        let result = run_oneshot(script.clone()).await.unwrap();
        assert!(result.success);
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.script, script);
    }

    #[tokio::test]
    async fn run_oneshot_invalid_script_reports_failure() {
        if !k6_available() {
            eprintln!("skipping: k6 not installed");
            return;
        }
        let result = run_oneshot("this is not valid javascript {{{".to_string())
            .await
            .unwrap();
        assert!(!result.success);
        assert_ne!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn run_streaming_success_yields_lines_and_clean_exit() {
        if !k6_available() {
            eprintln!("skipping: k6 not installed");
            return;
        }
        let RunOutput { mut lines, exit } =
            run_streaming("export default function() {}".to_string())
                .await
                .unwrap();
        let mut saw_any_line = false;
        while lines.recv().await.is_some() {
            saw_any_line = true;
        }
        assert!(saw_any_line, "expected at least one log line from k6");
        assert_eq!(exit.await.unwrap(), Some(0));
    }

    #[tokio::test]
    async fn run_streaming_broken_script_reports_nonzero_exit() {
        if !k6_available() {
            eprintln!("skipping: k6 not installed");
            return;
        }
        let RunOutput { mut lines, exit } = run_streaming("not javascript {{{".to_string())
            .await
            .unwrap();
        while lines.recv().await.is_some() {}
        let code = exit.await.unwrap();
        assert!(
            matches!(code, Some(c) if c != 0),
            "expected non-zero exit, got {code:?}"
        );
    }
}
