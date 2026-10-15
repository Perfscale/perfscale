//! `perfscale self-update` — replace the running binary with the latest
//! GitHub release for this platform.
//!
//! Flow: resolve latest tag → compare versions → download the platform asset
//! and `sha256sums.txt` → verify the digest → atomically swap the executable.
//! The downloaded file is staged *next to* the current executable (same
//! filesystem) so the final rename is atomic.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::SelfUpdateArgs;
use crate::error::CliError;
use crate::update;

/// Timeouts: NO total request timeout — the 44 MiB asset on a ~470 KB/s
/// link legitimately needs minutes, and a total cap fails mid-body with a
/// misleading "error decoding response body". Instead: connect must succeed
/// in 10s (DNS+TCP+TLS is quick or broken), and a read may stall for at most
/// 60s (per-read, not cumulative) — a live-but-slow download is exactly what
/// we must not kill. A truly dead transfer is caught by the read stall, so a
/// total backstop would only ever fire on transfers that were working fine.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Transport/decode failures are retried (a dropped TCP connection halfway
/// through 44 MiB is routine); HTTP statuses and checksum failures are not.
/// Attempts = backoff slots + 1 (3 with the default 1s/2s).
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];

async fn download(url: &str, progress: bool) -> Result<Vec<u8>, CliError> {
    download_with(url, progress, CONNECT_TIMEOUT, READ_TIMEOUT, &RETRY_BACKOFF).await
}

async fn download_with(
    url: &str,
    progress: bool,
    connect_timeout: Duration,
    read_timeout: Duration,
    backoff: &[Duration],
) -> Result<Vec<u8>, CliError> {
    let attempts = backoff.len() as u32 + 1;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match download_once(url, progress, connect_timeout, read_timeout).await {
            Ok(bytes) => return Ok(bytes),
            // HTTP statuses are definitive (404 = not there, retrying won't
            // help); only transport-level failures get another attempt.
            Err(e) if !e.retryable => return Err(e.error),
            Err(e) if attempt > backoff.len() as u32 => return Err(e.error),
            Err(e) => {
                let _ = &e;
                let wait = backoff[(attempt - 1) as usize];
                eprintln!(
                    "[self-update] attempt {attempt}/{attempts} failed, retrying in {}s...",
                    wait.as_secs()
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// One attempt's failure, classified: retryable transport/decode failures
/// vs definitive HTTP statuses.
struct DownloadFailure {
    error: CliError,
    retryable: bool,
}

async fn download_once(
    url: &str,
    progress: bool,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<Vec<u8>, DownloadFailure> {
    let resp = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .read_timeout(read_timeout)
        .build()
        .expect("default client config builds")
        .get(url)
        .header(
            "user-agent",
            format!("perfscale/{}", update::CURRENT_VERSION),
        )
        .send()
        .await
        .map_err(|e| network_failure(url, e, 0, None))?;

    if !resp.status().is_success() {
        return Err(DownloadFailure {
            error: CliError::new(format!("download failed: {url}"))
                .cause(format!("HTTP {}", resp.status()))
                .hint("the release may still be uploading — retry in a minute")
                .docs("cli/commands.md#perfscale-self-update"),
            retryable: false,
        });
    }

    // Stream the body so a slow link shows progress instead of silence, and
    // a stall is caught by read_timeout per chunk, not by a total budget.
    let total = resp.content_length();
    let mut body = Vec::with_capacity(total.unwrap_or(0).min(64 << 20) as usize);
    let mut received: u64 = 0;
    let show_progress = progress && std::io::stderr().is_terminal();
    let mut resp = resp;
    let mut last_print = std::time::Instant::now();
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => return Err(network_failure(url, e, received, total)),
        };
        body.extend_from_slice(&chunk);
        received += chunk.len() as u64;
        if show_progress && last_print.elapsed() >= Duration::from_millis(100) {
            last_print = std::time::Instant::now();
            match total {
                Some(t) => eprint!(
                    "\r[self-update] {:.1}/{:.1} MiB",
                    received as f64 / (1 << 20) as f64,
                    t as f64 / (1 << 20) as f64,
                ),
                None => eprint!(
                    "\r[self-update] {:.1} MiB",
                    received as f64 / (1 << 20) as f64
                ),
            }
        }
    }
    if show_progress {
        eprintln!();
    }
    Ok(body)
}

/// Translate reqwest's cryptic mid-body failures ("error decoding response
/// body" on a stalled or dropped connection) into what actually happened.
fn network_failure(
    url: &str,
    e: reqwest::Error,
    received: u64,
    total: Option<u64>,
) -> DownloadFailure {
    let what = if e.is_timeout() {
        "network stalled (no data for a minute)".to_string()
    } else if received > 0 {
        let got = match total {
            Some(t) => format!(
                "{:.1}/{:.1} MiB received",
                received as f64 / (1 << 20) as f64,
                t as f64 / (1 << 20) as f64
            ),
            None => format!("{:.1} MiB received", received as f64 / (1 << 20) as f64),
        };
        format!("connection dropped mid-download ({got})")
    } else {
        "connection failed".to_string()
    };
    DownloadFailure {
        error: CliError::new(format!("download interrupted: {what}"))
            .cause(format!("{url}: {e}"))
            .hint("check your network connection and retry — the release asset is tens of MiB, so a slow or unstable link needs time")
            .docs("cli/commands.md#perfscale-self-update"),
        retryable: true,
    }
}

pub async fn self_update(args: SelfUpdateArgs) -> Result<(), CliError> {
    let latest = update::fetch_latest_tag(Duration::from_secs(10)).await.map_err(|e| {
        CliError::new("failed to look up the latest release")
            .cause(e)
            .hint("check network access; the release feed is queried from api.github.com (override with PERFSCALE_UPDATE_API_BASE)")
            .docs("cli/commands.md#perfscale-self-update")
    })?;
    // A successful lookup is also what the passive notice wants to know.
    update::write_cache(&latest);

    let newer = update::is_newer(&latest, update::CURRENT_VERSION);

    if args.check {
        return if newer {
            println!(
                "update available: {latest} (current: {})",
                update::CURRENT_VERSION
            );
            // Distinguishable from "up to date" in scripts: 10 = update exists.
            std::process::exit(10);
        } else {
            println!(
                "perfscale {} is up to date (latest: {latest})",
                update::CURRENT_VERSION
            );
            Ok(())
        };
    }

    if !newer && !args.force {
        println!(
            "perfscale {} is already the latest version — nothing to do (use --force to reinstall)",
            update::CURRENT_VERSION
        );
        return Ok(());
    }

    // npm owns the files of an @perfscale/exe install — swapping the binary
    // behind npm's back would leave its metadata claiming the old version.
    if update::is_npm_install() {
        return Err(CliError::new(
            "this perfscale was installed via npm (@perfscale/exe) — self-update would desync npm's package metadata",
        )
        .hint(format!("update through npm instead: {}", update::NPM_UPDATE_COMMAND))
        .docs("cli/commands.md#perfscale-self-update"));
    }

    let artifact = update::current_artifact().ok_or_else(|| {
        CliError::new(format!(
            "no prebuilt binary for this platform ({}/{})",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
        .hint("build from source instead: cargo install --git https://github.com/Perfscale/perfscale perfscale-cli")
        .docs("getting-started.md#install")
    })?;

    // Confirm before anything is downloaded. Interactive by default; scripts
    // and CI pass --non-interactive.
    match confirm_update(&latest, args.non_interactive)? {
        UpdateConfirmation::Proceed => {}
        UpdateConfirmation::Declined => {
            println!(
                "update cancelled — perfscale {} unchanged",
                update::CURRENT_VERSION
            );
            return Ok(());
        }
    }

    eprintln!("[self-update] downloading {artifact} {latest}...");
    let binary = download(&update::asset_url(&latest, artifact), true).await?;

    let sums_text = download(&update::asset_url(&latest, "sha256sums.txt"), false).await?;
    let sums_text = String::from_utf8_lossy(&sums_text).to_string();
    verify_digest(&binary, &sums_text, artifact)?;
    eprintln!("[self-update] sha256 verified");

    let exe = std::env::current_exe()
        .map_err(|e| CliError::new("cannot locate the running executable").cause(e.to_string()))?;
    replace_executable(&exe, &binary)?;

    println!(
        "updated perfscale {} → {latest} ({})",
        update::CURRENT_VERSION,
        exe.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Confirmation prompt
// ---------------------------------------------------------------------------

/// The outcome of the pre-download confirmation gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateConfirmation {
    Proceed,
    Declined,
}

/// The user's answer to the prompt, mapped to a decision. Empty input (just
/// Enter) is the default "yes"; `n`/`no` (any case) declines; EOF (`None`)
/// declines — a terminal that went away must not install anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnswerDecision {
    Yes,
    No,
    Invalid,
}

fn answer_decision(answer: Option<&str>) -> AnswerDecision {
    match answer.map(str::trim) {
        None => AnswerDecision::No,
        Some("") => AnswerDecision::Yes,
        Some(a) if a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes") => {
            AnswerDecision::Yes
        }
        Some(a) if a.eq_ignore_ascii_case("n") || a.eq_ignore_ascii_case("no") => {
            AnswerDecision::No
        }
        Some(_) => AnswerDecision::Invalid,
    }
}

/// Gate the download on a confirmation. Pure decision, stdin reading lives
/// in [`confirm_update`]: `--non-interactive` short-circuits (scripts, CI);
/// without it a non-TTY stdin is an error — a prompt nobody can answer would
/// hang a CI job, which is worse than a refused run.
fn confirmation_gate(non_interactive: bool, stdin_is_tty: bool) -> Result<bool, CliError> {
    if non_interactive {
        return Ok(false);
    }
    if !stdin_is_tty {
        return Err(CliError::new(
            "self-update asks for confirmation before replacing the binary, but stdin is not a terminal",
        )
        .hint("re-run with --non-interactive in scripts/CI")
        .docs("cli/commands.md#perfscale-self-update"));
    }
    Ok(true)
}

/// One line of user input — the only stdin read in the flow, kept behind the
/// decision functions so they stay unit-testable.
fn read_answer() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None, // EOF or read error
        Ok(_) => Some(line),
    }
}

/// Ask before downloading and replacing the binary: `Update perfscale
/// <current> → <latest>? [Y/n]` on stderr (stdout stays machine-parseable),
/// answer from stdin. Declining is a normal exit-0 outcome, not an error.
fn confirm_update(latest: &str, non_interactive: bool) -> Result<UpdateConfirmation, CliError> {
    if !confirmation_gate(non_interactive, std::io::stdin().is_terminal())? {
        return Ok(UpdateConfirmation::Proceed);
    }
    loop {
        eprint!(
            "Update perfscale {} → {latest}? [Y/n] ",
            update::CURRENT_VERSION
        );
        match answer_decision(read_answer().as_deref()) {
            AnswerDecision::Yes => return Ok(UpdateConfirmation::Proceed),
            AnswerDecision::No => return Ok(UpdateConfirmation::Declined),
            AnswerDecision::Invalid => {
                eprintln!("please answer y or n");
            }
        }
    }
}

fn verify_digest(binary: &[u8], sums_text: &str, artifact: &str) -> Result<(), CliError> {
    let expected = update::digest_from_sums(sums_text, artifact).ok_or_else(|| {
        CliError::new(format!("sha256sums.txt has no entry for {artifact}"))
            .hint("the release assets look incomplete — report this at https://github.com/Perfscale/perfscale/issues")
    })?;
    let actual = update::sha256_hex(binary);
    if actual != expected {
        return Err(
            CliError::new("downloaded binary failed checksum verification")
                .cause(format!("expected {expected}, got {actual}"))
                .hint("refusing to install a corrupted or tampered binary; retry the update"),
        );
    }
    Ok(())
}

/// Swap `exe` for `new_contents`, staging in the same directory so the final
/// rename is atomic (and never leaves a half-written executable in place).
fn replace_executable(exe: &Path, new_contents: &[u8]) -> Result<(), CliError> {
    let staged = staged_path(exe);
    std::fs::write(&staged, new_contents).map_err(|e| {
        CliError::new(format!(
            "failed to stage new binary at '{}'",
            staged.display()
        ))
        .cause(e.to_string())
        .hint("is the install directory writable? re-run with appropriate permissions")
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(|e| {
            CliError::new("failed to mark new binary executable").cause(e.to_string())
        })?;
        std::fs::rename(&staged, exe).map_err(|e| {
            let _ = std::fs::remove_file(&staged);
            CliError::new(format!("failed to replace '{}'", exe.display()))
                .cause(e.to_string())
                .hint("is the install directory writable? re-run with appropriate permissions")
        })?;
    }

    #[cfg(windows)]
    {
        // Windows can't overwrite a running executable, but it CAN rename it:
        // move the running binary aside, then move the new one into place.
        let old = exe.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).map_err(|e| {
            CliError::new(format!(
                "failed to move current binary aside to '{}'",
                old.display()
            ))
            .cause(e.to_string())
        })?;
        if let Err(e) = std::fs::rename(&staged, exe) {
            // Roll back so the user still has a working binary.
            let _ = std::fs::rename(&old, exe);
            return Err(CliError::new(format!(
                "failed to install new binary at '{}'",
                exe.display()
            ))
            .cause(e.to_string()));
        }
        // Best effort — the .old.exe may stay behind until next update.
        let _ = std::fs::remove_file(&old);
    }

    Ok(())
}

fn staged_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".new");
    exe.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // download(): timeouts, retries, truncated bodies
    // -----------------------------------------------------------------

    /// One attempt's server behavior for the raw test server.
    #[derive(Clone)]
    enum Serve {
        /// Full body in one go.
        Full(Vec<u8>),
        /// Advertise the full length, send a prefix, close — the mid-body
        /// drop a flaky link produces.
        Truncated { body: Vec<u8>, send: usize },
        /// Full body in chunks with a delay between them — each under the
        /// read timeout, so a slow-but-alive transfer must succeed.
        Slow {
            body: Vec<u8>,
            chunk: usize,
            delay: Duration,
        },
        /// A definitive HTTP status (no body semantics to speak of).
        Status(u16),
    }

    /// A bare-bones HTTP/1.1 server: request number i gets `script[i]` (or
    /// the last entry once the script runs out). Returns (base url, hit
    /// count) — the hit count is how we prove retries did or did not happen.
    async fn spawn_server(
        script: Vec<Serve>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits_bg = std::sync::Arc::clone(&hits);
        tokio::spawn(async move {
            let mut i = 0usize;
            while let Ok((mut sock, _)) = listener.accept().await {
                hits_bg.fetch_add(1, Ordering::Relaxed);
                let serve = script.get(i).or_else(|| script.last()).unwrap().clone();
                i += 1;
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut req = [0u8; 1024];
                    let _ = sock.read(&mut req).await;
                    match &serve {
                        Serve::Status(code) => {
                            let _ = sock
                                .write_all(format!("HTTP/1.1 {code} Nope\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes())
                                .await;
                        }
                        full @ (Serve::Full(_) | Serve::Truncated { .. } | Serve::Slow { .. }) => {
                            let body = match full {
                                Serve::Full(b) => b,
                                Serve::Truncated { body, .. } => body,
                                Serve::Slow { body, .. } => body,
                                Serve::Status(_) => unreachable!(),
                            };
                            let _ = sock
                                .write_all(
                                    format!(
                                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                        body.len()
                                    )
                                    .as_bytes(),
                                )
                                .await;
                            match full {
                                Serve::Full(body) => {
                                    let _ = sock.write_all(body).await;
                                }
                                Serve::Truncated { send, .. } => {
                                    let _ = sock.write_all(&body[..*send]).await;
                                    // Close mid-body: drop the socket.
                                }
                                Serve::Slow { chunk, delay, .. } => {
                                    for part in body.chunks(*chunk) {
                                        if sock.write_all(part).await.is_err() {
                                            break;
                                        }
                                        tokio::time::sleep(*delay).await;
                                    }
                                }
                                Serve::Status(_) => unreachable!(),
                            }
                        }
                    }
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    fn test_backoff() -> [Duration; 2] {
        [Duration::from_millis(10), Duration::from_millis(10)]
    }

    /// (a) Chunked body, every inter-chunk delay < read_timeout → success,
    /// body assembled correctly.
    #[tokio::test]
    async fn download_slow_but_alive_transfer_succeeds() {
        let body: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
        let (url, _hits) = spawn_server(vec![Serve::Slow {
            body: body.clone(),
            chunk: 1024,
            delay: Duration::from_millis(20),
        }])
        .await;
        let got = download_with(
            &format!("{url}/asset"),
            false,
            Duration::from_secs(2),
            Duration::from_secs(2),
            &test_backoff(),
        )
        .await
        .expect("slow transfer within the read timeout must succeed");
        assert_eq!(got, body);
    }

    /// (b) First attempt drops mid-body → retried; a whole second attempt
    /// succeeds. Two hits, no more.
    #[tokio::test]
    async fn download_retries_a_truncated_body() {
        let body = b"complete asset body".to_vec();
        let (url, hits) = spawn_server(vec![
            Serve::Truncated {
                body: body.clone(),
                send: 5,
            },
            Serve::Full(body.clone()),
        ])
        .await;
        let got = download_with(
            &format!("{url}/asset"),
            false,
            Duration::from_secs(2),
            Duration::from_secs(2),
            &test_backoff(),
        )
        .await
        .expect("the retry must succeed");
        assert_eq!(got, body);
        assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    /// (b') Every attempt drops mid-body → a human-readable error that names
    /// what happened (never reqwest's "error decoding response body").
    #[tokio::test]
    async fn download_always_truncated_fails_readably() {
        let body = b"complete asset body".to_vec();
        let (url, hits) = spawn_server(vec![Serve::Truncated { body, send: 5 }]).await;
        let err = download_with(
            &format!("{url}/asset"),
            false,
            Duration::from_secs(2),
            Duration::from_secs(2),
            &test_backoff(),
        )
        .await
        .unwrap_err();
        let text = err.to_string();
        let first_line = text.lines().next().unwrap();
        assert!(
            first_line.contains("connection dropped mid-download"),
            "{text}"
        );
        // The raw reqwest error stays as the indented cause for debugging,
        // but the headline is human.
        assert!(!first_line.contains("error decoding"), "{text}");
        assert!(text.contains("hint:"), "{text}");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "initial attempt + 2 retries"
        );
    }

    /// (c) An HTTP status is definitive: no retries.
    #[tokio::test]
    async fn download_http_status_is_not_retried() {
        let (url, hits) = spawn_server(vec![Serve::Status(404)]).await;
        let err = download_with(
            &format!("{url}/asset"),
            false,
            Duration::from_secs(2),
            Duration::from_secs(2),
            &test_backoff(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("HTTP 404"), "{err}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn confirmation_gate_skips_prompt_only_when_allowed() {
        // --non-interactive: never prompts, TTY or not.
        assert!(!confirmation_gate(true, true).unwrap());
        assert!(!confirmation_gate(true, false).unwrap());
        // Interactive + TTY: prompt.
        assert!(confirmation_gate(false, true).unwrap());
        // Interactive without a TTY: a clean error, not a hanging prompt.
        let err = confirmation_gate(false, false).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("not a terminal"), "{rendered}");
        assert!(rendered.contains("--non-interactive"), "{rendered}");
    }

    #[test]
    fn answer_decision_maps_answers() {
        assert_eq!(answer_decision(Some("")), AnswerDecision::Yes);
        assert_eq!(answer_decision(Some("  ")), AnswerDecision::Yes);
        assert_eq!(answer_decision(Some("y")), AnswerDecision::Yes);
        assert_eq!(answer_decision(Some("YES\n")), AnswerDecision::Yes);
        assert_eq!(answer_decision(Some("n")), AnswerDecision::No);
        assert_eq!(answer_decision(Some("No")), AnswerDecision::No);
        assert_eq!(answer_decision(Some("NO\n")), AnswerDecision::No);
        // EOF (Ctrl-D): decline, never install.
        assert_eq!(answer_decision(None), AnswerDecision::No);
        assert_eq!(answer_decision(Some("maybe")), AnswerDecision::Invalid);
    }

    #[test]
    fn staged_path_is_next_to_exe() {
        let staged = staged_path(Path::new("/usr/local/bin/perfscale"));
        assert_eq!(staged, Path::new("/usr/local/bin/perfscale.new"));
    }

    #[test]
    fn verify_digest_accepts_matching_and_rejects_mismatched() {
        let binary = b"fake binary contents";
        let good = format!("{}  my-artifact\n", update::sha256_hex(binary));
        assert!(verify_digest(binary, &good, "my-artifact").is_ok());

        let bad = "0000000000000000000000000000000000000000000000000000000000000000  my-artifact\n";
        let err = verify_digest(binary, bad, "my-artifact").unwrap_err();
        assert!(err.to_string().contains("checksum"));
    }

    #[test]
    fn verify_digest_missing_entry_is_error() {
        let err = verify_digest(b"data", "abc  other-artifact\n", "my-artifact").unwrap_err();
        assert!(err.to_string().contains("no entry"));
    }

    #[cfg(unix)]
    #[test]
    fn replace_executable_swaps_contents_atomically() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("perfscale");
        std::fs::write(&exe, b"old").unwrap();

        replace_executable(&exe, b"new contents").unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"new contents");
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "binary must be executable");
        assert!(!staged_path(&exe).exists(), "staging file must not remain");
    }
}
