//! Managed child processes for `std/child_process@v1` / `std/kill_process@v1`.
//!
//! A [`ManagedProcess`] wraps a spawned child with everything a load test
//! needs around it:
//!
//! - line-captured stdout/stderr into bounded tail buffers ([`RingBuf`]),
//!   mirrored to the run's live log with a `{step}: ` prefix (same shape as
//!   the k6 runner's output);
//! - a `waitUntil` readiness gate (contains/matches/port_open matchers);
//! - a supervisor task applying the restart policy (`never` / `on-failure` /
//!   `always` + `max_restarts` + `backoff_ms`);
//! - signal-based termination with grace-period escalation to SIGKILL, by
//!   process (and process group — every child is spawned as its own group
//!   leader, so a tree kill cannot hit perfscale itself).
//!
//! All managed processes live in a run-scoped [`ProcessRegistry`], keyed by
//! the spawning step's name (and its `outputs` name — the runner aliases the
//! entry, so `kill_process` accepts either). Whatever is still alive at the
//! end of a run is stopped by [`ProcessRegistry::shutdown_all`].
//!
//! Portability: POSIX signals and process groups are unix-only (`libc`);
//! other platforms degrade to `Child::kill` on the direct child without a
//! tree kill.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::runner::{LogLine, LogSource};
use crate::step::parse_duration_secs;

/// Default captured-output tail size per stream (`buffer_kb` overrides).
pub(crate) const DEFAULT_BUFFER_CAP: usize = 64 * 1024;
/// How often the supervisor polls the child for exit. `Child::wait` would
/// hold the child handle for the whole wait, leaving `kill()` no way to reach
/// it on platforms without POSIX signals — a short poll keeps the handle
/// shareable and bounds exit-detection latency to ~50ms.
const SUPERVISOR_POLL: Duration = Duration::from_millis(50);
/// Poll interval for readiness gates and exit waits.
const WAIT_POLL: Duration = Duration::from_millis(25);

// ---------------------------------------------------------------------------
// Restart policy
// ---------------------------------------------------------------------------

/// When to restart a managed process that exited on its own.
///
/// Parsed from the `restart` parameter; the enum is deliberately explicit so
/// future strategies (e.g. backoff multipliers) slot in without a
/// stringly-typed refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Restart {
    /// Never restart (default).
    #[default]
    Never,
    /// Restart only when the exit was unsuccessful (non-zero code or signal).
    OnFailure,
    /// Restart on every exit, clean or not.
    Always,
}

impl Restart {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "never" => Some(Self::Never),
            "on-failure" => Some(Self::OnFailure),
            "always" => Some(Self::Always),
            _ => None,
        }
    }

    /// `code` is `None` when the child was reaped after dying to a signal —
    /// that counts as a failure for `on-failure`.
    fn should_restart(self, code: Option<i32>) -> bool {
        match self {
            Self::Never => false,
            Self::OnFailure => code != Some(0),
            Self::Always => true,
        }
    }
}

// ---------------------------------------------------------------------------
// Process spec
// ---------------------------------------------------------------------------

/// Static description of a process to manage, parsed from action params.
#[derive(Debug)]
pub(crate) struct ProcSpec {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    /// `Some(0)` auto-assigns a free port and exports it as `PORT`.
    pub port: Option<u16>,
    pub restart: Restart,
    pub max_restarts: u32,
    pub backoff: Duration,
    /// Captured stdout/stderr tail size in bytes (per stream).
    pub buffer_cap: usize,
}

// ---------------------------------------------------------------------------
// RingBuf — bounded tail capture
// ---------------------------------------------------------------------------

/// Bounded tail buffer for one output stream: keeps at most `cap` bytes,
/// dropping from the front on a char boundary so the content stays valid
/// UTF-8. Long-running servers would otherwise grow memory without bound.
///
/// Public only so `benches/` can exercise the hot path (`push`) — not part
/// of the supported API surface.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct RingBuf {
    buf: String,
    cap: usize,
}

impl RingBuf {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: String::new(),
            cap,
        }
    }

    pub fn push(&mut self, s: &str) {
        self.buf.push_str(s);
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.cap;
            // The first char boundary at or after `excess` — never split a
            // multibyte codepoint.
            let mut cut = excess;
            while !self.buf.is_char_boundary(cut) {
                cut += 1;
            }
            self.buf.drain(..cut);
        }
    }

    pub fn as_str(&self) -> &str {
        &self.buf
    }
}

/// The last `max` bytes of `s` (on a char boundary) — for error messages that
/// quote a stream tail.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

// ---------------------------------------------------------------------------
// waitUntil — readiness gate
// ---------------------------------------------------------------------------

/// A parsed `waitUntil` readiness gate: all matchers must hold before the
/// spawning step reports success.
///
/// Public only so `benches/` can exercise parsing and matcher evaluation —
/// not part of the supported API surface.
#[doc(hidden)]
#[derive(Debug)]
pub struct WaitUntil {
    matchers: Vec<Matcher>,
    timeout: Duration,
    /// `on_timeout: continue` — the step logs the miss but succeeds anyway.
    pub(crate) on_timeout_continue: bool,
}

#[derive(Debug)]
enum Matcher {
    StdoutContains(String),
    StderrContains(String),
    StdoutMatches(regex::Regex),
    StderrMatches(regex::Regex),
    /// `0` resolves to the step's own (possibly auto-assigned) `port`.
    PortOpen(u16),
}

impl WaitUntil {
    /// Parse the object form (`{ stdout_contains, stderr_contains,
    /// stdout_matches, stderr_matches, port_open, timeout, on_timeout }`) or
    /// the string form (`'contains(stdout, "...")'`, `'matches(stderr,
    /// "re")'`, `'port_open(8080)'`).
    pub fn parse(v: &Value) -> Result<Self, String> {
        match v {
            Value::String(s) => Ok(Self {
                matchers: vec![parse_matcher_expr(s)?],
                timeout: Duration::from_secs(30),
                on_timeout_continue: false,
            }),
            Value::Object(o) => {
                let mut matchers = Vec::new();
                if let Some(s) = o.get("stdout_contains").and_then(Value::as_str) {
                    matchers.push(Matcher::StdoutContains(s.to_string()));
                }
                if let Some(s) = o.get("stderr_contains").and_then(Value::as_str) {
                    matchers.push(Matcher::StderrContains(s.to_string()));
                }
                if let Some(s) = o.get("stdout_matches").and_then(Value::as_str) {
                    let re = regex::Regex::new(s)
                        .map_err(|e| format!("waitUntil.stdout_matches: invalid regex: {e}"))?;
                    matchers.push(Matcher::StdoutMatches(re));
                }
                if let Some(s) = o.get("stderr_matches").and_then(Value::as_str) {
                    let re = regex::Regex::new(s)
                        .map_err(|e| format!("waitUntil.stderr_matches: invalid regex: {e}"))?;
                    matchers.push(Matcher::StderrMatches(re));
                }
                if let Some(p) = o.get("port_open") {
                    let Some(p) = p.as_u64().filter(|p| *p <= 65535) else {
                        return Err("waitUntil.port_open must be an integer 0..=65535".into());
                    };
                    matchers.push(Matcher::PortOpen(p as u16));
                }
                if matchers.is_empty() {
                    return Err("waitUntil names no matcher — use stdout_contains, stderr_contains, stdout_matches, stderr_matches or port_open".into());
                }
                let timeout = match o.get("timeout").and_then(Value::as_str) {
                    Some(s) => Duration::from_secs(parse_duration_secs(s)),
                    None => Duration::from_secs(30),
                };
                let on_timeout_continue = match o.get("on_timeout").and_then(Value::as_str) {
                    None | Some("fail") => false,
                    Some("continue") => true,
                    Some(other) => {
                        return Err(format!(
                            "waitUntil.on_timeout must be \"fail\" or \"continue\", got '{other}'"
                        ))
                    }
                };
                Ok(Self {
                    matchers,
                    timeout,
                    on_timeout_continue,
                })
            }
            _ => Err(
                "waitUntil must be an object or a string like 'contains(stdout, \"...\")'".into(),
            ),
        }
    }

    /// True when every stream matcher holds against the given captured
    /// contents. `port_open` is not a buffer matcher and is ignored here —
    /// its TCP probe lives in `ManagedProcess::wait_until`.
    #[doc(hidden)]
    pub fn matches_buffers(&self, stdout: &str, stderr: &str) -> bool {
        self.matchers.iter().all(|m| match m {
            Matcher::StdoutContains(n) => stdout.contains(n.as_str()),
            Matcher::StderrContains(n) => stderr.contains(n.as_str()),
            Matcher::StdoutMatches(re) => re.is_match(stdout),
            Matcher::StderrMatches(re) => re.is_match(stderr),
            Matcher::PortOpen(_) => true,
        })
    }
}

/// Parse the string form of one matcher — a miniature of `parse_call` from
/// `generate.rs`: `name(args)` with a comma split.
fn parse_matcher_expr(s: &str) -> Result<Matcher, String> {
    let s = s.trim();
    let open = s
        .find('(')
        .ok_or_else(|| format!("invalid waitUntil expression '{s}'"))?;
    if !s.ends_with(')') {
        return Err(format!("invalid waitUntil expression '{s}'"));
    }
    let name = s[..open].trim();
    let inner = s[open + 1..s.len() - 1].trim();
    match name {
        "contains" | "matches" => {
            let (stream, needle) = inner.split_once(',').ok_or_else(|| {
                format!(
                    "waitUntil '{name}' wants (stream, \"text\") — e.g. {name}(stdout, \"ready\")"
                )
            })?;
            let needle = unquote(needle.trim())?;
            match (stream.trim(), name) {
                ("stdout", "contains") => Ok(Matcher::StdoutContains(needle)),
                ("stderr", "contains") => Ok(Matcher::StderrContains(needle)),
                ("stdout", "matches") => regex::Regex::new(&needle)
                    .map(Matcher::StdoutMatches)
                    .map_err(|e| format!("waitUntil matches(): invalid regex: {e}")),
                ("stderr", "matches") => regex::Regex::new(&needle)
                    .map(Matcher::StderrMatches)
                    .map_err(|e| format!("waitUntil matches(): invalid regex: {e}")),
                (other, _) => Err(format!(
                    "waitUntil stream must be stdout or stderr, got '{other}'"
                )),
            }
        }
        "port_open" => inner
            .parse::<u16>()
            .map(Matcher::PortOpen)
            .map_err(|_| format!("waitUntil port_open wants a port number, got '{inner}'")),
        other => Err(format!(
            "unknown waitUntil matcher '{other}' — use contains, matches or port_open"
        )),
    }
}

/// Strip surrounding double quotes from a matcher argument, unescaping `\"`
/// and `\\` on the way.
fn unquote(s: &str) -> Result<String, String> {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return Err(format!(
            "waitUntil argument must be double-quoted, got '{s}'"
        ));
    };
    Ok(inner.replace("\\\"", "\"").replace("\\\\", "\\"))
}

/// One TCP connect attempt against loopback — the probe behind the
/// `port_open` matcher. Refused connections fail fast, so polling is cheap.
async fn tcp_connectable(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}

// ---------------------------------------------------------------------------
// ManagedProcess
// ---------------------------------------------------------------------------

/// Lifecycle state of a [`ManagedProcess`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcStatus {
    /// The child is alive (or in the backoff window before a restart).
    Running,
    /// A stop was requested (`kill_process` / shutdown): the supervisor must
    /// not restart the child.
    Stopping,
    /// Final exit: the child is gone and the supervisor gave up (no restart
    /// left, or stopped on purpose). Carries the exit code, `None` when the
    /// child died to a signal.
    Exited(Option<i32>),
}

/// Mutable live state of a managed process (updated by the supervisor and
/// reader tasks).
#[derive(Debug)]
struct ProcInner {
    status: ProcStatus,
    pid: Option<u32>,
    pgid: Option<u32>,
    restart_count: u32,
    stdout: RingBuf,
    stderr: RingBuf,
}

/// Outcome of a kill operation — the `std/kill_process@v1` output value.
#[derive(Debug)]
pub(crate) struct KillOutcome {
    pub pid: Option<u32>,
    pub signal: String,
    pub exit_code: Option<i32>,
    pub waited_ms: u64,
}

/// One managed child process: live state, captured output, and the tasks
/// (readers + supervisor) that keep them current. Shared as `Arc` — the
/// spawning step returns immediately, but the process must stay manageable
/// for the rest of the run.
#[derive(Debug)]
pub(crate) struct ManagedProcess {
    /// Step name — the log-line prefix and the registry key.
    name: String,
    spec: ProcSpec,
    /// Extra env derived at spawn (the auto-assigned `PORT`).
    extra_env: Vec<(String, String)>,
    /// The port this process answers on, when the step declared one
    /// (`port: 0` resolves to the auto-assigned value).
    port: Option<u16>,
    inner: Mutex<ProcInner>,
    /// Current child handle. A `std` mutex: every critical section is a
    /// single syscall (try_wait / start_kill), never held across an `.await`.
    child: Mutex<Option<Child>>,
    /// Live log stream of the run (prefixed lines), when it has one.
    log_tx: Option<mpsc::Sender<LogLine>>,
}

impl ManagedProcess {
    /// Spawn the child and start its reader + supervisor tasks.
    pub(crate) fn spawn(
        spec: ProcSpec,
        name: String,
        log_tx: Option<mpsc::Sender<LogLine>>,
    ) -> Result<Arc<Self>, String> {
        // `port: 0` asks for an auto-assigned free port, exported to the
        // child as PORT (standard PaaS convention). The bind-then-release
        // inherently races with another process grabbing the port before the
        // child binds it — accepted and documented.
        let (port, extra_env) = match spec.port {
            Some(0) => {
                let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
                    .map_err(|e| format!("cannot allocate a free port: {e}"))?;
                let port = listener
                    .local_addr()
                    .map_err(|e| format!("cannot allocate a free port: {e}"))?
                    .port();
                drop(listener);
                (Some(port), vec![("PORT".to_string(), port.to_string())])
            }
            other => (other, Vec::new()),
        };

        let mut child = build_command(&spec, &extra_env)
            .spawn()
            .map_err(|e| format!("failed to spawn '{}': {e}", spec.command))?;
        let pid = child.id();
        let cap = spec.buffer_cap;

        let mp = Arc::new(Self {
            name,
            spec,
            extra_env,
            port,
            inner: Mutex::new(ProcInner {
                status: ProcStatus::Running,
                pid,
                pgid: pgid_of(pid),
                restart_count: 0,
                stdout: RingBuf::new(cap),
                stderr: RingBuf::new(cap),
            }),
            child: Mutex::new(None),
            log_tx,
        });
        spawn_reader(
            &mp,
            child.stdout.take().expect("stdout is piped"),
            LogSource::Stdout,
        );
        spawn_reader(
            &mp,
            child.stderr.take().expect("stderr is piped"),
            LogSource::Stderr,
        );
        *mp.child.lock().unwrap() = Some(child);
        mp.spawn_supervisor();
        Ok(mp)
    }

    /// Point-in-time outputs snapshot for the step's `outputs` value. `pid` /
    /// `pgid` / `restart_count` move on restarts — `std/kill_process@v1` by
    /// `name` reads the live registry entry instead of this snapshot.
    pub(crate) fn snapshot(&self) -> Value {
        let g = self.inner.lock().unwrap();
        let mut out = json!({
            "pid": g.pid,
            "ppid": std::process::id(),
            "pgid": g.pgid,
            "stdout": g.stdout.as_str(),
            "stderr": g.stderr.as_str(),
            "restart_count": g.restart_count,
        });
        if let Some(port) = self.port {
            out["port"] = json!(port);
        }
        out
    }

    /// Block until every matcher of the readiness gate holds, the process
    /// exits, or the gate times out.
    pub(crate) async fn wait_until(&self, w: &WaitUntil) -> Result<(), String> {
        let deadline = Instant::now() + w.timeout;
        loop {
            // One lock per poll covers all buffer matchers; the port probes
            // below run lock-free.
            let mut ready = {
                let g = self.inner.lock().unwrap();
                w.matches_buffers(g.stdout.as_str(), g.stderr.as_str())
            };
            if ready {
                for m in &w.matchers {
                    let Matcher::PortOpen(p) = m else {
                        continue;
                    };
                    // `port_open: 0` probes the step's own port.
                    let port = if *p == 0 { self.port } else { Some(*p) };
                    let Some(port) = port else {
                        return Err("waitUntil.port_open: 0 needs the step's own `port`".into());
                    };
                    if !tcp_connectable(port).await {
                        ready = false;
                        break;
                    }
                }
            }
            if ready {
                return Ok(());
            }

            {
                let g = self.inner.lock().unwrap();
                match g.status {
                    ProcStatus::Exited(code) => {
                        return Err(format!(
                            "process exited (code {code:?}) before becoming ready — stderr tail: {}",
                            tail(g.stderr.as_str(), 300).trim()
                        ));
                    }
                    ProcStatus::Stopping => {
                        return Err("process is being stopped".into());
                    }
                    ProcStatus::Running => {}
                }
            }

            if Instant::now() >= deadline {
                return Err(format!("no readiness after {}s", w.timeout.as_secs()));
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// Signal the process and wait for it to die: `signal` first, SIGKILL
    /// after `grace`. With `tree`, the whole process group is signalled.
    pub(crate) async fn kill(
        &self,
        signal: &str,
        grace: Duration,
        tree: bool,
    ) -> Result<KillOutcome, String> {
        self.begin_kill(signal, tree)?;
        Ok(self.finish_kill(signal, grace, tree).await)
    }

    /// First half of [`ManagedProcess::kill`]: mark the process `Stopping`
    /// (so the supervisor will not restart it) and deliver the signal.
    /// Idempotent — an already-exited process is a no-op.
    fn begin_kill(&self, signal: &str, tree: bool) -> Result<(), String> {
        // Validate the signal name before touching any state, so a typo does
        // not leave an unstoppable process behind.
        #[cfg(unix)]
        let sig = parse_signal(signal).ok_or_else(|| unknown_signal(signal))?;

        let (pid, pgid) = {
            let mut g = self.inner.lock().unwrap();
            if matches!(g.status, ProcStatus::Exited(_)) {
                return Ok(());
            }
            g.status = ProcStatus::Stopping;
            (g.pid, g.pgid)
        };

        #[cfg(unix)]
        if let Some(pid) = pid {
            send_signal(pid, if tree { pgid } else { None }, sig);
        }
        #[cfg(not(unix))]
        {
            // No POSIX signals or process groups: terminate the direct child;
            // `tree` is unsupported there (documented).
            let _ = (signal, tree, pgid);
            if let Some(child) = self.child.lock().unwrap().as_mut() {
                let _ = child.start_kill();
            }
        }
        Ok(())
    }

    /// Second half of [`ManagedProcess::kill`]: wait out the grace period,
    /// escalate to SIGKILL, and report.
    async fn finish_kill(&self, signal: &str, grace: Duration, tree: bool) -> KillOutcome {
        let start = Instant::now();
        let pid = self.inner.lock().unwrap().pid;
        if self.wait_until_exited(grace).await.is_none() {
            // Survived the grace period — escalate.
            #[cfg(unix)]
            {
                let pgid = self.inner.lock().unwrap().pgid;
                if let Some(pid) = pid {
                    send_signal(pid, if tree { pgid } else { None }, libc::SIGKILL);
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tree;
                if let Some(child) = self.child.lock().unwrap().as_mut() {
                    let _ = child.start_kill();
                }
            }
            // Reaping is the supervisor's job; give it a bounded moment so a
            // wedged child can never hang the run.
            let _ = self.wait_until_exited(Duration::from_secs(5)).await;
        }
        let exit_code = match self.inner.lock().unwrap().status {
            ProcStatus::Exited(code) => code,
            _ => None,
        };
        KillOutcome {
            pid,
            signal: signal.to_string(),
            exit_code,
            waited_ms: start.elapsed().as_millis() as u64,
        }
    }

    /// Wait until the status becomes `Exited` (poll-based; `None` on
    /// timeout). Returns the exit code — itself `None` for signal deaths.
    pub(crate) async fn wait_until_exited(&self, max: Duration) -> Option<Option<i32>> {
        let deadline = Instant::now() + max;
        loop {
            {
                let g = self.inner.lock().unwrap();
                if let ProcStatus::Exited(code) = g.status {
                    return Some(code);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// Emit a system log line into the run's live stream, prefixed like the
    /// process's own output lines.
    async fn log_sys(&self, text: &str) {
        if let Some(tx) = &self.log_tx {
            let _ = tx
                .send(LogLine {
                    source: LogSource::System,
                    text: format!("{}: {text}", self.name),
                })
                .await;
        }
    }

    /// Supervisor task: watch the child, record its exit, and apply the
    /// restart policy. Runs for the whole process lifetime; returns once the
    /// status becomes `Exited` for good.
    fn spawn_supervisor(self: &Arc<Self>) {
        let mp = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let exited = {
                    let mut slot = mp.child.lock().unwrap();
                    match slot.as_mut() {
                        Some(child) => match child.try_wait() {
                            Ok(Some(status)) => {
                                *slot = None;
                                Some(status.code())
                            }
                            Ok(None) => None,
                            Err(_) => {
                                *slot = None;
                                Some(None)
                            }
                        },
                        None => Some(None),
                    }
                };

                let Some(code) = exited else {
                    tokio::time::sleep(SUPERVISOR_POLL).await;
                    continue;
                };

                // The child is gone. Restart, or call it a final exit.
                let restart = {
                    let mut g = mp.inner.lock().unwrap();
                    if g.status == ProcStatus::Stopping {
                        g.status = ProcStatus::Exited(code);
                        false
                    } else if mp.spec.restart.should_restart(code)
                        && g.restart_count < mp.spec.max_restarts
                    {
                        g.restart_count += 1;
                        true
                    } else {
                        g.status = ProcStatus::Exited(code);
                        false
                    }
                };
                if !restart {
                    return;
                }

                mp.log_sys(&format!(
                    "process exited (code {code:?}), restarting ({}/{})",
                    mp.inner.lock().unwrap().restart_count,
                    mp.spec.max_restarts,
                ))
                .await;
                tokio::time::sleep(mp.spec.backoff).await;

                // A kill may have landed during the backoff.
                {
                    let mut g = mp.inner.lock().unwrap();
                    if g.status == ProcStatus::Stopping {
                        g.status = ProcStatus::Exited(code);
                        return;
                    }
                }

                match build_command(&mp.spec, &mp.extra_env).spawn() {
                    Ok(mut child) => {
                        let pid = child.id();
                        {
                            let mut g = mp.inner.lock().unwrap();
                            g.pid = pid;
                            g.pgid = pgid_of(pid);
                        }
                        spawn_reader(
                            &mp,
                            child.stdout.take().expect("stdout is piped"),
                            LogSource::Stdout,
                        );
                        spawn_reader(
                            &mp,
                            child.stderr.take().expect("stderr is piped"),
                            LogSource::Stderr,
                        );
                        *mp.child.lock().unwrap() = Some(child);
                    }
                    Err(e) => {
                        {
                            let mut g = mp.inner.lock().unwrap();
                            g.status = ProcStatus::Exited(code);
                            g.stderr.push(&format!("<respawn failed: {e}>\n"));
                        }
                        mp.log_sys(&format!("respawn failed: {e}")).await;
                        return;
                    }
                }
            }
        });
    }
}

/// Build the tokio command for a (re)spawn.
fn build_command(spec: &ProcSpec, extra_env: &[(String, String)]) -> Command {
    let mut cmd = Command::new(&spec.command);
    cmd.args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in spec.env.iter().chain(extra_env.iter()) {
        cmd.env(k, v);
    }
    // Own process group (pgid = pid) so a tree kill signals exactly the
    // child's group and can never hit perfscale's own.
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// The child's process group id: with `process_group(0)` the child leads its
/// own group, so pgid == pid. Other platforms have no POSIX process groups.
fn pgid_of(pid: Option<u32>) -> Option<u32> {
    #[cfg(unix)]
    {
        pid
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

/// Stream one pipe line by line into the ring buffer and, when the run has a
/// live log channel, out to the user with a `{step}: ` prefix (the same shape
/// the k6 runner uses).
fn spawn_reader<R>(mp: &Arc<ManagedProcess>, pipe: R, source: LogSource)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mp = Arc::clone(mp);
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            {
                let mut g = mp.inner.lock().unwrap();
                let buf = match source {
                    LogSource::Stdout => &mut g.stdout,
                    _ => &mut g.stderr,
                };
                buf.push(&line);
                buf.push("\n");
            }
            if let Some(tx) = &mp.log_tx {
                let log = LogLine {
                    source,
                    text: format!("{}: {line}", mp.name),
                };
                if tx.send(log).await.is_err() {
                    break;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Signals (unix)
// ---------------------------------------------------------------------------

/// Map a signal name (`TERM` / `SIGTERM`, case-insensitive) to its number.
/// Kept deliberately small — these are the signals meaningful for lifecycle
/// management.
#[cfg(unix)]
fn parse_signal(name: &str) -> Option<libc::c_int> {
    let upper = name.to_ascii_uppercase();
    let short = upper.strip_prefix("SIG").unwrap_or(&upper);
    Some(match short {
        "TERM" => libc::SIGTERM,
        "KILL" => libc::SIGKILL,
        "INT" => libc::SIGINT,
        "HUP" => libc::SIGHUP,
        "QUIT" => libc::SIGQUIT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        _ => return None,
    })
}

#[cfg(unix)]
fn unknown_signal(name: &str) -> String {
    format!("unknown signal '{name}' — try TERM, KILL, INT, HUP, QUIT, USR1, USR2")
}

/// Send `sig` to the process group `pgid` when given (tree kill), falling
/// back to the single process when the group is already gone or not given.
#[cfg(unix)]
fn send_signal(pid: u32, pgid: Option<u32>, sig: libc::c_int) {
    // Safety: `kill`/`killpg` with a mapped-in signal number. Errors (ESRCH,
    // EPERM) are deliberately ignored — the target may have exited between
    // our check and the signal; that is the nature of best-effort process
    // management.
    unsafe {
        if let Some(g) = pgid {
            if libc::killpg(g as libc::pid_t, sig) == 0 {
                return;
            }
        }
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// `kill(pid, 0)` liveness probe: alive when the call succeeds or fails with
/// EPERM (exists, owned by someone else); ESRCH means gone.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    unsafe {
        if libc::kill(pid as libc::pid_t, 0) == 0 {
            true
        } else {
            std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
}

/// Best-effort kill of a process that is NOT in the registry (`pid:` in
/// `std/kill_process@v1`): signal it, wait for it to disappear (probing with
/// `kill(pid, 0)`), escalate to SIGKILL after `grace`. No registry state is
/// touched; the exit code stays unknown.
#[cfg(unix)]
pub(crate) async fn kill_raw_pid(
    pid: u32,
    signal: &str,
    grace: Duration,
    tree: bool,
) -> Result<KillOutcome, String> {
    let sig = parse_signal(signal).ok_or_else(|| unknown_signal(signal))?;
    let start = Instant::now();
    // Tree mode assumes the raw pid is also its group id — true for processes
    // started via child_process (`process_group(0)`), best-effort otherwise.
    send_signal(pid, if tree { Some(pid) } else { None }, sig);
    loop {
        if !pid_alive(pid) {
            break;
        }
        if start.elapsed() >= grace {
            send_signal(pid, if tree { Some(pid) } else { None }, libc::SIGKILL);
            // Give the kernel a bounded moment to tear the process down.
            let deadline = Instant::now() + Duration::from_secs(2);
            while pid_alive(pid) && Instant::now() < deadline {
                tokio::time::sleep(WAIT_POLL).await;
            }
            break;
        }
        tokio::time::sleep(WAIT_POLL).await;
    }
    Ok(KillOutcome {
        pid: Some(pid),
        signal: signal.to_string(),
        exit_code: None,
        waited_ms: start.elapsed().as_millis() as u64,
    })
}

// ---------------------------------------------------------------------------
// ProcessRegistry
// ---------------------------------------------------------------------------

/// Run-scoped registry of managed child processes.
///
/// Keyed by name: the spawning step's name, plus its `outputs` name when the
/// runner aliases the entry while storing outputs — so `kill_process`
/// accepts either. One process may therefore appear under two keys; lookups
/// clone the `Arc`, and [`ProcessRegistry::shutdown_all`] de-duplicates by
/// pointer.
#[derive(Debug, Default)]
pub struct ProcessRegistry {
    procs: Mutex<HashMap<String, Arc<ManagedProcess>>>,
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a freshly spawned process under `name`.
    pub(crate) fn insert(&self, name: &str, mp: &Arc<ManagedProcess>) {
        self.procs
            .lock()
            .unwrap()
            .insert(name.to_string(), Arc::clone(mp));
    }

    /// Point a second name at an already-registered process (step name →
    /// `outputs` name). No-op when `from` is unknown or the names match.
    pub(crate) fn alias(&self, from: &str, to: &str) {
        if from == to {
            return;
        }
        let mut map = self.procs.lock().unwrap();
        if let Some(mp) = map.get(from).cloned() {
            map.insert(to.to_string(), mp);
        }
    }

    /// Look up a live process by either of its names.
    pub(crate) fn get(&self, name: &str) -> Option<Arc<ManagedProcess>> {
        self.procs.lock().unwrap().get(name).cloned()
    }

    /// Stop everything still registered: TERM the lot (so graceful stops
    /// overlap), then wait + escalate per process. Called at the end of every
    /// native run — normal finish, failed `before`, or interrupted run alike.
    pub(crate) async fn shutdown_all(&self) {
        let procs: Vec<Arc<ManagedProcess>> = {
            let map = self.procs.lock().unwrap();
            let mut seen = std::collections::HashSet::new();
            map.values()
                .filter(|p| seen.insert(Arc::as_ptr(p)))
                .cloned()
                .collect()
        };
        for p in &procs {
            let _ = p.begin_kill("TERM", true);
        }
        for p in &procs {
            let _ = p.finish_kill("TERM", Duration::from_secs(5), true).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh_spec(script: &str) -> ProcSpec {
        ProcSpec {
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: Vec::new(),
            cwd: None,
            port: None,
            restart: Restart::Never,
            max_restarts: 3,
            backoff: Duration::from_millis(50),
            buffer_cap: DEFAULT_BUFFER_CAP,
        }
    }

    // -----------------------------------------------------------------
    // RingBuf
    // -----------------------------------------------------------------

    #[test]
    fn ring_buf_keeps_only_the_last_cap_bytes() {
        let mut b = RingBuf::new(10);
        b.push("hello ");
        b.push("world");
        assert_eq!(b.as_str(), "ello world");
        b.push("!");
        assert_eq!(b.as_str(), "llo world!");
    }

    #[test]
    fn ring_buf_trims_on_a_char_boundary() {
        let mut b = RingBuf::new(5);
        b.push("😀😀"); // 8 bytes, two 4-byte codepoints
                        // Only the last emoji fits whole — the trim must not split one.
        assert_eq!(b.as_str(), "😀");
    }

    #[test]
    fn ring_buf_zero_cap_discards_everything() {
        let mut b = RingBuf::new(0);
        b.push("data");
        assert_eq!(b.as_str(), "");
    }

    // -----------------------------------------------------------------
    // Restart policy
    // -----------------------------------------------------------------

    #[test]
    fn restart_policy_matrix() {
        assert!(!Restart::Never.should_restart(Some(1)));
        assert!(!Restart::OnFailure.should_restart(Some(0)));
        assert!(Restart::OnFailure.should_restart(Some(1)));
        // Signal death (no exit code) counts as failure.
        assert!(Restart::OnFailure.should_restart(None));
        assert!(Restart::Always.should_restart(Some(0)));
        assert!(Restart::Always.should_restart(Some(1)));
    }

    // -----------------------------------------------------------------
    // waitUntil parsing
    // -----------------------------------------------------------------

    #[test]
    fn wait_until_parses_full_object_form() {
        let w = WaitUntil::parse(&json!({
            "stdout_contains": "ready",
            "stderr_contains": "warn",
            "stdout_matches": "listening on \\d+",
            "stderr_matches": "err\\d+",
            "port_open": 8080,
            "timeout": "5s",
            "on_timeout": "continue",
        }))
        .unwrap();
        assert_eq!(w.matchers.len(), 5);
        assert_eq!(w.timeout, Duration::from_secs(5));
        assert!(w.on_timeout_continue);
    }

    #[test]
    fn wait_until_object_defaults_are_30s_and_fail() {
        let w = WaitUntil::parse(&json!({ "stdout_contains": "x" })).unwrap();
        assert_eq!(w.timeout, Duration::from_secs(30));
        assert!(!w.on_timeout_continue);
    }

    #[test]
    fn wait_until_parses_string_forms() {
        let w = WaitUntil::parse(&json!("contains(stdout, \"ready\")")).unwrap();
        assert!(matches!(&w.matchers[0], Matcher::StdoutContains(s) if s == "ready"));

        let w = WaitUntil::parse(&json!("contains(stderr, \"boom\")")).unwrap();
        assert!(matches!(&w.matchers[0], Matcher::StderrContains(s) if s == "boom"));

        let w = WaitUntil::parse(&json!("matches(stdout, \"re\\d+\")")).unwrap();
        match &w.matchers[0] {
            Matcher::StdoutMatches(re) => assert!(re.is_match("re123")),
            other => panic!("expected StdoutMatches, got {other:?}"),
        }

        let w = WaitUntil::parse(&json!("matches(stderr, \"re\")")).unwrap();
        assert!(matches!(&w.matchers[0], Matcher::StderrMatches(_)));

        let w = WaitUntil::parse(&json!("port_open(8080)")).unwrap();
        assert!(matches!(&w.matchers[0], Matcher::PortOpen(8080)));
    }

    #[test]
    fn wait_until_string_form_unescapes_quotes() {
        let w = WaitUntil::parse(&json!("contains(stdout, \"a \\\"quoted\\\" word\")")).unwrap();
        assert!(matches!(&w.matchers[0], Matcher::StdoutContains(s) if s == "a \"quoted\" word"));
    }

    #[test]
    fn wait_until_rejects_invalid_specs() {
        // Not an expression at all.
        assert!(WaitUntil::parse(&json!("not a matcher")).is_err());
        // Object without any matcher.
        assert!(WaitUntil::parse(&json!({})).is_err());
        // Bad on_timeout value.
        assert!(
            WaitUntil::parse(&json!({ "stdout_contains": "x", "on_timeout": "maybe" })).is_err()
        );
        // Bad regex.
        assert!(WaitUntil::parse(&json!({ "stdout_matches": "(" })).is_err());
        assert!(WaitUntil::parse(&json!("matches(stdout, \"(\")")).is_err());
        // Unquoted string argument.
        assert!(WaitUntil::parse(&json!("contains(stdout, ready)")).is_err());
        // Unknown matcher name / stream.
        assert!(WaitUntil::parse(&json!("starts_with(stdout, \"x\")")).is_err());
        assert!(WaitUntil::parse(&json!("contains(stdin, \"x\")")).is_err());
        // Non-string non-object.
        assert!(WaitUntil::parse(&json!(42)).is_err());
        // Port out of range / not a number.
        assert!(WaitUntil::parse(&json!({ "port_open": 70000 })).is_err());
        assert!(WaitUntil::parse(&json!("port_open(http)")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn parses_signal_names_with_optional_sig_prefix() {
        assert_eq!(parse_signal("TERM"), Some(libc::SIGTERM));
        assert_eq!(parse_signal("sigkill"), Some(libc::SIGKILL));
        assert_eq!(parse_signal("HUP"), Some(libc::SIGHUP));
        assert_eq!(parse_signal("SIGUSR1"), Some(libc::SIGUSR1));
        assert_eq!(parse_signal("WAT"), None);
    }

    // -----------------------------------------------------------------
    // Live process management (unix — real `sh`/`sleep` children)
    // -----------------------------------------------------------------

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_captures_output_and_kills_cleanly() {
        let mp = ManagedProcess::spawn(sh_spec("echo ready; sleep 60"), "t".into(), None).unwrap();
        let w = WaitUntil::parse(&json!({ "stdout_contains": "ready", "timeout": "5s" })).unwrap();
        mp.wait_until(&w).await.unwrap();

        let snap = mp.snapshot();
        assert!(snap["pid"].as_u64().unwrap() > 0);
        assert_eq!(snap["ppid"], json!(std::process::id()));
        // process_group(0): the child leads its own group.
        assert_eq!(snap["pgid"], snap["pid"]);
        assert!(snap["stdout"].as_str().unwrap().contains("ready"));
        assert_eq!(snap["restart_count"], 0);
        // No `port` key when the step declared none.
        assert!(snap.get("port").is_none());

        let outcome = mp.kill("TERM", Duration::from_secs(5), true).await.unwrap();
        assert_eq!(outcome.pid, snap["pid"].as_u64().map(|p| p as u32));
        assert!(matches!(
            mp.inner.lock().unwrap().status,
            ProcStatus::Exited(_)
        ));
        // Killing an already-dead process is a no-op, not an error.
        mp.kill("TERM", Duration::from_secs(1), true).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_lines_stream_to_the_run_log_with_step_prefix() {
        let (tx, mut rx) = mpsc::channel(16);
        let mp = ManagedProcess::spawn(
            sh_spec("echo hello-log; echo oops-log >&2; sleep 60"),
            "logger".into(),
            Some(tx),
        )
        .unwrap();
        let mut texts = Vec::new();
        for _ in 0..2 {
            let line = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .expect("a log line");
            texts.push((line.source, line.text));
        }
        assert!(texts.contains(&(LogSource::Stdout, "logger: hello-log".into())));
        assert!(texts.contains(&(LogSource::Stderr, "logger: oops-log".into())));
        mp.kill("KILL", Duration::from_secs(2), true).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_until_port_open_probes_tcp() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mp = ManagedProcess::spawn(sh_spec("sleep 60"), "probe".into(), None).unwrap();

        let w = WaitUntil::parse(&json!({ "port_open": port, "timeout": "5s" })).unwrap();
        mp.wait_until(&w).await.unwrap();

        // Nothing listens on port 1 — this gate must time out.
        let w = WaitUntil::parse(&json!({ "port_open": 1, "timeout": "1s" })).unwrap();
        assert!(mp.wait_until(&w).await.is_err());

        mp.kill("KILL", Duration::from_secs(2), true).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_until_times_out_when_never_ready() {
        let mp = ManagedProcess::spawn(sh_spec("sleep 60"), "slow".into(), None).unwrap();
        let w = WaitUntil::parse(&json!({ "stdout_contains": "never", "timeout": "1s" })).unwrap();
        let start = Instant::now();
        let msg = mp.wait_until(&w).await.unwrap_err();
        assert!(msg.contains("no readiness after 1s"), "{msg}");
        assert!(start.elapsed() >= Duration::from_secs(1));
        mp.kill("KILL", Duration::from_secs(2), true).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_until_reports_early_exit_with_stderr_tail() {
        let mp =
            ManagedProcess::spawn(sh_spec("echo boom >&2; exit 3"), "early".into(), None).unwrap();
        let w = WaitUntil::parse(&json!({ "stdout_contains": "never-comes", "timeout": "10s" }))
            .unwrap();
        let msg = mp.wait_until(&w).await.unwrap_err();
        assert!(msg.contains("exited"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_on_failure_respects_max_restarts() {
        let mut spec = sh_spec("exit 1");
        spec.restart = Restart::OnFailure;
        spec.max_restarts = 2;
        spec.backoff = Duration::from_millis(20);
        let mp = ManagedProcess::spawn(spec, "flaky".into(), None).unwrap();

        // The supervisor gives up after 2 restarts → final exit with code 1.
        let code = mp
            .wait_until_exited(Duration::from_secs(10))
            .await
            .expect("supervisor never gave up");
        assert_eq!(code, Some(1));
        let g = mp.inner.lock().unwrap();
        assert_eq!(g.restart_count, 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_on_failure_ignores_clean_exits() {
        let mut spec = sh_spec("exit 0");
        spec.restart = Restart::OnFailure;
        spec.backoff = Duration::from_millis(20);
        let mp = ManagedProcess::spawn(spec, "ok".into(), None).unwrap();

        let code = mp
            .wait_until_exited(Duration::from_secs(10))
            .await
            .expect("process never exited");
        assert_eq!(code, Some(0));
        assert_eq!(mp.inner.lock().unwrap().restart_count, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_always_restarts_even_on_success() {
        let mut spec = sh_spec("exit 0");
        spec.restart = Restart::Always;
        spec.max_restarts = 1;
        spec.backoff = Duration::from_millis(20);
        let mp = ManagedProcess::spawn(spec, "once-more".into(), None).unwrap();

        let _ = mp
            .wait_until_exited(Duration::from_secs(10))
            .await
            .expect("supervisor never gave up");
        assert_eq!(mp.inner.lock().unwrap().restart_count, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_targets_the_current_pid_after_a_restart() {
        let registry = ProcessRegistry::new();
        let mut spec = sh_spec("sleep 1");
        spec.restart = Restart::Always;
        spec.max_restarts = 100;
        spec.backoff = Duration::from_millis(20);
        let mp = ManagedProcess::spawn(spec, "svc".into(), None).unwrap();
        registry.insert("svc", &mp);
        let first_pid = mp.inner.lock().unwrap().pid.unwrap();

        // `sleep 1` exits cleanly, `always` respawns it → pid changes. The
        // count flips before the backoff, the pid after the respawn — wait
        // for both so we never sample the stale pid mid-restart.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (count, pid) = {
                let g = mp.inner.lock().unwrap();
                (g.restart_count, g.pid)
            };
            if count >= 1 && pid != Some(first_pid) {
                break;
            }
            assert!(Instant::now() < deadline, "process never restarted");
            tokio::time::sleep(WAIT_POLL).await;
        }
        let current_pid = mp.inner.lock().unwrap().pid.unwrap();
        assert_ne!(current_pid, first_pid);

        // A kill through the registry must hit the *current* pid, not the
        // one the step's outputs snapshot captured at spawn time.
        let mp = registry.get("svc").unwrap();
        mp.kill("KILL", Duration::from_secs(5), true).await.unwrap();
        assert!(matches!(
            mp.inner.lock().unwrap().status,
            ProcStatus::Exited(_)
        ));
        assert!(!pid_alive(current_pid));
        assert!(!pid_alive(first_pid));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tree_kill_takes_the_whole_process_group() {
        let mp = ManagedProcess::spawn(
            sh_spec("sleep 1000 & echo grandchild=$!; wait"),
            "tree".into(),
            None,
        )
        .unwrap();
        let w = WaitUntil::parse(&json!({ "stdout_contains": "grandchild=", "timeout": "5s" }))
            .unwrap();
        mp.wait_until(&w).await.unwrap();

        let stdout = mp.snapshot()["stdout"].as_str().unwrap().to_string();
        let gpid: u32 = stdout
            .split("grandchild=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
            .expect("grandchild pid captured");
        assert!(pid_alive(gpid));

        mp.kill("TERM", Duration::from_secs(5), true).await.unwrap();

        // The grandchild lived in the child's process group → TERM reached it.
        let deadline = Instant::now() + Duration::from_secs(2);
        while pid_alive(gpid) && Instant::now() < deadline {
            tokio::time::sleep(WAIT_POLL).await;
        }
        assert!(!pid_alive(gpid));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn port_zero_auto_assigns_and_exports_port_env() {
        let mut spec = sh_spec("echo port=$PORT; sleep 60");
        spec.port = Some(0);
        let mp = ManagedProcess::spawn(spec, "auto".into(), None).unwrap();
        let port = mp.port.expect("auto-assigned port");
        assert!(port > 0);

        // The child saw the assigned port via the PORT env var.
        let w = WaitUntil::parse(
            &json!({ "stdout_contains": format!("port={port}"), "timeout": "5s" }),
        )
        .unwrap();
        mp.wait_until(&w).await.unwrap();
        assert_eq!(mp.snapshot()["port"], json!(port));

        // `port_open: 0` resolves to the step's own assigned port.
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let w = WaitUntil::parse(&json!({ "port_open": 0, "timeout": "5s" })).unwrap();
        mp.wait_until(&w).await.unwrap();
        drop(listener);

        mp.kill("KILL", Duration::from_secs(2), true).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_missing_binary_reports_spawn_error() {
        let spec = ProcSpec {
            command: "perfscale-no-such-binary-xyz".into(),
            ..sh_spec("")
        };
        let msg = ManagedProcess::spawn(spec, "nope".into(), None).unwrap_err();
        assert!(msg.contains("failed to spawn"), "{msg}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn raw_pid_kill_is_best_effort() {
        // Direct exec (no sh) so the pid *is* the sleeping process.
        let spec = ProcSpec {
            command: "sleep".into(),
            args: vec!["300".into()],
            ..sh_spec("")
        };
        let mp = ManagedProcess::spawn(spec, "raw".into(), None).unwrap();
        let pid = mp.inner.lock().unwrap().pid.unwrap();

        let outcome = kill_raw_pid(pid, "TERM", Duration::from_secs(5), false)
            .await
            .unwrap();
        assert_eq!(outcome.pid, Some(pid));
        assert!(!pid_alive(pid));

        // The supervisor notices the death and records a final exit — with
        // the default `never` policy there is no respawn.
        let _ = mp.wait_until_exited(Duration::from_secs(5)).await;
        assert!(matches!(
            mp.inner.lock().unwrap().status,
            ProcStatus::Exited(_)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unknown_signal_is_rejected_before_any_state_change() {
        let mp = ManagedProcess::spawn(sh_spec("sleep 60"), "sig".into(), None).unwrap();
        let msg = mp
            .kill("WAT", Duration::from_secs(1), true)
            .await
            .unwrap_err();
        assert!(msg.contains("unknown signal"), "{msg}");
        // Still Running — a typo must not mark the process Stopping.
        assert_eq!(mp.inner.lock().unwrap().status, ProcStatus::Running);
        mp.kill("KILL", Duration::from_secs(2), true).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_all_stops_everything_exactly_once() {
        let registry = ProcessRegistry::new();
        let sleep = |name: &str| {
            let spec = ProcSpec {
                command: "sleep".into(),
                args: vec!["300".into()],
                ..sh_spec("")
            };
            ManagedProcess::spawn(spec, name.into(), None).unwrap()
        };
        let a = sleep("a");
        let b = sleep("b");
        registry.insert("a", &a);
        registry.insert("b", &b);
        // A process registered under two names (step name + outputs name)
        // must be killed once, not twice.
        registry.alias("a", "a-outputs");
        assert!(registry.get("a-outputs").is_some());

        let pid_a = a.inner.lock().unwrap().pid.unwrap();
        let pid_b = b.inner.lock().unwrap().pid.unwrap();
        registry.shutdown_all().await;

        assert!(matches!(
            a.inner.lock().unwrap().status,
            ProcStatus::Exited(_)
        ));
        assert!(matches!(
            b.inner.lock().unwrap().status,
            ProcStatus::Exited(_)
        ));
        assert!(!pid_alive(pid_a));
        assert!(!pid_alive(pid_b));
    }
}
