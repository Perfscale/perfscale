//! `import:` — compose test/config documents from a shared base.
//!
//! Both `-c config.yaml` and `-f test.yaml` accept a top-level `import` key
//! naming a base document. The base is loaded first (recursively — it may
//! import its own base), then the importing document is deep-merged on top:
//! objects merge key-by-key, everything else (scalars, arrays — including
//! `steps:`) is replaced by the importing side.
//!
//! Three source forms:
//!
//! ```yaml
//! # relative filesystem path (resolved against the importing file's dir)
//! import: ../shared/_base.yaml
//!
//! # raw HTTP(S) URL — ref pinned in the path
//! import: "https://raw.githubusercontent.com/org/repo/v1.2.0/perf/_base.yaml"
//!
//! # any git host (SSH or HTTPS remotes, self-hosted included)
//! import:
//!   git: git@gitlab.example.com:group/repo.git
//!   ref: v1.2.0
//!   file: perf/config/_base.yaml
//! ```
//!
//! # Security
//!
//! Resolution happens at config-load time — *before* the
//! `allow_file_actions`/`allow_process_actions` gates ever run — so a network
//! fetch here is an SSRF primitive and a supply-chain vector (a remote base
//! can carry `allow_process_actions: true` plus a `std/child_process@v1`
//! step). Permission to touch the network therefore comes from the caller,
//! never from the document itself: remote imports are fail-closed until the
//! embedding process sets [`ImportOptions::allow_remote`] (the CLI maps this
//! to `--allow-remote-import`). A document cannot grant itself the right.
//!
//! Origin confinement follows the same logic: a document fetched from a URL
//! or a git repo may only import siblings from its own origin (relative URL,
//! or a path inside the same clone — `../` escapes out of the clone root are
//! rejected). A remote document can never read the local filesystem.
//!
//! # Caching
//!
//! Git clones land under `~/.cache/perfscale/imports/<hash(git+ref)>`
//! (`$XDG_CACHE_HOME` respected, `$PERFSCALE_CACHE_DIR` overrides). Tags and
//! commit SHAs are treated as immutable — cached forever, content-addressed.
//! Branches are mutable: a cached branch is revalidated with `git ls-remote`
//! after a short TTL, so a floating `ref: main` never silently freezes at
//! its first fetch. [`ImportOptions::refresh`] (`--refresh-imports`) forces
//! refetch of every tag/branch ref; SHAs never refetch.
//!
//! Cloning shells out to the system `git` binary (like the `--k6`/`--locust`
//! runners shell out to theirs) — no libgit2/gitoxide dependency, and the
//! user's existing SSH agent, credential helpers, and proxy config all apply.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// How long a cached *branch* ref is trusted before `git ls-remote`
/// revalidates it. Tags and SHAs never expire.
const BRANCH_TTL_SECS: u64 = 300;

/// Import chains deeper than this are almost certainly a mistake even
/// without a cycle (each hop is a fetch); fail with the chain printed.
const MAX_DEPTH: usize = 16;

/// Refuse to buffer HTTP import bodies beyond this size.
const MAX_HTTP_BODY: usize = 10 * 1024 * 1024;

const REMOTE_BLOCKED: &str = "remote import blocked: this document imports from the network, \
     which is disabled by default (a config from an untrusted source must not be able to \
     reach out or pull in steps that relax the sandbox). Re-run with --allow-remote-import \
     if you trust every import in the chain";

// ---------------------------------------------------------------------------
// Spec (what appears in YAML)
// ---------------------------------------------------------------------------

/// The value of a top-level `import:` key.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ImportSpec {
    /// A relative filesystem path, or an `http(s)://` URL with the ref
    /// already pinned in the path.
    Path(String),
    /// A file at a ref of an arbitrary git remote (SSH or HTTPS).
    Git(GitImport),
}

/// `import: { git, ref, file }` — fetch `file` from `git` at `ref`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitImport {
    /// Git remote, e.g. `git@gitlab.example.com:group/repo.git` or
    /// `https://github.com/org/repo.git`.
    pub git: String,
    /// Tag, branch, or commit SHA. Tags/SHAs cache forever; branches
    /// revalidate against the remote after a short TTL.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Path of the YAML document inside the repository.
    pub file: String,
}

/// Caller-side policy for import resolution. Fail-closed by construction:
/// `Default` allows local file imports only.
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Permit `http(s)://` and `git:` imports. Set by the *caller* (CLI flag
    /// `--allow-remote-import`), never parsed from the document.
    pub allow_remote: bool,
    /// Force refetch of tag/branch git imports (SHAs are immutable and never
    /// refetch). CLI flag `--refresh-imports`.
    pub refresh: bool,
    /// Cache directory override; defaults to
    /// `$PERFSCALE_CACHE_DIR` → `$XDG_CACHE_HOME/perfscale` → `~/.cache/perfscale`.
    pub cache_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Load a local YAML document, resolve its `import` chain, and return the
/// merged JSON value plus the number of imports resolved (0 = plain file).
pub async fn load_document(path: &Path, opts: &ImportOptions) -> Result<(Value, usize), String> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let fp = fingerprint_local(path);
    let mut chain = vec![fp];
    let mut resolved = 0usize;
    let value = resolve_text(
        &text,
        Origin::Local { dir },
        opts,
        &mut chain,
        &mut resolved,
    )
    .await?;
    Ok((value, resolved))
}

/// [`load_document`] + schema validation into a [`crate::yaml::ConfigFile`].
pub async fn load_config_file(
    path: &Path,
    opts: &ImportOptions,
) -> Result<crate::yaml::ConfigFile, String> {
    let (value, _) = load_document(path, opts).await?;
    crate::yaml::config_from_value(value)
}

/// [`load_document`] + schema validation into a [`crate::step::TestDef`].
pub async fn load_test_file(
    path: &Path,
    opts: &ImportOptions,
) -> Result<crate::step::TestDef, String> {
    let (value, _) = load_document(path, opts).await?;
    crate::yaml::test_from_value(value)
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Where the document currently being resolved came from — determines how a
/// relative `import:` inside it is interpreted, and what it may reach.
enum Origin {
    /// Local file: relative imports resolve against its directory.
    Local { dir: PathBuf },
    /// Fetched from a URL: relative imports resolve against that URL.
    Url { base: reqwest::Url },
    /// Checked out of a git clone: relative imports stay inside the clone.
    Git {
        git: String,
        git_ref: String,
        repo_root: PathBuf,
        /// Directory of the current file, absolute, inside `repo_root`.
        dir: PathBuf,
    },
}

async fn resolve_text(
    text: &str,
    origin: Origin,
    opts: &ImportOptions,
    chain: &mut Vec<String>,
    resolved: &mut usize,
) -> Result<Value, String> {
    let mut value: Value = serde_yaml::from_str(text).map_err(|e| format!("invalid YAML: {e}"))?;

    let Some(import_value) = value.as_object_mut().and_then(|obj| obj.remove("import")) else {
        return Ok(value);
    };

    if chain.len() > MAX_DEPTH {
        return Err(format!(
            "import chain deeper than {MAX_DEPTH} levels:\n  {}",
            chain.join("\n  → ")
        ));
    }

    let spec: ImportSpec = serde_json::from_value(import_value).map_err(|_| {
        "invalid `import`: expected a path/URL string or `{ git, ref, file }`".to_string()
    })?;

    let (base_text, base_origin, fp) = fetch(&spec, &origin, opts).await?;
    if chain.contains(&fp) {
        return Err(format!(
            "import cycle detected:\n  {}\n  → {fp} (already in the chain)",
            chain.join("\n  → ")
        ));
    }

    chain.push(fp);
    *resolved += 1;
    let base = Box::pin(resolve_text(&base_text, base_origin, opts, chain, resolved))
        .await
        .map_err(|e| format!("in import '{}': {e}", chain.last().unwrap()))?;
    chain.pop();

    Ok(deep_merge(base, value))
}

/// Deep-merge `overlay` on top of `base`: objects merge key-by-key
/// (recursively), any other pair — scalars, arrays, mismatched types — is
/// won by the overlay. This makes `variables:` from a base composable while
/// a local `steps:` list replaces the base's outright.
fn deep_merge(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Object(mut base_map), Value::Object(overlay_map)) => {
            for (key, overlay_val) in overlay_map {
                let merged = match base_map.remove(&key) {
                    Some(base_val) => deep_merge(base_val, overlay_val),
                    None => overlay_val,
                };
                base_map.insert(key, merged);
            }
            Value::Object(base_map)
        }
        (_, overlay) => overlay,
    }
}

/// Fetch the imported document's text; returns (text, its origin, fingerprint).
async fn fetch(
    spec: &ImportSpec,
    parent: &Origin,
    opts: &ImportOptions,
) -> Result<(String, Origin, String), String> {
    match spec {
        ImportSpec::Path(s) if s.starts_with("http://") || s.starts_with("https://") => {
            if !opts.allow_remote {
                return Err(REMOTE_BLOCKED.into());
            }
            let url =
                reqwest::Url::parse(s).map_err(|e| format!("invalid import URL '{s}': {e}"))?;
            fetch_url(url).await
        }
        ImportSpec::Path(s) if s.starts_with("file://") => Err(format!(
            "import '{s}': file:// URLs are not supported — use a plain relative path"
        )),
        ImportSpec::Path(s) => match parent {
            Origin::Local { dir } => {
                let path = dir.join(s);
                let canonical = path.canonicalize().map_err(|e| {
                    format!("import '{s}': cannot resolve '{}': {e}", path.display())
                })?;
                let text = tokio::fs::read_to_string(&canonical)
                    .await
                    .map_err(|e| format!("import '{s}': {e}"))?;
                let dir = canonical
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                let fp = canonical.display().to_string();
                Ok((text, Origin::Local { dir }, fp))
            }
            // A URL-origin document imports relative to its own URL — it
            // stays remote; it can never point back into the local FS.
            Origin::Url { base } => {
                let url = base
                    .join(s)
                    .map_err(|e| format!("import '{s}' relative to '{base}': {e}"))?;
                fetch_url(url).await
            }
            // A git-origin document may only import inside its own clone.
            Origin::Git {
                git,
                git_ref,
                repo_root,
                dir,
            } => {
                if s.starts_with('/') {
                    return Err(format!(
                        "import '{s}': a document imported from git cannot use absolute paths"
                    ));
                }
                let path = dir.join(s);
                read_repo_file(repo_root, &path, git, git_ref).await
            }
        },
        ImportSpec::Git(remote) => {
            if !opts.allow_remote {
                return Err(REMOTE_BLOCKED.into());
            }
            let repo_root = git_fetch(remote, opts).await?;
            let path = repo_root.join(&remote.file);
            read_repo_file(&repo_root, &path, &remote.git, &remote.git_ref).await
        }
    }
}

/// Read a file that must live inside `repo_root` (escapes via `../` or
/// symlinks are rejected), returning it with a git origin + fingerprint.
async fn read_repo_file(
    repo_root: &Path,
    path: &Path,
    git: &str,
    git_ref: &str,
) -> Result<(String, Origin, String), String> {
    let root = repo_root
        .canonicalize()
        .map_err(|e| format!("git cache dir '{}' vanished: {e}", repo_root.display()))?;
    let canonical = path.canonicalize().map_err(|e| {
        format!(
            "import: '{}' not found at {git}@{git_ref}: {e}",
            path.display()
        )
    })?;
    if !canonical.starts_with(&root) {
        return Err(format!(
            "import '{}': path escapes the repository root — imports from a git \
             source must stay inside that repository",
            path.display()
        ));
    }
    let text = tokio::fs::read_to_string(&canonical)
        .await
        .map_err(|e| format!("import '{}': {e}", canonical.display()))?;
    let rel = canonical
        .strip_prefix(&root)
        .expect("starts_with checked above")
        .to_path_buf();
    let dir = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.clone());
    let fp = format!("{git}#{git_ref}:{}", rel.display());
    Ok((
        text,
        Origin::Git {
            git: git.to_string(),
            git_ref: git_ref.to_string(),
            repo_root: root,
            dir,
        },
        fp,
    ))
}

fn fingerprint_local(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

async fn fetch_url(url: reqwest::Url) -> Result<(String, Origin, String), String> {
    let fp = url.as_str().to_string();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("import '{fp}': http client: {e}"))?;
    let resp = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| format!("import '{fp}': request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("import '{fp}': HTTP {status}"));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("import '{fp}': read failed: {e}"))?;
    if body.len() > MAX_HTTP_BODY {
        return Err(format!(
            "import '{fp}': body exceeds {} bytes",
            MAX_HTTP_BODY
        ));
    }
    let text = String::from_utf8(body.to_vec())
        .map_err(|_| format!("import '{fp}': body is not valid UTF-8"))?;
    Ok((text, Origin::Url { base: url }, fp))
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct CacheMeta {
    /// "sha" | "tag" | "branch"
    kind: String,
    /// Resolved commit SHA of the cached checkout.
    sha: String,
    /// Unix seconds of the last fetch/revalidation.
    fetched_at: u64,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn default_cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PERFSCALE_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("perfscale");
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache").join("perfscale")
}

fn cache_entry_dir(opts: &ImportOptions, remote: &GitImport) -> PathBuf {
    let root = opts
        .cache_dir
        .clone()
        .unwrap_or_else(default_cache_dir)
        .join("imports");
    let mut hasher = Sha256::new();
    hasher.update(remote.git.as_bytes());
    hasher.update(b"\n");
    hasher.update(remote.git_ref.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    root.join(hex)
}

fn looks_like_sha(git_ref: &str) -> bool {
    git_ref.len() >= 7
        && git_ref.len() <= 40
        && git_ref.chars().all(|c| c.is_ascii_hexdigit())
        // All-digit strings are more likely a tag like "20260811".
        && git_ref.chars().any(|c| c.is_ascii_alphabetic())
        || git_ref.len() == 40 && git_ref.chars().all(|c| c.is_ascii_hexdigit())
}

/// Run `git <args>`, returning trimmed stdout; errors carry trimmed stderr.
async fn git(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0") // fail instead of prompting for creds
        .stdin(std::process::Stdio::null());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "git binary not found on PATH — git imports shell out to your system git".to_string()
        } else {
            format!("failed to spawn git: {e}")
        }
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `git ls-remote` classification of a ref: (kind, remote sha).
async fn ls_remote(git_url: &str, git_ref: &str) -> Result<(&'static str, String), String> {
    let out = git(&["ls-remote", "--tags", "--heads", git_url, git_ref], None).await?;
    let mut branch_sha = None;
    let mut tag_sha = None;
    let mut peeled_tag_sha = None;
    for line in out.lines() {
        let Some((sha, name)) = line.split_once('\t') else {
            continue;
        };
        if name == format!("refs/heads/{git_ref}") {
            branch_sha = Some(sha.to_string());
        } else if name == format!("refs/tags/{git_ref}") {
            tag_sha = Some(sha.to_string());
        } else if name == format!("refs/tags/{git_ref}^{{}}") {
            peeled_tag_sha = Some(sha.to_string());
        }
    }
    if let Some(sha) = peeled_tag_sha.or(tag_sha) {
        return Ok(("tag", sha));
    }
    if let Some(sha) = branch_sha {
        return Ok(("branch", sha));
    }
    Err(format!(
        "ref '{git_ref}' not found at '{git_url}' (no matching branch or tag)"
    ))
}

/// Materialize `remote` in the cache and return the checkout directory.
async fn git_fetch(remote: &GitImport, opts: &ImportOptions) -> Result<PathBuf, String> {
    let entry = cache_entry_dir(opts, remote);
    let repo = entry.join("repo");
    let meta_path = entry.join("meta.json");

    if repo.is_dir() {
        if let Some(meta) = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|t| serde_json::from_str::<CacheMeta>(&t).ok())
        {
            match meta.kind.as_str() {
                // Content-addressed: a SHA can never change meaning.
                "sha" => return Ok(repo),
                "tag" if !opts.refresh => return Ok(repo),
                "branch" if !opts.refresh => {
                    let age = now_unix().saturating_sub(meta.fetched_at);
                    if age <= BRANCH_TTL_SECS {
                        return Ok(repo);
                    }
                    // TTL expired: one cheap ls-remote decides reuse vs refetch.
                    match ls_remote(&remote.git, &remote.git_ref).await {
                        Ok((_, sha)) if sha == meta.sha => {
                            write_meta(&meta_path, &meta.kind, &sha);
                            return Ok(repo);
                        }
                        Ok(_) => { /* moved — fall through to refetch */ }
                        Err(e) => {
                            // Offline: stale beats broken, but say so.
                            eprintln!(
                                "[sys] import: could not revalidate branch '{}' ({e}); \
                                 using cached checkout from {}s ago",
                                remote.git_ref, age
                            );
                            return Ok(repo);
                        }
                    }
                }
                _ => { /* refresh requested or unknown kind — refetch */ }
            }
        }
        let _ = std::fs::remove_dir_all(&entry);
    }

    // Fresh fetch into a temp sibling, then atomically move into place so a
    // concurrent run never sees a half-written checkout.
    let tmp = entry.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("cache dir '{}': {e}", tmp.display()))?;
    let tmp_repo = tmp.join("repo");

    let kind: &str;
    if looks_like_sha(&remote.git_ref) {
        kind = "sha";
        git(
            &["init", "--quiet", tmp_repo.to_str().unwrap_or_default()],
            None,
        )
        .await?;
        git(&["remote", "add", "origin", &remote.git], Some(&tmp_repo)).await?;
        git(
            &[
                "fetch",
                "--quiet",
                "--depth",
                "1",
                "origin",
                &remote.git_ref,
            ],
            Some(&tmp_repo),
        )
        .await
        .map_err(|e| {
            format!(
                "{e}\nhint: fetching a bare commit SHA requires the server to allow it \
                 (GitHub/GitLab do for reachable commits); use a tag or branch otherwise"
            )
        })?;
        git(
            &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
            Some(&tmp_repo),
        )
        .await?;
    } else {
        kind = match ls_remote(&remote.git, &remote.git_ref).await {
            Ok((kind, _)) => kind,
            // ls-remote can fail on hosts that restrict ref listing; fall
            // back to treating the ref as a branch (mutable → revalidated).
            Err(_) => "branch",
        };
        git(
            &[
                "clone",
                "--quiet",
                "--depth",
                "1",
                "--branch",
                &remote.git_ref,
                "--single-branch",
                &remote.git,
                tmp_repo.to_str().unwrap_or_default(),
            ],
            None,
        )
        .await?;
    }
    let sha = git(&["rev-parse", "HEAD"], Some(&tmp_repo)).await?;
    write_meta(&tmp.join("meta.json"), kind, &sha);

    if let Some(parent) = entry.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(&tmp, &entry) {
        Ok(()) => {}
        Err(_) if entry.join("repo").is_dir() => {
            // Lost a race with a concurrent run that fetched the same ref.
            let _ = std::fs::remove_dir_all(&tmp);
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("cache move '{}': {e}", entry.display()));
        }
    }
    Ok(repo)
}

fn write_meta(path: &Path, kind: &str, sha: &str) {
    let meta = CacheMeta {
        kind: kind.to_string(),
        sha: sha.to_string(),
        fetched_at: now_unix(),
    };
    if let Ok(json) = serde_json::to_string(&meta) {
        let _ = std::fs::write(path, json);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("perfscale-import-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn deep_merge_objects_recurse_arrays_replace() {
        let base = serde_json::json!({
            "vus": 5,
            "variables": { "region": "eu", "retries": 3 },
            "steps": [ { "use": "std/log@v1" } ]
        });
        let overlay = serde_json::json!({
            "vus": 10,
            "variables": { "region": "us" },
            "steps": [ { "use": "std/http@v1" }, { "use": "std/check@v1" } ]
        });
        let merged = deep_merge(base, overlay);
        assert_eq!(merged["vus"], 10);
        assert_eq!(merged["variables"]["region"], "us");
        assert_eq!(merged["variables"]["retries"], 3, "base keys survive");
        assert_eq!(
            merged["steps"].as_array().unwrap().len(),
            2,
            "arrays replace"
        );
    }

    #[test]
    fn import_spec_parses_all_three_forms() {
        let s: ImportSpec = serde_json::from_value(serde_json::json!("../base.yaml")).unwrap();
        assert!(matches!(s, ImportSpec::Path(p) if p == "../base.yaml"));

        let s: ImportSpec =
            serde_json::from_value(serde_json::json!("https://example.com/base.yaml")).unwrap();
        assert!(matches!(s, ImportSpec::Path(_)));

        let s: ImportSpec = serde_json::from_value(serde_json::json!({
            "git": "git@gitlab.example.com:group/repo.git",
            "ref": "v1.2.0",
            "file": "perf/config/_base.yaml"
        }))
        .unwrap();
        match s {
            ImportSpec::Git(g) => {
                assert_eq!(g.git_ref, "v1.2.0");
                assert_eq!(g.file, "perf/config/_base.yaml");
            }
            _ => panic!("expected git import"),
        }
    }

    #[test]
    fn import_spec_rejects_unknown_git_keys() {
        let err = serde_json::from_value::<ImportSpec>(serde_json::json!({
            "git": "x", "ref": "y", "file": "z", "branch": "oops"
        }));
        assert!(err.is_err());
    }

    #[test]
    fn looks_like_sha_heuristics() {
        assert!(looks_like_sha("4471101ab"));
        assert!(looks_like_sha(&"a".repeat(40)));
        assert!(!looks_like_sha("v1.2.0"));
        assert!(!looks_like_sha("main"));
        assert!(!looks_like_sha("20260811"), "all-digit refs read as tags");
        assert!(!looks_like_sha("abc"), "too short to be a SHA");
    }

    #[test]
    fn cache_key_is_stable_and_ref_sensitive() {
        let opts = ImportOptions::default();
        let a = |r: &str| GitImport {
            git: "git@example.com:o/r.git".into(),
            git_ref: r.into(),
            file: "f.yaml".into(),
        };
        assert_eq!(
            cache_entry_dir(&opts, &a("v1")),
            cache_entry_dir(&opts, &a("v1"))
        );
        assert_ne!(
            cache_entry_dir(&opts, &a("v1")),
            cache_entry_dir(&opts, &a("v2"))
        );
    }

    #[tokio::test]
    async fn local_import_merges_base_under_current() {
        let dir = tmpdir("local");
        fs::write(dir.join("_base.yaml"), "vus: 5\nvariables:\n  region: eu\n").unwrap();
        fs::write(
            dir.join("config.yaml"),
            "import: ./_base.yaml\nduration: 10s\nvariables:\n  region: us\n",
        )
        .unwrap();

        let (value, n) = load_document(&dir.join("config.yaml"), &ImportOptions::default())
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(value["vus"], 5, "inherited from base");
        assert_eq!(value["duration"], "10s");
        assert_eq!(value["variables"]["region"], "us", "local wins");
        assert!(value.get("import").is_none(), "import key consumed");
    }

    #[tokio::test]
    async fn local_import_chain_and_typed_load() {
        let dir = tmpdir("chain");
        fs::write(dir.join("a.yaml"), "vus: 2\n").unwrap();
        fs::write(dir.join("b.yaml"), "import: ./a.yaml\nduration: 5s\n").unwrap();
        fs::write(dir.join("c.yaml"), "import: ./b.yaml\nvus: 9\n").unwrap();

        let cfg = load_config_file(&dir.join("c.yaml"), &ImportOptions::default())
            .await
            .unwrap();
        assert_eq!(cfg.run.vus, 9);
        assert_eq!(cfg.run.duration, "5s");
    }

    #[tokio::test]
    async fn import_cycle_is_detected() {
        let dir = tmpdir("cycle");
        fs::write(dir.join("a.yaml"), "import: ./b.yaml\n").unwrap();
        fs::write(dir.join("b.yaml"), "import: ./a.yaml\n").unwrap();

        let err = load_document(&dir.join("a.yaml"), &ImportOptions::default())
            .await
            .unwrap_err();
        assert!(err.contains("cycle"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn self_import_is_a_cycle() {
        let dir = tmpdir("self");
        fs::write(dir.join("a.yaml"), "import: ./a.yaml\n").unwrap();
        let err = load_document(&dir.join("a.yaml"), &ImportOptions::default())
            .await
            .unwrap_err();
        assert!(err.contains("cycle"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn remote_url_import_blocked_by_default() {
        let dir = tmpdir("blocked-url");
        fs::write(
            dir.join("config.yaml"),
            "import: \"https://example.invalid/base.yaml\"\n",
        )
        .unwrap();
        let err = load_document(&dir.join("config.yaml"), &ImportOptions::default())
            .await
            .unwrap_err();
        assert!(
            err.contains("--allow-remote-import"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn remote_git_import_blocked_by_default() {
        let dir = tmpdir("blocked-git");
        fs::write(
            dir.join("config.yaml"),
            "import:\n  git: git@example.com:o/r.git\n  ref: v1\n  file: base.yaml\n",
        )
        .unwrap();
        let err = load_document(&dir.join("config.yaml"), &ImportOptions::default())
            .await
            .unwrap_err();
        assert!(
            err.contains("--allow-remote-import"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_document_import_composes_steps() {
        let dir = tmpdir("testdoc");
        fs::write(
            dir.join("_base.test.yaml"),
            "steps:\n  - use: std/log@v1\n    with: { message: base }\n",
        )
        .unwrap();
        fs::write(
            dir.join("t.test.yaml"),
            "import: ./_base.test.yaml\nsteps:\n  - use: std/http@v1\n    with: { url: https://example.com }\n",
        )
        .unwrap();

        let test = load_test_file(&dir.join("t.test.yaml"), &ImportOptions::default())
            .await
            .unwrap();
        // Arrays replace: the local steps list wins outright.
        assert_eq!(test.steps.len(), 1);
        assert_eq!(test.steps[0].action, "std/http@v1");

        // A test doc that only imports inherits the base's steps.
        fs::write(dir.join("only.test.yaml"), "import: ./_base.test.yaml\n").unwrap();
        let test = load_test_file(&dir.join("only.test.yaml"), &ImportOptions::default())
            .await
            .unwrap();
        assert_eq!(test.steps[0].action, "std/log@v1");
    }

    /// HTTP imports: served from a local axum server; relative import inside
    /// a URL-origin document resolves against that URL, and the same server
    /// exercises the merged result end to end.
    #[tokio::test]
    async fn http_import_with_relative_hop() {
        use axum::{routing::get, Router};

        let app = Router::new()
            .route(
                "/perf/base.yaml",
                get(|| async { "import: ./deeper.yaml\nvus: 3\n" }),
            )
            .route("/perf/deeper.yaml", get(|| async { "duration: 7s\n" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tmpdir("http");
        fs::write(
            dir.join("config.yaml"),
            format!("import: \"http://{addr}/perf/base.yaml\"\nvus: 8\n"),
        )
        .unwrap();

        let opts = ImportOptions {
            allow_remote: true,
            ..Default::default()
        };
        let (value, n) = load_document(&dir.join("config.yaml"), &opts)
            .await
            .unwrap();
        assert_eq!(n, 2, "URL base plus its relative hop");
        assert_eq!(value["vus"], 8, "local overrides URL base");
        assert_eq!(value["duration"], "7s", "inherited through the hop");
    }

    #[tokio::test]
    async fn http_import_404_is_reported() {
        use axum::Router;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, Router::new()).await.unwrap() });

        let dir = tmpdir("http404");
        fs::write(
            dir.join("config.yaml"),
            format!("import: \"http://{addr}/missing.yaml\"\n"),
        )
        .unwrap();
        let opts = ImportOptions {
            allow_remote: true,
            ..Default::default()
        };
        let err = load_document(&dir.join("config.yaml"), &opts)
            .await
            .unwrap_err();
        assert!(err.contains("404"), "unexpected error: {err}");
    }

    // -- git fixtures ------------------------------------------------------

    async fn git_available() -> bool {
        git(&["--version"], None).await.is_ok()
    }

    /// Fixture git: user/system config neutralized so host-level settings
    /// (signed tags, hooks, templates) cannot break the fixtures.
    async fn fgit(args: &[&str], cwd: &Path) {
        let out = tokio::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "fixture `git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Build a local repo with `_base.yaml` (+nested import) at tag v1 on
    /// branch main; returns (repo_url, workdir).
    async fn git_fixture(tag: &str) -> (String, PathBuf) {
        let dir = tmpdir(&format!("gitrepo-{tag}"));
        fgit(&["init", "--quiet", "-b", "main", "."], &dir).await;
        fs::create_dir_all(dir.join("perf")).unwrap();
        fs::write(
            dir.join("perf/_base.yaml"),
            "import: ./inner.yaml\nvus: 4\n",
        )
        .unwrap();
        fs::write(dir.join("perf/inner.yaml"), "variables:\n  region: eu\n").unwrap();
        fgit(&["add", "."], &dir).await;
        fgit(&["commit", "--quiet", "-m", "base"], &dir).await;
        fgit(&["tag", "v1"], &dir).await;
        (format!("file://{}", dir.display()), dir)
    }

    #[tokio::test]
    async fn git_import_tag_with_nested_relative_import() {
        if !git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (url, _work) = git_fixture("tag").await;
        let dir = tmpdir("git-tag-doc");
        fs::write(
            dir.join("config.yaml"),
            format!(
                "import:\n  git: \"{url}\"\n  ref: v1\n  file: perf/_base.yaml\nduration: 9s\n"
            ),
        )
        .unwrap();

        let opts = ImportOptions {
            allow_remote: true,
            cache_dir: Some(tmpdir("git-tag-cache")),
            ..Default::default()
        };
        let (value, n) = load_document(&dir.join("config.yaml"), &opts)
            .await
            .unwrap();
        assert_eq!(n, 2, "git base + its in-repo relative import");
        assert_eq!(value["vus"], 4);
        assert_eq!(value["variables"]["region"], "eu");
        assert_eq!(value["duration"], "9s");

        // Second resolve hits the cache: works with the remote deleted.
        let (value2, _) = load_document(&dir.join("config.yaml"), &opts)
            .await
            .unwrap();
        assert_eq!(value2["vus"], 4);
        let meta: CacheMeta = serde_json::from_str(
            &fs::read_to_string(
                cache_entry_dir(
                    &opts,
                    &GitImport {
                        git: url.clone(),
                        git_ref: "v1".into(),
                        file: "perf/_base.yaml".into(),
                    },
                )
                .join("meta.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(meta.kind, "tag");
    }

    #[tokio::test]
    async fn git_import_branch_revalidates_after_ttl() {
        if !git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (url, work) = git_fixture("branch").await;
        let dir = tmpdir("git-branch-doc");
        fs::write(
            dir.join("config.yaml"),
            format!("import:\n  git: \"{url}\"\n  ref: main\n  file: perf/inner.yaml\n"),
        )
        .unwrap();
        let opts = ImportOptions {
            allow_remote: true,
            cache_dir: Some(tmpdir("git-branch-cache")),
            ..Default::default()
        };

        let (value, _) = load_document(&dir.join("config.yaml"), &opts)
            .await
            .unwrap();
        assert_eq!(value["variables"]["region"], "eu");

        // Advance the branch, expire the TTL by rewriting meta, resolve again.
        fs::write(work.join("perf/inner.yaml"), "variables:\n  region: us\n").unwrap();
        fgit(&["add", "."], &work).await;
        fgit(&["commit", "--quiet", "-m", "move"], &work).await;

        let entry = cache_entry_dir(
            &opts,
            &GitImport {
                git: url.clone(),
                git_ref: "main".into(),
                file: "perf/inner.yaml".into(),
            },
        );
        let meta_path = entry.join("meta.json");
        let mut meta: CacheMeta =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        assert_eq!(meta.kind, "branch");
        meta.fetched_at = 1; // long past the TTL
        fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        let (value, _) = load_document(&dir.join("config.yaml"), &opts)
            .await
            .unwrap();
        assert_eq!(
            value["variables"]["region"], "us",
            "expired branch cache must pick up the new commit"
        );
    }

    #[tokio::test]
    async fn git_import_rejects_repo_escape() {
        if !git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (url, _work) = git_fixture("escape").await;
        let dir = tmpdir("git-escape-doc");
        fs::write(
            dir.join("config.yaml"),
            format!("import:\n  git: \"{url}\"\n  ref: v1\n  file: ../../etc/passwd\n"),
        )
        .unwrap();
        let opts = ImportOptions {
            allow_remote: true,
            cache_dir: Some(tmpdir("git-escape-cache")),
            ..Default::default()
        };
        let err = load_document(&dir.join("config.yaml"), &opts)
            .await
            .unwrap_err();
        assert!(
            err.contains("escape") || err.contains("not found"),
            "unexpected error: {err}"
        );
    }
}
