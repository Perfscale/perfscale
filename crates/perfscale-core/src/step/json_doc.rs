//! Parsed-JSON document cache shared by the `std/read-json@v1` step and the
//! `@std/json@v1` library — one cache and one disk-read counter, so a file
//! read through either surface is a hit for the other.
//!
//! Entries are keyed by path and revalidated against the file's
//! `(mtime, size)` on every access: the first read pays disk + parse, every
//! following access is served from RAM, and a file edited between runs of a
//! long-lived agent is picked up via a cheap stat.
//!
//! Two loaders wrap the same cache: [`load_async`] (tokio, for the step) and
//! [`load_sync`] (std, for the synchronous library call path). Filesystem
//! access is fail-closed through [`confine_fs`] — the same gate
//! `std/file-read@v1` uses.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use serde_json::Value;

use super::context::Context;

type JsonCacheEntry = (Option<SystemTime>, u64, Arc<Value>);

fn json_cache() -> &'static Mutex<std::collections::HashMap<String, JsonCacheEntry>> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, JsonCacheEntry>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Actual disk reads performed through this cache — observable cache
/// behaviour for tests; costs one relaxed increment per miss.
pub(crate) static JSON_DISK_READS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Gate and confine a filesystem path — the flag/root form of
/// [`super::actions::confine_fs_path`], so library calls (which see a
/// `CallCtx`, not a `Context`) enforce the identical contract.
///
/// Fail-closed: returns `Err` unless the run opted in via
/// `allow_file_actions`. When `fs_root` is set, the canonicalized path must
/// stay under the canonicalized root, rejecting `../` escapes and symlink
/// hops out of the sandbox. Write targets may not exist yet, so their parent
/// directory is canonicalized instead (`for_write`).
pub(crate) fn confine_fs(
    allow_file_actions: bool,
    fs_root: Option<&std::path::Path>,
    path: &str,
    for_write: bool,
) -> Result<std::path::PathBuf, String> {
    if !allow_file_actions {
        return Err("file actions disabled (allow_file_actions is false)".into());
    }
    let Some(root) = fs_root else {
        return Ok(std::path::PathBuf::from(path));
    };
    let root = std::fs::canonicalize(root)
        .map_err(|e| format!("fs_root '{}' cannot be resolved: {e}", root.display()))?;
    let candidate = if for_write {
        let p = std::path::Path::new(path);
        let parent = p
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let canon_parent = std::fs::canonicalize(parent)
            .map_err(|e| format!("cannot resolve directory of '{path}': {e}"))?;
        let Some(name) = p.file_name() else {
            return Err(format!("'{path}' is not a file path"));
        };
        canon_parent.join(name)
    } else {
        std::fs::canonicalize(path).map_err(|e| format!("cannot resolve '{path}': {e}"))?
    };
    if !candidate.starts_with(&root) {
        return Err(format!(
            "path '{path}' escapes fs_root '{}'",
            root.display()
        ));
    }
    Ok(candidate)
}

/// A cache hit: the stored entry matches the file's current `(mtime, len)`.
fn cached(key: &str, mtime: Option<SystemTime>, len: u64) -> Option<Arc<Value>> {
    let cache = json_cache().lock().unwrap();
    cache
        .get(key)
        .and_then(|(m, l, doc)| (*m == mtime && *l == len).then(|| Arc::clone(doc)))
}

/// Parse `bytes` and insert under `key` (the tail of both loaders).
fn parse_and_cache(
    key: String,
    mtime: Option<SystemTime>,
    len: u64,
    bytes: &[u8],
    display: &std::path::Path,
) -> Result<Arc<Value>, String> {
    let doc = match serde_json::from_slice::<Value>(bytes) {
        Ok(v) => Arc::new(v),
        Err(e) => {
            return Err(format!(
                "file '{}' is not valid JSON: {e}",
                display.display()
            ))
        }
    };
    json_cache()
        .lock()
        .unwrap()
        .insert(key, (mtime, len, Arc::clone(&doc)));
    Ok(doc)
}

/// The step's loader — tokio I/O on the miss path.
pub(crate) async fn load_async(path: &str, ctx: &Context) -> Result<Arc<Value>, String> {
    let path = confine_fs(ctx.allow_file_actions, ctx.fs_root.as_deref(), path, false)?;
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| format!("cannot read file '{}': {e}", path.display()))?;
    let (mtime, len) = (meta.modified().ok(), meta.len());
    let key = path.to_string_lossy().into_owned();
    // Never hold the lock across an await: check-release, read, insert.
    if let Some(doc) = cached(&key, mtime, len) {
        return Ok(doc);
    }
    JSON_DISK_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("cannot read file '{}': {e}", path.display()))?;
    parse_and_cache(key, mtime, len, &bytes, &path)
}

/// The library's loader — library calls are synchronous, so this uses
/// blocking std I/O (the miss path only; hits never touch the disk).
pub(crate) fn load_sync(
    path: &str,
    allow_file_actions: bool,
    fs_root: Option<&std::path::Path>,
) -> Result<Arc<Value>, String> {
    let path = confine_fs(allow_file_actions, fs_root, path, false)?;
    let meta = std::fs::metadata(&path)
        .map_err(|e| format!("cannot read file '{}': {e}", path.display()))?;
    let (mtime, len) = (meta.modified().ok(), meta.len());
    let key = path.to_string_lossy().into_owned();
    if let Some(doc) = cached(&key, mtime, len) {
        return Ok(doc);
    }
    JSON_DISK_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let bytes =
        std::fs::read(&path).map_err(|e| format!("cannot read file '{}': {e}", path.display()))?;
    parse_and_cache(key, mtime, len, &bytes, &path)
}
