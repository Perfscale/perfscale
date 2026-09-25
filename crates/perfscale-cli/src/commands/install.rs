//! `perfscale install` — fetch remote libraries and pin them (RFC 005 phase 3).
//!
//! For every `libraries[].use` that names a remote source (`https://…` or
//! `git+<repo>@<ref>#<path>`) in the given documents — including documents
//! pulled in through `import:` — this fetches the artifact once, verifies
//! its digest, stores it in the content-addressed cache
//! (`<cache>/libraries/<sha256>.wasm`), and writes/updates `perfscale.lock`
//! next to the declaring document. `run`/`lint` afterwards work fully
//! offline against lock + cache.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use perfscale_core::import::{self, CollectedLibrary, GitImport, ImportOptions};
use perfscale_core::library::lockfile::{LockEntry, Lockfile};
use perfscale_core::library::{normalize_sha256, parse_git_library_ref};
use sha2::{Digest, Sha256};

use crate::cli::InstallArgs;
use crate::error::CliError;

pub async fn run(args: InstallArgs) -> Result<(), CliError> {
    let collected: Arc<Mutex<Vec<CollectedLibrary>>> = Arc::new(Mutex::new(Vec::new()));
    // Install is the network operation: imports are followed so libraries
    // declared in remote base documents are pinned too (their lockfile
    // belongs to the imported repository's root). Resolution itself is off —
    // collecting is the whole point.
    let opts = ImportOptions {
        allow_remote: true,
        refresh: args.refresh,
        resolve_libraries: false,
        collect_libraries: Some(collected.clone()),
        ..Default::default()
    };

    for path in &args.files {
        import::load_document(path, &opts).await.map_err(|e| {
            CliError::new(format!("failed to load '{}': {e}", path.display()))
                .hint("`perfscale install` expects the same YAML documents `run`/`lint` take")
                .docs("yaml-reference.md#libraries")
        })?;
    }

    let gathered = std::mem::take(
        &mut *collected
            .lock()
            .map_err(|_| CliError::new("internal error: library collection poisoned"))?,
    );
    if gathered.is_empty() {
        println!("no remote libraries to install");
        return Ok(());
    }

    // Dedup (same library imported through several files), then group by
    // declaring directory: one lockfile write per directory.
    let mut seen = HashSet::new();
    let mut by_dir: BTreeMap<PathBuf, Vec<CollectedLibrary>> = BTreeMap::new();
    for lib in gathered {
        if seen.insert((lib.declaring_dir.clone(), lib.use_.clone())) {
            by_dir
                .entry(lib.declaring_dir.clone())
                .or_default()
                .push(lib);
        }
    }

    let cache_root = import::library_cache_root(&opts);
    for (dir, libs) in by_dir {
        let mut lock = Lockfile::load(&dir)
            .map_err(CliError::new)?
            .unwrap_or_default();
        for lib in &libs {
            install_one(lib, &mut lock, &cache_root, &opts).await?;
        }
        let path = lock.save(&dir).map_err(CliError::new)?;
        println!(
            "wrote {} ({} libraries)",
            path.display(),
            lock.libraries.len()
        );
    }
    Ok(())
}

/// Fetch, verify, cache, and pin one remote library.
async fn install_one(
    lib: &CollectedLibrary,
    lock: &mut Lockfile,
    cache_root: &Path,
    opts: &ImportOptions,
) -> Result<(), CliError> {
    if lib.use_.starts_with("http://") || lib.use_.starts_with("https://") {
        install_https(lib, lock, cache_root).await
    } else if lib.use_.starts_with("git+") {
        install_git(lib, lock, cache_root, opts).await
    } else {
        Err(CliError::new(format!(
            "library '{}': not a remote source",
            lib.use_
        )))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

async fn install_https(
    lib: &CollectedLibrary,
    lock: &mut Lockfile,
    cache_root: &Path,
) -> Result<(), CliError> {
    let declared = lib.sha256.as_deref().ok_or_else(|| {
        CliError::new(format!(
            "library '{}': https sources require a `sha256:` field (64 hex) in the libraries: entry",
            lib.use_
        ))
        .hint("pin the artifact you reviewed: sha256 of the .wasm, e.g. `shasum -a 256 lib.wasm`")
        .docs("yaml-reference.md#libraries")
    })?;
    let declared = normalize_sha256(&lib.use_, declared).map_err(CliError::new)?;

    let already_pinned = lock.find(&lib.use_).is_some_and(|e| e.sha256 == declared)
        && import::library_artifact_path(cache_root, &declared).is_file();
    if already_pinned {
        println!("installed {} ({}…, up to date)", lib.use_, short(&declared));
        return Ok(());
    }

    let bytes = fetch_bytes(&lib.use_).await?;
    let actual = sha256_hex(&bytes);
    if actual != declared {
        return Err(CliError::new(format!(
            "library '{}': sha256 mismatch — the YAML declares {declared} but the fetched artifact hashes to {actual}",
            lib.use_
        ))
        .hint("the publisher re-released under the same URL, or the pin is stale — verify which, then update the YAML")
        .docs("yaml-reference.md#libraries"));
    }
    import::write_library_artifact(cache_root, &actual, &bytes).map_err(CliError::new)?;
    lock.upsert(LockEntry {
        use_: lib.use_.clone(),
        commit: None,
        sha256: actual.clone(),
    });
    println!("installed {} ({}…)", lib.use_, short(&actual));
    Ok(())
}

async fn install_git(
    lib: &CollectedLibrary,
    lock: &mut Lockfile,
    cache_root: &Path,
    opts: &ImportOptions,
) -> Result<(), CliError> {
    let parsed = parse_git_library_ref(&lib.use_).map_err(CliError::new)?;

    // Without --refresh the lock is the truth: a pinned commit with the
    // artifact in cache needs no network at all. Declared sha256 (optional
    // for git) is still verified against the pinned digest.
    if !opts.refresh {
        if let Some(existing) = lock.find(&lib.use_) {
            if let Some(declared) = &lib.sha256 {
                let declared = normalize_sha256(&lib.use_, declared).map_err(CliError::new)?;
                if declared != existing.sha256 {
                    return Err(CliError::new(format!(
                        "library '{}': sha256 mismatch — the YAML declares {declared} but {} pins {}",
                        lib.use_,
                        perfscale_core::library::lockfile::LOCKFILE_NAME,
                        existing.sha256
                    ))
                    .hint("verify the publisher, then re-run with --refresh to re-resolve the ref"));
                }
            }
            if import::library_artifact_path(cache_root, &existing.sha256).is_file() {
                println!(
                    "installed {} ({}…, up to date)",
                    lib.use_,
                    short(&existing.sha256)
                );
                return Ok(());
            }
        }
    }

    let remote = GitImport {
        git: parsed.repo.clone(),
        git_ref: parsed.git_ref.clone(),
        file: parsed.path.clone(),
    };
    let repo_root = import::git_fetch(&remote, opts).await.map_err(|e| {
        CliError::new(format!("library '{}': git fetch failed: {e}", lib.use_))
            .hint("git sources shell out to your system git — check the URL, the ref, and your credentials (SSH agent / credential helper)")
            .docs("yaml-reference.md#libraries")
    })?;
    let bytes = import::read_repo_artifact(&repo_root, &parsed.path)
        .await
        .map_err(|e| CliError::new(format!("library '{}': {e}", lib.use_)))?;
    let actual = sha256_hex(&bytes);

    if let Some(declared) = &lib.sha256 {
        let declared = normalize_sha256(&lib.use_, declared).map_err(CliError::new)?;
        if declared != actual {
            return Err(CliError::new(format!(
                "library '{}': sha256 mismatch — the YAML declares {declared} but the artifact at {}@{} hashes to {actual}",
                lib.use_, parsed.repo, parsed.git_ref
            ))
            .hint("the ref moved or the pin is stale — verify which, then update the YAML"));
        }
    }

    // Pin the commit the ref resolved to (tags/branches are mutable;
    // commits are not).
    let (_, commit) = import::ls_remote(&parsed.repo, &parsed.git_ref)
        .await
        .unwrap_or_else(|_| ("sha", parsed.git_ref.clone()));
    import::write_library_artifact(cache_root, &actual, &bytes).map_err(CliError::new)?;
    lock.upsert(LockEntry {
        use_: lib.use_.clone(),
        commit: Some(commit),
        sha256: actual.clone(),
    });
    println!("installed {} ({}…)", lib.use_, short(&actual));
    Ok(())
}

/// GET an artifact over HTTP(S), capped like the import fetcher.
async fn fetch_bytes(url: &str) -> Result<Vec<u8>, CliError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| CliError::new(format!("http client: {e}")))?;
    let resp = client.get(url).send().await.map_err(|e| {
        CliError::new(format!("library '{url}': request failed: {e}"))
            .hint("check the URL and your network")
    })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(CliError::new(format!("library '{url}': HTTP {status}")));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| CliError::new(format!("library '{url}': read failed: {e}")))?;
    const MAX_ARTIFACT: usize = 64 * 1024 * 1024;
    if bytes.len() > MAX_ARTIFACT {
        return Err(CliError::new(format!(
            "library '{url}': artifact exceeds {MAX_ARTIFACT} bytes"
        )));
    }
    Ok(bytes.to_vec())
}
