//! `perfscale burn` — build a standalone binary with the WASM libraries of
//! the given documents embedded (RFC 005 burn, phase 2).
//!
//! The output is a byte-copy of the running executable plus an appended
//! payload of `.cwasm` burn artifacts behind a fixed-size `PFSEMBED` trailer
//! (see `perfscale_core::library::burn` for both formats). At startup the
//! derived binary reads its own trailer and resolves every declared library
//! from itself, so it runs with zero `.wasm`/cache files on disk. Burn
//! artifacts are tied to the wasmtime version, target triple, and engine
//! configuration of the build that produced them — re-run `perfscale burn`
//! after upgrading perfscale.

use std::io::Write;
use std::path::Path;

use perfscale_core::import::{self, ImportOptions};
use perfscale_core::library::burn;
use sha2::{Digest, Sha256};

use crate::cli::BurnArgs;
use crate::error::CliError;

pub async fn run(args: BurnArgs) -> Result<(), CliError> {
    // Offline, like `run`: remote refs resolve through perfscale.lock + the
    // artifact cache; anything missing is a load error pointing at install.
    let opts = ImportOptions::default();
    let mut uses: Vec<String> = Vec::new();
    for path in args.files.iter().chain(args.config.iter()) {
        let (value, _) = import::load_document(path, &opts).await.map_err(|e| {
            CliError::new(format!("failed to load '{}': {e}", path.display()))
                .hint("`perfscale burn` takes the same YAML documents as `run` — remote libraries must be installed first (`perfscale install`)")
                .docs("yaml-reference.md#libraries")
        })?;
        for entry in value
            .get("libraries")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let Some(use_) = entry.get("use").and_then(|v| v.as_str()) else {
                continue;
            };
            // Built-ins are native code already — nothing to embed.
            if use_.starts_with('@') || uses.iter().any(|u| u == use_) {
                continue;
            }
            uses.push(use_.to_string());
        }
    }
    if uses.is_empty() {
        return Err(CliError::new("no WASM libraries declared in the given documents")
            .hint("`perfscale burn` embeds the `libraries:` entries of -f/-c documents (local .wasm paths, or remote refs resolved via perfscale.lock); built-ins like @std/random@v1 need no embedding")
            .docs("yaml-reference.md#libraries"));
    }

    let cache_root = import::default_library_cache_root();
    let mut entries: Vec<(String, burn::EmbeddedLibrary)> = Vec::with_capacity(uses.len());
    for use_ in &uses {
        let bytes = std::fs::read(use_).map_err(|e| {
            CliError::new(format!("library '{use_}': failed to read: {e}"))
                .hint("remote libraries resolve to the cache via `perfscale install`; local paths must exist")
                .docs("yaml-reference.md#libraries")
        })?;
        let sha = burn::sha256_hex(&bytes);
        ensure_burned(use_, &sha, &bytes, &cache_root).map_err(|e| {
            CliError::new(e)
                .hint("a library must compile as a WASM component to be embedded")
                .docs("yaml-reference.md#libraries")
        })?;
        let artifact = std::fs::read(import::library_burn_path(&cache_root, &sha))
            .map_err(|e| CliError::new(format!("library '{use_}': burn artifact vanished: {e}")))?;
        entries.push((
            use_.clone(),
            burn::EmbeddedLibrary {
                source_sha256: burn::sha256_bytes(&bytes),
                artifact,
            },
        ));
    }

    let me = std::env::current_exe()
        .map_err(|e| CliError::new(format!("cannot locate the running executable: {e}")))?;
    let out = &args.output;
    if same_file(&me, out) {
        return Err(CliError::new(format!(
            "output '{}' is the running executable — pick a different -o path",
            out.display()
        )));
    }
    std::fs::copy(&me, out).map_err(|e| {
        CliError::new(format!("failed to copy the executable to '{}': {e}", out.display()))
    })?;
    // Stat the copy, not the original: metadata + copy would be two opens of
    // the same path, and a concurrent rebuild swapping the binary in between
    // would record a stale payload offset.
    let offset = std::fs::metadata(out)
        .map_err(|e| CliError::new(format!("cannot stat '{}': {e}", out.display())))?
        .len();

    let payload = burn::encode_embedded_payload(&entries);
    let trailer = burn::encode_embedded_trailer(&payload, offset);
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(out)
            .map_err(|e| CliError::new(format!("failed to open '{}': {e}", out.display())))?;
        file.write_all(&payload)
            .and_then(|()| file.write_all(&trailer))
            .map_err(|e| CliError::new(format!("failed to append to '{}': {e}", out.display())))?;
    }

    let image = std::fs::read(out)
        .map_err(|e| CliError::new(format!("failed to read back '{}': {e}", out.display())))?;
    let digest = Sha256::digest(&image);
    println!(
        "burned {} librar{} into {}",
        entries.len(),
        if entries.len() == 1 { "y" } else { "ies" },
        out.display()
    );
    for (use_, _) in &entries {
        println!("  embedded {use_}");
    }
    println!(
        "sha256: {}",
        digest.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    Ok(())
}

/// Same canonical path (the running executable)? `current_exe` may be a
/// symlink, so compare canonicalized where possible.
fn same_file(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// Guarantee a valid `.cwasm` burn artifact for `bytes` exists in the cache,
/// burning when missing or stale (header mismatch — e.g. after a perfscale
/// upgrade). Idempotent; shared by `perfscale install`. Errors are plain
/// strings: `install` downgrades them to a warning (a fetched artifact that
/// is not a valid component was installable before burn existed), while
/// `perfscale burn` treats them as fatal.
pub(crate) fn ensure_burned(
    use_: &str,
    sha256: &str,
    bytes: &[u8],
    cache_root: &Path,
) -> Result<(), String> {
    let short = sha256.get(..12).unwrap_or(sha256);
    let dest = import::library_burn_path(cache_root, sha256);
    if let Ok(existing) = std::fs::read(&dest) {
        if burn::artifact_matches(&existing, &burn::sha256_bytes(bytes)) {
            println!("burned {use_} ({short}…, up to date)");
            return Ok(());
        }
    }
    let artifact = burn::burn_component(bytes)
        .map_err(|e| format!("library '{use_}': {e}"))?;
    import::write_library_burn(cache_root, sha256, &artifact)?;
    println!("burned {use_} ({short}…)");
    Ok(())
}
