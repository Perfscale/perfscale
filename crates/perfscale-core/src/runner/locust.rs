//! Locust process runner.
//!
//! Runs a locustfile headless via `locust -f <file> --headless ...`, streams
//! stdout/stderr as it runs, then parses locust's `--csv` stats output into a
//! k6-compatible summary so all three engines report in the same shape.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::runner::{LogLine, LogSource, RunOutput};
use crate::step::RunConfig;

/// Load options for a locust run — mirrors [`RunConfig`] but uses locust's
/// own vocabulary (`users`/`spawn_rate`/`host`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocustOpts {
    pub users: u32,
    pub spawn_rate: u32,
    pub duration: String,
    pub host: Option<String>,
}

impl Default for LocustOpts {
    fn default() -> Self {
        Self {
            users: 1,
            spawn_rate: 1,
            duration: "1m".into(),
            host: None,
        }
    }
}

impl LocustOpts {
    /// Build options from a generic [`RunConfig`], spawning all users at once.
    pub fn from_run_config(cfg: &RunConfig, host: Option<String>) -> Self {
        Self {
            users: cfg.vus.max(1),
            spawn_rate: cfg.vus.max(1),
            duration: cfg.duration.clone(),
            host,
        }
    }
}

/// Spawn locust in headless mode and return its live output plus final
/// exit code.
///
/// Streams raw stdout/stderr while the run is in progress, then appends a
/// k6-compatible summary parsed from locust's `--csv` stats file once the
/// process exits. Note locust exits non-zero when any request failed — a
/// non-zero code with a summary present is test feedback, not a crash.
pub async fn run_streaming(script: PathBuf, opts: LocustOpts) -> Result<RunOutput, String> {
    let run_id = Uuid::new_v4().to_string();
    let csv_prefix = std::env::temp_dir().join(format!("perfscale-locust-{run_id}"));

    let mut child = spawn_locust(&script, &opts, &csv_prefix)?;
    let pid = child.id();

    let (tx, rx) = mpsc::channel::<LogLine>(512);
    let tx_stdout = tx.clone();
    let tx_stderr = tx.clone();

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

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

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let code = match child.wait().await {
            Ok(status) => {
                debug!(%run_id, ?status, "locust exited");
                status.code()
            }
            Err(e) => {
                warn!(%run_id, error = %e, "locust wait error");
                None
            }
        };

        match parse_csv_summary(&csv_prefix).await {
            Ok(lines) => {
                for line in lines {
                    let _ = tx
                        .send(LogLine {
                            source: LogSource::Stdout,
                            text: line,
                        })
                        .await;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(LogLine {
                        source: LogSource::System,
                        text: format!("failed to read locust stats: {e}"),
                    })
                    .await;
            }
        }

        cleanup_csv(&csv_prefix).await;
        // tx dropped here → channel closes
        let _ = exit_tx.send(code);
    });

    Ok(RunOutput {
        lines: rx,
        exit: exit_rx,
        pid,
    })
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn spawn_locust(
    script: &Path,
    opts: &LocustOpts,
    csv_prefix: &Path,
) -> Result<tokio::process::Child, String> {
    let mut cmd = Command::new("locust");
    cmd.arg("-f")
        .arg(script)
        .arg("--headless")
        .arg("-u")
        .arg(opts.users.to_string())
        .arg("-r")
        .arg(opts.spawn_rate.to_string())
        .arg("-t")
        .arg(&opts.duration)
        .arg("--csv")
        .arg(csv_prefix);

    if let Some(host) = &opts.host {
        cmd.arg("--host").arg(host);
    }

    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| locust_exec_error(&e))
}

fn locust_exec_error(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "locust not found in PATH — install with `pip install locust` (https://docs.locust.io/en/stable/installation.html)"
            .into()
    } else {
        format!("Failed to spawn locust: {e}")
    }
}

/// Read `{prefix}_stats.csv` (as written by locust's `--csv` flag) and build
/// a k6-compatible summary from the `Aggregated` row locust always writes
/// last. Public so callers driving locust directly can reuse the exact same
/// parsing this runner uses, for an apples-to-apples comparison.
pub async fn parse_csv_summary(csv_prefix: &Path) -> Result<Vec<String>, String> {
    let stats_path = PathBuf::from(format!("{}_stats.csv", csv_prefix.display()));
    let content = tokio::fs::read_to_string(&stats_path)
        .await
        .map_err(|e| format!("{}: {e}", stats_path.display()))?;

    let mut reader = csv::Reader::from_reader(content.as_bytes());
    let headers = reader.headers().map_err(|e| e.to_string())?.clone();
    let name_idx = header_idx(&headers, "Name").ok_or("stats.csv missing 'Name' column")?;

    let mut aggregated: Option<csv::StringRecord> = None;
    for record in reader.records() {
        let record = record.map_err(|e| e.to_string())?;
        if record.get(name_idx) == Some("Aggregated") {
            aggregated = Some(record);
        }
    }

    let row = aggregated.ok_or("no 'Aggregated' row in locust stats.csv")?;
    let col = |name: &str| -> f64 {
        header_idx(&headers, name)
            .and_then(|i| row.get(i))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };

    let total = col("Request Count");
    let failures = col("Failure Count");
    let avg = col("Average Response Time");
    let min = col("Min Response Time");
    let max = col("Max Response Time");
    let p50 = col("50%");
    let p90 = col("90%");
    let p95 = col("95%");
    let p99 = col("99%");
    let rps = col("Requests/s");
    let err_pct = if total > 0.0 {
        failures / total * 100.0
    } else {
        0.0
    };

    Ok(vec![
        format!(
            "http_req_duration......: avg={avg:.2}ms p(50)={p50:.0}ms p(90)={p90:.0}ms p(95)={p95:.0}ms p(99)={p99:.0}ms min={min:.0}ms max={max:.0}ms"
        ),
        format!("http_req_failed........: {err_pct:.2}%"),
        format!("http_reqs..............: {total:.0} {rps:.2}/s"),
    ])
}

fn header_idx(headers: &csv::StringRecord, name: &str) -> Option<usize> {
    headers.iter().position(|h| h == name)
}

async fn cleanup_csv(csv_prefix: &Path) {
    for suffix in [
        "_stats.csv",
        "_stats_history.csv",
        "_failures.csv",
        "_exceptions.csv",
    ] {
        let path = PathBuf::from(format!("{}{suffix}", csv_prefix.display()));
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parse_csv_summary_parses_aggregated_row() {
        let dir = std::env::temp_dir().join(format!("perfscale-locust-test-{}", Uuid::new_v4()));
        let csv_content = "Type,Name,Request Count,Failure Count,Median Response Time,Average Response Time,Min Response Time,Max Response Time,Average Content Size,Requests/s,Failures/s,50%,66%,75%,80%,90%,95%,98%,99%,99.9%,99.99%,100%\n\
GET,/,100,2,40,42.5,10,120,512,10.5,0.2,40,45,50,55,60,68,75,85,110,118,120\n\
None,Aggregated,100,2,40,42.5,10,120,512,10.5,0.2,40,45,50,55,60,68,75,85,110,118,120\n";
        tokio::fs::write(format!("{}_stats.csv", dir.display()), csv_content)
            .await
            .unwrap();

        let lines = parse_csv_summary(&dir).await.unwrap();
        assert!(lines[0].contains("avg=42.50ms"));
        assert!(lines[0].contains("p(95)=68ms"));
        assert!(lines[1].contains("2.00%"));
        assert!(lines[2].contains("100 10.50/s"));

        cleanup_csv(&dir).await;
    }

    #[tokio::test]
    async fn parse_csv_summary_missing_file_errors() {
        let dir = std::env::temp_dir().join(format!("perfscale-locust-missing-{}", Uuid::new_v4()));
        let err = parse_csv_summary(&dir).await.unwrap_err();
        assert!(err.contains("_stats.csv"));
    }

    #[tokio::test]
    async fn parse_csv_summary_without_aggregated_row_errors() {
        let dir = std::env::temp_dir().join(format!("perfscale-locust-noagg-{}", Uuid::new_v4()));
        let csv_content = "Type,Name,Request Count,Failure Count,Median Response Time,Average Response Time,Min Response Time,Max Response Time,Average Content Size,Requests/s,Failures/s,50%,66%,75%,80%,90%,95%,98%,99%,99.9%,99.99%,100%\n\
GET,/,100,2,40,42.5,10,120,512,10.5,0.2,40,45,50,55,60,68,75,85,110,118,120\n";
        tokio::fs::write(format!("{}_stats.csv", dir.display()), csv_content)
            .await
            .unwrap();

        let err = parse_csv_summary(&dir).await.unwrap_err();
        assert!(err.contains("Aggregated"));

        cleanup_csv(&dir).await;
    }

    #[tokio::test]
    async fn parse_csv_summary_zero_requests_has_zero_error_rate() {
        let dir = std::env::temp_dir().join(format!("perfscale-locust-zero-{}", Uuid::new_v4()));
        let csv_content = "Type,Name,Request Count,Failure Count,Median Response Time,Average Response Time,Min Response Time,Max Response Time,Average Content Size,Requests/s,Failures/s,50%,66%,75%,80%,90%,95%,98%,99%,99.9%,99.99%,100%\n\
None,Aggregated,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0\n";
        tokio::fs::write(format!("{}_stats.csv", dir.display()), csv_content)
            .await
            .unwrap();

        let lines = parse_csv_summary(&dir).await.unwrap();
        assert!(lines[1].contains("0.00%"));

        cleanup_csv(&dir).await;
    }

    #[test]
    fn locust_opts_default_is_one_user() {
        let opts = LocustOpts::default();
        assert_eq!(opts.users, 1);
        assert_eq!(opts.spawn_rate, 1);
        assert_eq!(opts.duration, "1m");
        assert!(opts.host.is_none());
    }

    #[test]
    fn locust_opts_from_run_config_maps_vus_to_users_and_spawn_rate() {
        let cfg = RunConfig {
            vus: 20,
            duration: "5m".into(),
            ..Default::default()
        };
        let opts = LocustOpts::from_run_config(&cfg, Some("https://example.com".into()));
        assert_eq!(opts.users, 20);
        assert_eq!(opts.spawn_rate, 20);
        assert_eq!(opts.duration, "5m");
        assert_eq!(opts.host.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn locust_opts_from_run_config_clamps_zero_vus_to_one() {
        let cfg = RunConfig {
            vus: 0,
            duration: "1m".into(),
            ..Default::default()
        };
        let opts = LocustOpts::from_run_config(&cfg, None);
        assert_eq!(opts.users, 1);
        assert_eq!(opts.spawn_rate, 1);
    }

    #[test]
    fn locust_exec_error_not_found_suggests_pip_install() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let msg = locust_exec_error(&e);
        assert!(msg.contains("pip install locust"));
    }

    #[test]
    fn locust_exec_error_other_kind_reports_generic_failure() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let msg = locust_exec_error(&e);
        assert!(msg.contains("Failed to spawn locust"));
    }

    fn locust_available() -> bool {
        std::process::Command::new("locust")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[tokio::test]
    async fn run_streaming_end_to_end_with_real_locust() {
        if !locust_available() {
            eprintln!("skipping: locust not installed");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("locustfile.py");
        tokio::fs::write(
            &script_path,
            "from locust import HttpUser, task\nclass U(HttpUser):\n    @task\n    def t(self):\n        self.client.get('/')\n",
        )
        .await
        .unwrap();

        let opts = LocustOpts {
            users: 1,
            spawn_rate: 1,
            duration: "1s".into(),
            host: Some("http://localhost:1".into()),
        };
        let RunOutput {
            mut lines,
            exit,
            pid,
        } = run_streaming(script_path, opts).await.unwrap();
        assert!(
            pid.is_some(),
            "expected a pid for the spawned locust process"
        );

        let mut saw_any_line = false;
        while lines.recv().await.is_some() {
            saw_any_line = true;
        }
        assert!(saw_any_line, "expected at least one log line from locust");
        // Requests all failed (host unreachable) → locust exits non-zero, but
        // the run itself completed and the exit code must be reported.
        assert!(exit.await.unwrap().is_some());
    }
}
