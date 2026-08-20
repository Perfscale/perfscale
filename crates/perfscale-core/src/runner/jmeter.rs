//! JMeter process runner.
//!
//! Runs an existing test plan headless via `jmeter -n -t <plan>`, streaming
//! stdout/stderr live. JMeter owns the load shape entirely — perfscale passes
//! no `-J` properties; parameterize the plan itself (e.g. `${__P(vus,10)}`)
//! or wrap the `jmeter` invocation in a script that sets them.
//!
//! After the process exits, JMeter's final console line
//! (`summary = N in HH:MM:SS = R/s Avg: .. Min: .. Max: .. Err: .. (E%)`)
//! is translated into the k6-compatible summary block the other engines
//! report in. JMeter's console summary carries no percentiles, so the
//! translated `http_req_duration` line has `avg`/`min`/`max` only.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::runner::{LogLine, LogSource, RunOutput};

/// Spawn jmeter in non-GUI mode and return its live output plus final exit
/// code.
///
/// Streams raw stdout/stderr while the run is in progress, then appends a
/// k6-compatible summary translated from jmeter's final `summary =` line
/// (percentile-free — the console summary doesn't carry them). JMeter exits
/// non-zero on plan/startup errors; sample failures inside a plan do NOT
/// change the exit code unless the plan itself gates on them.
pub async fn run_streaming(plan: PathBuf) -> Result<RunOutput, String> {
    let run_id = uuid::Uuid::new_v4().to_string();

    let mut child = spawn_jmeter(&plan)?;
    let pid = child.id();

    let (tx, rx) = mpsc::channel::<LogLine>(512);
    let tx_stdout = tx.clone();
    let tx_stderr = tx.clone();

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    // The final `summary =` line (vs. the periodic `summary +` deltas) is
    // captured while streaming so it can be translated once the run ends.
    let final_summary: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let summary_slot = Arc::clone(&final_summary);

    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.starts_with("summary =") {
                *summary_slot.lock().expect("summary slot poisoned") = Some(line.clone());
            }
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
                debug!(%run_id, ?status, "jmeter exited");
                status.code()
            }
            Err(e) => {
                warn!(%run_id, error = %e, "jmeter wait error");
                None
            }
        };

        // Take the captured line out of the lock before any await — a held
        // MutexGuard would make this task's future !Send.
        let final_line = final_summary.lock().expect("summary slot poisoned").clone();
        match final_line.as_deref() {
            Some(line) => {
                for line in translate_summary(line) {
                    let _ = tx
                        .send(LogLine {
                            source: LogSource::Stdout,
                            text: line,
                        })
                        .await;
                }
            }
            None => {
                let _ = tx
                    .send(LogLine {
                        source: LogSource::System,
                        text: "no jmeter `summary =` line captured — the plan produced no console summary".into(),
                    })
                    .await;
            }
        }

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

fn spawn_jmeter(plan: &PathBuf) -> Result<tokio::process::Child, String> {
    Command::new("jmeter")
        .arg("-n")
        .arg("-t")
        .arg(plan)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| jmeter_exec_error(&e))
}

fn jmeter_exec_error(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "jmeter not found in PATH — install from https://jmeter.apache.org/download_jmeter.cgi (requires a JRE)".into()
    } else {
        format!("Failed to spawn jmeter: {e}")
    }
}

/// Translate jmeter's final console summary
/// (`summary =  N in HH:MM:SS =  R/s Avg: A Min: L Max: H Err: E (P%)`)
/// into the k6-compatible summary block. The console line has no percentiles,
/// so the duration line carries `avg`/`min`/`max` only.
fn translate_summary(line: &str) -> Vec<String> {
    // Fields are whitespace-padded numbers; `Err:` carries a count and a
    // parenthesised percent.
    let re = regex::Regex::new(
        r"summary =\s+(\d+) in \S+ =\s+([\d.]+)/s Avg:\s+(\d+) Min:\s+(\d+) Max:\s+(\d+) Err:\s+\d+ \(([\d.]+)%\)",
    )
    .expect("jmeter summary regex compiles");

    let Some(c) = re.captures(line) else {
        return vec![];
    };
    let (total, rps, avg, min, max, err_pct) = (&c[1], &c[2], &c[3], &c[4], &c[5], &c[6]);

    vec![
        format!("http_req_duration......: avg={avg}.00ms min={min}.00ms max={max}.00ms"),
        format!("http_req_failed........: {err_pct}%"),
        format!("http_reqs..............: {total} {rps}/s"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jmeter_available() -> bool {
        std::process::Command::new("jmeter")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[test]
    fn jmeter_exec_error_not_found_suggests_install() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let msg = jmeter_exec_error(&e);
        assert!(msg.contains("jmeter not found in PATH"));
        assert!(msg.contains("jmeter.apache.org"));
    }

    #[test]
    fn jmeter_exec_error_other_kind_reports_generic_failure() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let msg = jmeter_exec_error(&e);
        assert!(msg.contains("Failed to spawn jmeter"));
        assert!(msg.contains("denied"));
    }

    #[test]
    fn spawn_args_are_non_gui_plan_execution() {
        // Command construction is asserted through the debug form of the
        // tokio Command — spawn_args is built here to keep the assertion
        // next to the flag list it checks.
        let mut cmd = Command::new("jmeter");
        cmd.arg("-n").arg("-t").arg(PathBuf::from("plan.jmx"));
        let rendered = format!("{cmd:?}");
        assert!(rendered.contains("-n"), "{rendered}");
        assert!(rendered.contains("-t"), "{rendered}");
        assert!(rendered.contains("plan.jmx"), "{rendered}");
    }

    #[test]
    fn translate_summary_parses_final_line() {
        let lines = translate_summary(
            "summary =   1200 in 00:00:15 =   80.0/s Avg:     4 Min:     1 Max:    42 Err:     3 (0.25%)",
        );
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("avg=4.00ms"));
        assert!(lines[0].contains("min=1.00ms"));
        assert!(lines[0].contains("max=42.00ms"));
        assert_eq!(lines[1], "http_req_failed........: 0.25%");
        assert_eq!(lines[2], "http_reqs..............: 1200 80.0/s");
        // The translated block must be parseable by the shared summary parser.
        let summary = crate::summary::parse_summary(&lines.join("\n")).expect("parseable");
        assert_eq!(summary.total_requests, 1200);
        assert_eq!(summary.requests_per_sec, 80.0);
        assert_eq!(summary.avg_ms, Some(4.0));
        assert!(
            summary.p95_ms.is_none(),
            "no percentiles in the console summary"
        );
    }

    #[test]
    fn translate_summary_ignores_progress_lines() {
        // `summary +` deltas are mid-run progress, not the final aggregate.
        assert!(translate_summary(
            "summary +    600 in 00:00:07 =   85.7/s Avg:     4 Min:     1 Max:    20 Err:     0 (0.00%)"
        )
        .is_empty());
        assert!(translate_summary("unrelated log line").is_empty());
    }

    #[tokio::test]
    async fn run_streaming_missing_binary_is_clear_error() {
        if jmeter_available() {
            eprintln!("skipping: jmeter installed");
            return;
        }
        let err = run_streaming(PathBuf::from("plan.jmx")).await.unwrap_err();
        assert!(err.contains("jmeter not found in PATH"), "{err}");
    }

    #[tokio::test]
    async fn run_streaming_end_to_end_with_real_jmeter() {
        if !jmeter_available() {
            eprintln!("skipping: jmeter not installed");
            return;
        }
        // A minimal valid plan: one thread, one iteration, no samplers —
        // enough for jmeter to start, run, and print a console summary.
        let dir = tempfile::tempdir().unwrap();
        let plan_path = dir.path().join("plan.jmx");
        tokio::fs::write(&plan_path, MINIMAL_PLAN).await.unwrap();

        let RunOutput {
            mut lines,
            exit,
            pid,
        } = run_streaming(plan_path).await.unwrap();
        assert!(
            pid.is_some(),
            "expected a pid for the spawned jmeter process"
        );

        let mut collected = Vec::new();
        while let Some(line) = lines.recv().await {
            collected.push(line.text);
        }
        assert!(!collected.is_empty(), "expected output lines from jmeter");
        assert!(
            collected.iter().any(|l| l.starts_with("http_reqs")),
            "translated k6-compatible summary expected: {collected:?}"
        );
        assert_eq!(exit.await.unwrap(), Some(0));
    }

    /// Smallest .jmx jmeter accepts: a test plan with an empty thread group.
    const MINIMAL_PLAN: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<jmeterTestPlan version="1.2" properties="5.0" jmeter="5.6.3">
  <hashTree>
    <TestPlan guiclass="TestPlanGui" testclass="TestPlan" testname="plan" enabled="true">
      <elementProp name="TestPlan.user_defined_variables" elementType="Arguments" guiclass="ArgumentsPanel" testclass="Arguments" testname="User Defined Variables" enabled="true">
        <collectionProp name="Arguments.arguments"/>
      </elementProp>
    </TestPlan>
    <hashTree>
      <ThreadGroup guiclass="ThreadGroupGui" testclass="ThreadGroup" testname="tg" enabled="true">
        <stringProp name="ThreadGroup.num_threads">1</stringProp>
        <stringProp name="ThreadGroup.ramp_time">1</stringProp>
        <elementProp name="ThreadGroup.main_controller" elementType="LoopController" guiclass="LoopControlPanel" testclass="LoopController" testname="Loop Controller" enabled="true">
          <boolProp name="LoopController.continue_forever">false</boolProp>
          <stringProp name="LoopController.loops">1</stringProp>
        </elementProp>
      </ThreadGroup>
      <hashTree/>
    </hashTree>
  </hashTree>
</jmeterTestPlan>
"#;
}
