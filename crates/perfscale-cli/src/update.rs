//! Release lookup, version comparison, and the update-available cache —
//! shared by `perfscale self-update` and the passive startup notice.
//!
//! All GitHub endpoints are derived from two base URLs that can be overridden
//! via environment variables, which makes the whole flow testable against a
//! local mock server and lets enterprise users point at an internal mirror:
//!
//! - `PERFSCALE_UPDATE_API_BASE` — default `https://api.github.com`
//! - `PERFSCALE_UPDATE_DOWNLOAD_BASE` — default `https://github.com`

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

pub const REPO: &str = "Perfscale/perfscale";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a cached "latest version" answer stays fresh.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

pub fn api_base() -> String {
    std::env::var("PERFSCALE_UPDATE_API_BASE").unwrap_or_else(|_| "https://api.github.com".into())
}

pub fn download_base() -> String {
    std::env::var("PERFSCALE_UPDATE_DOWNLOAD_BASE").unwrap_or_else(|_| "https://github.com".into())
}

/// `1` / `true` disables both the passive notice and its network call.
pub fn checks_disabled() -> bool {
    matches!(
        std::env::var("PERFSCALE_NO_UPDATE_CHECK").as_deref(),
        Ok("1") | Ok("true")
    )
}

// ---------------------------------------------------------------------------
// Latest-release lookup
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ReleaseResponse {
    tag_name: String,
}

/// Ask the API for the latest release tag (e.g. `"v0.2.0"`).
pub async fn fetch_latest_tag(timeout: Duration) -> Result<String, String> {
    let url = format!("{}/repos/{REPO}/releases/latest", api_base());
    let resp = reqwest::Client::new()
        .get(&url)
        // GitHub's API rejects requests without a User-Agent.
        .header("user-agent", format!("perfscale/{CURRENT_VERSION}"))
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("failed to reach {url}: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("{url} returned {}", resp.status()));
    }
    let release: ReleaseResponse = resp
        .json()
        .await
        .map_err(|e| format!("unexpected release response from {url}: {e}"))?;
    Ok(release.tag_name)
}

// ---------------------------------------------------------------------------
// Version comparison
// ---------------------------------------------------------------------------

/// Numeric component-wise comparison of `"0.2.1"`-style versions.
/// A leading `v` is ignored. Missing components count as 0.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split('.')
            .map(|c| {
                c.chars()
                    .take_while(|ch| ch.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    };
    let (a, b) = (parse(candidate), parse(current));
    let len = a.len().max(b.len());
    for i in 0..len {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Platform → release artifact
// ---------------------------------------------------------------------------

/// Release asset name for a given OS/arch pair (as in `std::env::consts`).
pub fn artifact_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("perfscale-linux-amd64"),
        ("linux", "aarch64") => Some("perfscale-linux-arm64"),
        ("macos", "aarch64") => Some("perfscale-darwin-arm64"),
        ("macos", "x86_64") => Some("perfscale-darwin-amd64"),
        ("windows", "x86_64") => Some("perfscale-windows-amd64.exe"),
        ("windows", "aarch64") => Some("perfscale-windows-arm64.exe"),
        _ => None,
    }
}

/// Asset name for the machine this binary is running on.
pub fn current_artifact() -> Option<&'static str> {
    artifact_for(std::env::consts::OS, std::env::consts::ARCH)
}

// ---------------------------------------------------------------------------
// npm-managed installs
// ---------------------------------------------------------------------------

/// The command that updates an npm-managed install (`@perfscale/exe`).
pub const NPM_UPDATE_COMMAND: &str = "npm install -g @perfscale/exe@latest";

/// True when the running binary was installed by npm: `@perfscale/exe`
/// resolves its platform package to
/// `…/node_modules/@perfscale/<os>-<arch>/bin/perfscale`. Swapping that file
/// in place would desync npm's metadata (the package would still claim the
/// old version), so npm installs must be updated through npm.
pub fn is_npm_install() -> bool {
    std::env::current_exe()
        .map(|p| path_is_npm_install(&p.to_string_lossy()))
        .unwrap_or(false)
}

fn path_is_npm_install(path: &str) -> bool {
    path.replace('\\', "/")
        .contains("/node_modules/@perfscale/")
}

/// Download URL for a release asset of a specific tag.
pub fn asset_url(tag: &str, artifact: &str) -> String {
    format!(
        "{}/{REPO}/releases/download/{tag}/{artifact}",
        download_base()
    )
}

// ---------------------------------------------------------------------------
// sha256sums.txt
// ---------------------------------------------------------------------------

/// Extract the hex digest for `artifact` from a `sha256sums.txt` body
/// (`<hex>  <name>` per line, sha256sum format).
pub fn digest_from_sums(sums: &str, artifact: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hex = parts.next()?;
        let name = parts.next()?;
        (name == artifact).then(|| hex.to_ascii_lowercase())
    })
}

/// Hex-encoded SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Update-available cache (drives the passive startup notice)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct VersionCache {
    /// Unix seconds of the last successful check.
    pub checked_at: u64,
    /// Latest tag seen at that time (e.g. `"v0.2.0"`).
    pub latest_tag: String,
}

pub fn cache_path() -> Option<PathBuf> {
    Some(
        dirs::cache_dir()?
            .join("perfscale")
            .join("version-check.json"),
    )
}

pub fn read_cache() -> Option<VersionCache> {
    let text = std::fs::read_to_string(cache_path()?).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_cache(latest_tag: &str) {
    let Some(path) = cache_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let cache = VersionCache {
        checked_at: unix_now(),
        latest_tag: latest_tag.to_string(),
    };
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = std::fs::write(path, json);
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl VersionCache {
    pub fn is_fresh(&self, now: u64) -> bool {
        now.saturating_sub(self.checked_at) < CACHE_TTL.as_secs()
    }
}

// ---------------------------------------------------------------------------
// Passive notice — called from main after the real command finishes
// ---------------------------------------------------------------------------

/// Print "update available" to stderr if a newer release is known.
///
/// Never blocks the command it decorates for more than ~2s, never runs in
/// non-interactive contexts (CI logs shouldn't accumulate hints or produce
/// network calls), and phones home at most once per [`CACHE_TTL`] thanks to
/// the on-disk cache.
pub async fn maybe_print_update_notice() {
    use std::io::IsTerminal;

    if checks_disabled() || !std::io::stderr().is_terminal() {
        return;
    }

    let now = unix_now();
    let latest = match read_cache().filter(|c| c.is_fresh(now)) {
        Some(cache) => cache.latest_tag,
        None => match fetch_latest_tag(Duration::from_secs(2)).await {
            Ok(tag) => {
                write_cache(&tag);
                tag
            }
            // Offline or rate-limited — stay silent, retry next TTL window.
            Err(_) => return,
        },
    };

    if is_newer(&latest, CURRENT_VERSION) {
        let cmd = if is_npm_install() {
            NPM_UPDATE_COMMAND
        } else {
            "perfscale self-update"
        };
        eprintln!("\nperfscale {latest} is available (you have {CURRENT_VERSION}) — run `{cmd}`");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_basic_ordering() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
    }

    #[test]
    fn is_newer_handles_missing_components_and_suffixes() {
        assert!(is_newer("0.2", "0.1.9"));
        assert!(!is_newer("0.1", "0.1.0"));
        // Pre-release suffixes: numeric prefix wins, suffix ignored.
        assert!(is_newer("0.2.0-rc1", "0.1.0"));
    }

    #[test]
    fn artifact_for_covers_all_release_platforms() {
        assert_eq!(
            artifact_for("linux", "x86_64"),
            Some("perfscale-linux-amd64")
        );
        assert_eq!(
            artifact_for("linux", "aarch64"),
            Some("perfscale-linux-arm64")
        );
        assert_eq!(
            artifact_for("macos", "aarch64"),
            Some("perfscale-darwin-arm64")
        );
        assert_eq!(
            artifact_for("macos", "x86_64"),
            Some("perfscale-darwin-amd64")
        );
        assert_eq!(
            artifact_for("windows", "x86_64"),
            Some("perfscale-windows-amd64.exe")
        );
        assert_eq!(
            artifact_for("windows", "aarch64"),
            Some("perfscale-windows-arm64.exe")
        );
        assert_eq!(artifact_for("freebsd", "x86_64"), None);
    }

    #[test]
    fn current_platform_has_an_artifact() {
        // The dev/CI machines we build on are all release platforms.
        assert!(current_artifact().is_some());
    }

    #[test]
    fn npm_install_detection_by_path() {
        assert!(path_is_npm_install(
            "/usr/local/lib/node_modules/@perfscale/darwin-arm64/bin/perfscale"
        ));
        assert!(path_is_npm_install(
            r"C:\Users\u\AppData\Roaming\npm\node_modules\@perfscale\win32-x64\bin\perfscale.exe"
        ));
        assert!(!path_is_npm_install("/usr/local/bin/perfscale"));
        // Other scopes' node_modules don't count.
        assert!(!path_is_npm_install(
            "/opt/app/node_modules/@other/pkg/bin/perfscale"
        ));
    }

    #[test]
    fn digest_from_sums_finds_matching_line() {
        let sums = "abc123  perfscale-linux-amd64\nDEF456  perfscale-darwin-arm64\n";
        assert_eq!(
            digest_from_sums(sums, "perfscale-linux-amd64").as_deref(),
            Some("abc123")
        );
        // Hex is normalised to lowercase.
        assert_eq!(
            digest_from_sums(sums, "perfscale-darwin-arm64").as_deref(),
            Some("def456")
        );
        assert_eq!(digest_from_sums(sums, "perfscale-windows-amd64.exe"), None);
    }

    #[test]
    fn sha256_hex_known_vector() {
        // sha256("abc")
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn cache_freshness_respects_ttl() {
        let cache = VersionCache {
            checked_at: 1_000_000,
            latest_tag: "v9.9.9".into(),
        };
        assert!(cache.is_fresh(1_000_000 + CACHE_TTL.as_secs() - 1));
        assert!(!cache.is_fresh(1_000_000 + CACHE_TTL.as_secs()));
    }

    #[test]
    fn asset_url_shape() {
        // Default base (no env override in unit tests).
        let url = asset_url("v0.2.0", "perfscale-linux-amd64");
        assert!(
            url.ends_with("/Perfscale/perfscale/releases/download/v0.2.0/perfscale-linux-amd64")
        );
    }
}
