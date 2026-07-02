//! `perfscale self-update` — replace the running binary with the latest
//! GitHub release for this platform.
//!
//! Flow: resolve latest tag → compare versions → download the platform asset
//! and `sha256sums.txt` → verify the digest → atomically swap the executable.
//! The downloaded file is staged *next to* the current executable (same
//! filesystem) so the final rename is atomic.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::SelfUpdateArgs;
use crate::error::CliError;
use crate::update;

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

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

    let artifact = update::current_artifact().ok_or_else(|| {
        CliError::new(format!(
            "no prebuilt binary for this platform ({}/{})",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
        .hint("build from source instead: cargo install --git https://github.com/Perfscale/perfscale perfscale-cli")
        .docs("getting-started.md#install")
    })?;

    eprintln!("[self-update] downloading {artifact} {latest}...");
    let binary = download(&update::asset_url(&latest, artifact)).await?;

    let sums_text = download(&update::asset_url(&latest, "sha256sums.txt")).await?;
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

async fn download(url: &str) -> Result<Vec<u8>, CliError> {
    let resp = reqwest::Client::new()
        .get(url)
        .header(
            "user-agent",
            format!("perfscale/{}", update::CURRENT_VERSION),
        )
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            CliError::new(format!("download failed: {url}"))
                .cause(e.to_string())
                .docs("cli/commands.md#perfscale-self-update")
        })?;

    if !resp.status().is_success() {
        return Err(CliError::new(format!("download failed: {url}"))
            .cause(format!("HTTP {}", resp.status()))
            .hint("the release may still be uploading — retry in a minute")
            .docs("cli/commands.md#perfscale-self-update"));
    }

    Ok(resp
        .bytes()
        .await
        .map_err(|e| CliError::new(format!("download interrupted: {url}")).cause(e.to_string()))?
        .to_vec())
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
