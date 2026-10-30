//! One-shot host-mediated process execution for step components (RFC 009,
//! `process` capability → the `perfscale:step/host-exec` import).
//!
//! Deliberately NOT a WASI interface and NOT `std/child_process@v1`: that
//! action family is a long-running supervisor (restart policy, readiness
//! gates, output mirroring); this is a bounded request/response spawn. The
//! child runs with its own process group, an explicit replacement
//! environment (nothing inherited), piped and output-capped stdio, and a
//! wall-clock timeout that kills the whole process tree on expiry.

use std::io::Read;
use std::time::{Duration, Instant};

/// Per-stream capture cap for stdout/stderr. Past the cap the bytes are
/// drained and discarded (never spilled to disk): the child must not block
/// on a full pipe, and the host must not buffer unboundedly.
pub(crate) const EXEC_OUTPUT_CAP: usize = 1 << 20; // 1 MiB

/// Default budget when the request carries no `timeout_ms`.
pub(crate) const EXEC_DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling for an explicit `timeout_ms` — a step may not park a child
/// forever.
pub(crate) const EXEC_MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// One `host-exec.exec` call, converted from the WIT record by the caller.
#[derive(Debug, Default)]
pub(crate) struct ExecRequest {
    pub executable: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub stdin: Option<String>,
    pub timeout_ms: Option<u64>,
}

/// What the guest gets back.
#[derive(Debug)]
pub(crate) struct ExecOutcome {
    /// Process exit code; -1 when the child died by signal.
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run the request to completion (blocking — the caller's guest-call path
/// already sits on a blocking-tolerant thread; see `library::wasm`).
pub(crate) fn run(req: ExecRequest) -> Result<ExecOutcome, String> {
    if req.executable.is_empty() {
        return Err("host-exec: `executable` must not be empty".into());
    }
    for (k, _) in &req.env {
        if k.is_empty() || k.contains('=') {
            return Err(format!("host-exec: invalid environment key '{k}'"));
        }
    }
    let timeout = match req.timeout_ms {
        None | Some(0) => EXEC_DEFAULT_TIMEOUT,
        Some(ms) => Duration::from_millis(ms).min(EXEC_MAX_TIMEOUT),
    };

    let mut cmd = std::process::Command::new(&req.executable);
    cmd.args(&req.argv)
        // Replacement environment: nothing inherited from the engine
        // process (no ambient HOME/PATH/secrets leak into the child).
        .env_clear()
        .envs(req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(if req.stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    // Own process group, so a timeout can kill the whole tree the child
    // spawned — mirroring `std/child_process@v1`'s kill semantics.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("host-exec: failed to spawn '{}': {e}", req.executable))?;

    if let Some(input) = req.stdin {
        if let Some(mut pipe) = child.stdin.take() {
            // Writes can block on a child that never reads; a thread keeps the
            // wait loop responsive. The join below bounds its lifetime to the
            // child's.
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = pipe.write_all(input.as_bytes());
            });
        }
    }

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let out_handle = std::thread::spawn(move || read_capped(stdout));
    let err_handle = std::thread::spawn(move || read_capped(stderr));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    kill_tree(&mut child);
                    let _ = child.wait();
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    return Err(format!(
                        "host-exec: '{}' exceeded its {}s timeout — the process tree was killed",
                        req.executable,
                        timeout.as_secs_f64()
                    ));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => {
                kill_tree(&mut child);
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(format!(
                    "host-exec: failed waiting on '{}': {e}",
                    req.executable
                ));
            }
        }
    };

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok(ExecOutcome {
        exit_code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Kill the child's whole process group on unix (the child is its group
/// leader, so this cannot hit perfscale itself); the direct child elsewhere.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Safety: `killpg` with a mapped-in signal; errors (ESRCH — the group
        // already exited) are fine.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
        }
        let _ = child.kill();
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

/// Read the stream to EOF, retaining at most [`EXEC_OUTPUT_CAP`] bytes.
/// Reading continues past the cap so the child never blocks on a full pipe.
fn read_capped(mut pipe: impl Read) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = EXEC_OUTPUT_CAP.saturating_sub(out.len());
                if room > 0 {
                    out.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh_req(script: &str) -> ExecRequest {
        ExecRequest {
            executable: "sh".into(),
            argv: vec!["-c".into(), script.into()],
            ..Default::default()
        }
    }

    #[test]
    fn runs_and_captures() {
        let out = run(sh_req("echo out; echo err >&2; exit 3")).unwrap();
        assert_eq!(out.exit_code, 3);
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
    }

    #[test]
    fn env_is_a_replacement_not_inherited() {
        let out = run(ExecRequest {
            env: vec![("ONLY_THIS".into(), "yes".into())],
            ..sh_req("echo \"${ONLY_THIS:-no}-${HOME:-nohome}\"")
        })
        .unwrap();
        assert_eq!(out.stdout, "yes-nohome\n");
    }

    #[test]
    fn stdin_is_piped() {
        let out = run(ExecRequest {
            stdin: Some("hello\n".into()),
            ..sh_req("cat")
        })
        .unwrap();
        assert_eq!(out.stdout, "hello\n");
    }

    #[test]
    fn timeout_kills_the_process_tree() {
        let started = Instant::now();
        let err = run(ExecRequest {
            timeout_ms: Some(150),
            ..sh_req("sleep 30 & sleep 30")
        })
        .unwrap_err();
        assert!(err.contains("timeout"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the wait must end at the timeout, not the child's natural death"
        );
    }

    #[test]
    fn output_is_capped_but_the_child_finishes() {
        let script = format!("yes | head -c {}", EXEC_OUTPUT_CAP * 3);
        let out = run(sh_req(&script)).unwrap();
        assert_eq!(out.stdout.len(), EXEC_OUTPUT_CAP);
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn bad_requests_are_actionable() {
        let err = run(ExecRequest::default()).unwrap_err();
        assert!(err.contains("executable"), "{err}");
        let err = run(sh_req("true").into_with_bad_env()).unwrap_err();
        assert!(err.contains("environment key"), "{err}");
        let err = run(ExecRequest {
            executable: "definitely-not-a-real-binary-xyz".into(),
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.contains("failed to spawn"), "{err}");
    }

    impl ExecRequest {
        fn into_with_bad_env(mut self) -> Self {
            self.env = vec![("BAD=KEY".into(), "v".into())];
            self
        }
    }
}
