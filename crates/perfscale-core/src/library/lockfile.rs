//! `perfscale.lock` — the pin file for remote library sources (RFC 005
//! phase 3).
//!
//! One lockfile lives next to each YAML document that declares remote
//! (`https://` / `git+`) libraries, written by `perfscale install` and read
//! at every load (see [`crate::import`]). It is TOML, keyed by the exact
//! `use:` string:
//!
//! ```toml
//! version = 1
//!
//! [[libraries]]
//! use = "https://example.com/lib.wasm"
//! sha256 = "…"
//!
//! [[libraries]]
//! use = "git+https://github.com/org/repo.git@v1.2.3#lib.wasm"
//! commit = "…resolved sha…"
//! sha256 = "…artifact digest…"
//! ```
//!
//! HTTPS entries pin the declared digest — a re-published artifact with a
//! different digest is a hard error, never a silent swap. Git entries pin
//! the resolved commit (tags and branches are mutable; commits are not)
//! plus the artifact digest.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Lockfile name, placed in the declaring document's directory.
pub const LOCKFILE_NAME: &str = "perfscale.lock";

/// The only format version this binary reads and writes.
pub const LOCKFILE_VERSION: u32 = 1;

/// Path of the lockfile belonging to documents in `dir`.
pub fn lockfile_path(dir: &Path) -> PathBuf {
    dir.join(LOCKFILE_NAME)
}

/// A parsed `perfscale.lock`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    /// Format version; must equal [`LOCKFILE_VERSION`].
    pub version: u32,
    /// Pinned entries, keyed by their exact `use:` string.
    #[serde(rename = "libraries", default)]
    pub libraries: Vec<LockEntry>,
}

/// One pinned library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    /// The exact `use:` string from the YAML document.
    #[serde(rename = "use")]
    pub use_: String,
    /// Resolved commit SHA — git sources only (`None` for HTTPS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// SHA-256 of the fetched artifact (64 lowercase hex characters).
    pub sha256: String,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self {
            version: LOCKFILE_VERSION,
            libraries: Vec::new(),
        }
    }
}

impl Lockfile {
    /// Parse lockfile TOML. A version this binary does not understand is an
    /// error — never silently reinterpreted or overwritten.
    pub fn parse(text: &str) -> Result<Self, String> {
        let lock: Lockfile =
            toml::from_str(text).map_err(|e| format!("invalid {LOCKFILE_NAME}: {e}"))?;
        if lock.version != LOCKFILE_VERSION {
            return Err(format!(
                "unsupported {LOCKFILE_NAME} version {} (this perfscale understands version {LOCKFILE_VERSION})",
                lock.version
            ));
        }
        Ok(lock)
    }

    /// Load the lockfile for documents in `dir`; `Ok(None)` when absent.
    pub fn load(dir: &Path) -> Result<Option<Self>, String> {
        let path = lockfile_path(dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text)
                .map(Some)
                .map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("failed to read '{}': {e}", path.display())),
        }
    }

    /// Serialize back to TOML.
    pub fn serialize(&self) -> String {
        toml::to_string_pretty(self).expect("lockfile serialization cannot fail")
    }

    /// Write the lockfile for documents in `dir`.
    pub fn save(&self, dir: &Path) -> Result<PathBuf, String> {
        let path = lockfile_path(dir);
        std::fs::write(&path, self.serialize())
            .map_err(|e| format!("failed to write '{}': {e}", path.display()))?;
        Ok(path)
    }

    /// Find the entry for an exact `use:` string.
    pub fn find(&self, use_: &str) -> Option<&LockEntry> {
        self.libraries.iter().find(|e| e.use_ == use_)
    }

    /// Insert or replace the entry for `entry.use_`, leaving every other
    /// entry (including sources this run did not touch) untouched.
    pub fn upsert(&mut self, entry: LockEntry) {
        match self.libraries.iter_mut().find(|e| e.use_ == entry.use_) {
            Some(existing) => *existing = entry,
            None => self.libraries.push(entry),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"version = 1

[[libraries]]
use = "https://example.com/lib.wasm"
sha256 = "aa11"

[[libraries]]
use = "git+https://github.com/org/repo.git@v1.2.3#lib.wasm"
commit = "4471101ab"
sha256 = "bb22"
"#;

    #[test]
    fn roundtrip_preserves_entries() {
        let lock = Lockfile::parse(SAMPLE).unwrap();
        assert_eq!(lock.version, 1);
        assert_eq!(lock.libraries.len(), 2);
        let text = lock.serialize();
        let again = Lockfile::parse(&text).unwrap();
        assert_eq!(again.libraries.len(), 2);
        let git = again
            .find("git+https://github.com/org/repo.git@v1.2.3#lib.wasm")
            .unwrap();
        assert_eq!(git.commit.as_deref(), Some("4471101ab"));
        assert_eq!(git.sha256, "bb22");
        // HTTPS entries carry no commit key in the serialized form.
        let https = again.find("https://example.com/lib.wasm").unwrap();
        assert!(https.commit.is_none());
        assert!(!text.contains("commit = \"\""));
    }

    #[test]
    fn upsert_replaces_by_use_key_and_keeps_others() {
        let mut lock = Lockfile::parse(SAMPLE).unwrap();
        lock.upsert(LockEntry {
            use_: "https://example.com/lib.wasm".into(),
            commit: None,
            sha256: "cc33".into(),
        });
        lock.upsert(LockEntry {
            use_: "https://new.example.com/x.wasm".into(),
            commit: None,
            sha256: "dd44".into(),
        });
        assert_eq!(lock.libraries.len(), 3, "one replaced, one appended");
        assert_eq!(
            lock.find("https://example.com/lib.wasm").unwrap().sha256,
            "cc33"
        );
        assert_eq!(
            lock.find("git+https://github.com/org/repo.git@v1.2.3#lib.wasm")
                .unwrap()
                .sha256,
            "bb22",
            "untouched entry survives"
        );
    }

    #[test]
    fn lookup_misses_unknown_use() {
        let lock = Lockfile::parse(SAMPLE).unwrap();
        assert!(lock.find("https://other.example.com/l.wasm").is_none());
    }

    #[test]
    fn unknown_version_is_rejected() {
        let err = Lockfile::parse("version = 99\n").unwrap_err();
        assert!(err.contains("version 99"), "{err}");
    }

    #[test]
    fn malformed_toml_is_rejected() {
        assert!(Lockfile::parse("version = [").is_err());
        assert!(Lockfile::parse("[[libraries]]\nsha256 = \"x\"\n").is_err());
    }

    #[test]
    fn load_distinguishes_absent_from_invalid() {
        let dir = std::env::temp_dir().join(format!("perfscale-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Lockfile::load(&dir).unwrap().is_none());
        std::fs::write(dir.join(LOCKFILE_NAME), "version = 99\n").unwrap();
        assert!(Lockfile::load(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
