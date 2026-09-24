//! Libraries — value generators behind `${alias.fn(...)}` tokens (RFC 005).
//!
//! A **library** provides functions that generate values inside `${…}`
//! generator tokens: ids (uuid4/uuid7/ULID/nanoid), faker-style data, dates —
//! anything a payload needs beyond the hardcoded `${seq}`/`${rand}`/…
//! built-ins. Libraries are declared in YAML (`libraries: - use:
//! @std/random@v1`), addressed through an alias (`${random.ulid()}`), and
//! validated before the run starts.
//!
//! Phase 1 shipped the native built-in `@std/random@v1`; phase 2 (feature
//! `wasm-libs`, always on in the CLI) adds WASM libraries: local `.wasm`
//! paths (relative to the declaring file) load as WASI Preview 2 components
//! under the fail-closed capability model (see the `wasm` module). HTTPS/git refs are
//! phase-3 distribution and are rejected at validation.
//!
//! # Token resolution
//!
//! Built-in tokens (`${seq}`, `${rand(a,b)}`, …) match first and keep their
//! exact behavior. After a built-in miss, a token containing a `.` is split
//! at the first dot: an **unknown alias** leaves the token verbatim
//! (backward compatible), a **known alias with an unknown function is a hard
//! error** — the alias is a declared contract and a typo must not ship
//! silently into a payload.
//!
//! # Call context and reuse
//!
//! Every call receives a [`CallCtx`]: the message sequence (same counter as
//! `${seq}`), the VU loop iteration, the VU id, the per-instance seed, and
//! the wall clock. Reuse within one message is library-side, via
//! [`MemoCache`]: a function's optional trailing `key` argument memoizes the
//! result per `message_seq`, so `${random.uuid4(order)}` appearing twice in
//! one message yields one value while `${random.uuid4()}` is always fresh.
//!
//! # Determinism
//!
//! An optional `seed:` in the config makes a run reproducible: per-instance
//! seeds derive as `hash(seed, vu_id, conn_seq)` (see [`derive_seed`]) and
//! libraries draw all entropy from a seeded PRNG. Without `seed:`, behavior
//! is as before (random). Wall-clock time stays non-deterministic by design.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub mod metrics;
pub mod std_random;
#[cfg(feature = "wasm-libs")]
pub mod wasm;

pub use metrics::{LibraryAliasSummary, LibraryMetrics};

/// Every library ref the engine can resolve — the complete list, named in
/// "unknown library" errors.
pub const AVAILABLE_LIBRARIES: &[&str] = &["@std/random@v1"];

/// Token names owned by the generator itself; a library alias may not shadow
/// them (built-ins match first, so the alias would silently never resolve).
pub const RESERVED_TOKEN_NAMES: &[&str] = &[
    "seq", "uuid", "now", "now_ms", "now_iso", "rand", "randf", "choice",
];

// ---------------------------------------------------------------------------
// Config surface
// ---------------------------------------------------------------------------

/// One `libraries:` entry — a value-generator library bound to a token alias.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LibraryRef {
    /// Library reference: `@std/random@v1` (built-in, native) or a local
    /// `.wasm` path (WASM component, relative to the declaring file's
    /// directory — resolved by `import:` handling; requires a build with the
    /// `wasm-libs` feature, which the CLI enables). HTTPS URLs and git refs
    /// are distribution sources (phase 3) and are rejected at validation.
    #[serde(rename = "use")]
    pub use_: String,

    /// Token alias — `${alias.fn(...)}`. Default: the library's name part
    /// (`random` for `@std/random@v1`). Must be a lowercase identifier and
    /// unique across all declared libraries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#as: Option<String>,

    /// Explicit capability grants, fail-closed: any non-empty grant requires
    /// `allow_library_capabilities: true` in the config. `fs` / `clock` are
    /// bare strings; network egress is an object with a host allowlist
    /// (`net: ["api.example.com"]`) — raw sockets are never granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<Capability>>,

    /// JSON config handed to the library at instantiation (`with:`). May use
    /// `${{ env.X }}` like any other parameter block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with: Option<serde_json::Value>,
}

/// A capability grant in a `libraries:` entry (RFC 005 capability model).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Capability {
    /// A bare grant: `fs`, `clock`.
    Simple(String),
    /// Network egress restricted to an allowlist of destination hosts.
    Net {
        /// Allowed destination hosts (`*` wildcards allowed).
        net: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// Provider contract — the trait external WASM libraries implement in phase 2
// ---------------------------------------------------------------------------

/// The context every library call receives (RFC 005 "Call context").
#[derive(Debug, Clone, Copy)]
pub struct CallCtx {
    /// Same counter as `${seq}` — bumped per message send. Memoization
    /// scopes to it: same `key` within one `message_seq` → same value.
    pub message_seq: u64,
    /// VU loop iteration (0 where the engine has not wired it yet).
    pub iteration_seq: u64,
    /// Virtual-user id, for per-VU partitioning (0 when unavailable).
    pub vu_id: u64,
    /// Per-instance deterministic seed (`hash(config.seed, vu_id, conn_seq)`
    /// when the config sets `seed:`, random otherwise).
    pub seed: u64,
    /// Wall clock, unix milliseconds. Libraries never read the clock
    /// themselves — this is the only time source they get.
    pub time_ms: u64,
}

/// One exported function, as listed by [`LibraryProvider::functions`].
#[derive(Debug, Clone, Copy)]
pub struct FunctionInfo {
    /// Function name as used in `${alias.name(...)}`.
    pub name: &'static str,
    /// One-line description for docs/lint output.
    pub description: &'static str,
    /// Results are secrets: the engine registers them for log masking.
    pub secret: bool,
}

/// A library as declared — mints instances. One provider per run; instances
/// are per generator owner (per VU, per live connection), parked alongside
/// the `Gen` they serve.
pub trait LibraryProvider: Send + Sync {
    /// Canonical id, e.g. `"@std/random@v1"`.
    fn id(&self) -> &str;

    /// Exported functions, for docs and lint-time token validation.
    fn functions(&self) -> &[FunctionInfo];

    /// Create one instance with its derived seed and optional `with:` config.
    fn instantiate(
        &self,
        config: Option<serde_json::Value>,
        seed: u64,
    ) -> Result<Box<dyn LibraryInstance>, String>;
}

/// One live library instance. Arguments and results map to payload strings:
/// args arrive as parsed JSON values (see the mapping contract in
/// [`crate::generate`]), the result is always a string.
pub trait LibraryInstance: Send {
    /// Invoke `func` with parsed JSON `args`. Unknown function names and bad
    /// arguments are `Err` — the engine fails the step with the message.
    fn call(
        &mut self,
        ctx: &CallCtx,
        func: &str,
        args: &[serde_json::Value],
    ) -> Result<String, String>;
}

// ---------------------------------------------------------------------------
// Keyed reuse within one message
// ---------------------------------------------------------------------------

/// Keyed value memoization scoped to one `message_seq` (RFC 005 "Call
/// context and reuse semantics"). The cache clears as soon as the sequence
/// moves on, so a memoized value never leaks into the next message. There is
/// deliberately no step/iteration/run-scoped state here — cross-step reuse
/// is `outputs:` + `${{ }}` interpolation, not library business.
#[derive(Debug, Default)]
pub struct MemoCache {
    current_seq: u64,
    map: HashMap<String, String>,
}

impl MemoCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached value for `key` in this message, computing and
    /// caching it on first use.
    pub fn get_or(
        &mut self,
        message_seq: u64,
        key: &str,
        f: impl FnOnce() -> Result<String, String>,
    ) -> Result<String, String> {
        if message_seq != self.current_seq {
            self.current_seq = message_seq;
            self.map.clear();
        }
        if let Some(v) = self.map.get(key) {
            return Ok(v.clone());
        }
        let v = f()?;
        self.map.insert(key.to_string(), v.clone());
        Ok(v)
    }
}

// ---------------------------------------------------------------------------
// Validation and resolution
// ---------------------------------------------------------------------------

/// A validated library binding: alias + provider + `with:` config, ready to
/// mint per-generator instances.
pub struct ResolvedLibrary {
    pub alias: String,
    pub provider: Arc<dyn LibraryProvider>,
    pub config: Option<serde_json::Value>,
}

impl std::fmt::Debug for ResolvedLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ResolvedLibrary({} as {})",
            self.provider.id(),
            self.alias
        )
    }
}

/// Validated, run-scoped library bindings — built once per run and shared by
/// every VU context; each `Gen` instantiates its own instances from these.
#[derive(Debug)]
pub struct LibrarySet {
    pub libraries: Vec<ResolvedLibrary>,
}

/// Resolve a built-in library id to its provider. `None` for anything that
/// is not a phase-1 built-in.
pub fn builtin_provider(id: &str) -> Option<Arc<dyn LibraryProvider>> {
    match id {
        "@std/random@v1" => Some(Arc::new(std_random::StdRandomProvider)),
        _ => None,
    }
}

/// Resolve a local `.wasm` path to a WASM provider (RFC 005 phase 2). The
/// path arrives anchored to the declaring file's directory (see
/// [`crate::import`]); the file must exist and carry a `.wasm` extension.
#[cfg(feature = "wasm-libs")]
fn resolve_wasm(
    lib: &LibraryRef,
    fs_root: Option<&std::path::Path>,
) -> Result<(Arc<dyn LibraryProvider>, String), String> {
    let path = std::path::Path::new(&lib.use_);
    if !path.exists() {
        return Err(format!(
            "library '{}': file not found (paths resolve relative to the declaring file's directory)",
            lib.use_
        ));
    }
    if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
        return Err(format!(
            "library '{}': expected a .wasm component file",
            lib.use_
        ));
    }
    let provider = wasm::WasmLibraryProvider::load(path, lib.capabilities.as_deref(), fs_root)?;
    let name = provider.library_name().to_string();
    Ok((Arc::new(provider), name))
}

/// Local `.wasm` paths without the runtime: a clear error instead of a
/// confusing "unknown library".
#[cfg(not(feature = "wasm-libs"))]
fn resolve_wasm(
    lib: &LibraryRef,
    _fs_root: Option<&std::path::Path>,
) -> Result<(Arc<dyn LibraryProvider>, String), String> {
    Err(format!(
        "library '{}': this perfscale build has no WASM support (feature wasm-libs) — use a perfscale CLI binary, which ships it",
        lib.use_
    ))
}

/// Validate declared `libraries:` and resolve them to providers. Catches —
/// before anything runs — unknown libraries, capability grants without
/// `allow_library_capabilities: true`, grants to libraries that declare no
/// capabilities, bad aliases, alias collisions (between libraries or with the
/// built-in token names), and (for WASM libraries) missing files, unsupported
/// WIT versions and capability grants narrower than the component's imports.
///
/// `fs_root` confines the `fs` capability preopen of WASM libraries
/// ([`crate::step::RunConfig::fs_root`]; lint passes `None`).
pub fn validate_libraries(
    refs: &[LibraryRef],
    allow_capabilities: bool,
    fs_root: Option<&std::path::Path>,
) -> Result<Vec<ResolvedLibrary>, String> {
    let mut resolved = Vec::with_capacity(refs.len());
    let mut aliases: HashSet<String> = HashSet::new();
    for lib in refs {
        let has_grants = lib.capabilities.as_ref().is_some_and(|c| !c.is_empty());
        let (provider, default_alias): (Arc<dyn LibraryProvider>, String) = if lib
            .use_
            .starts_with('@')
        {
            let name = parse_std_ref(&lib.use_).ok_or_else(|| {
                format!(
                    "library '{}': unknown library — available libraries: {}",
                    lib.use_,
                    AVAILABLE_LIBRARIES.join(", ")
                )
            })?;
            let provider = builtin_provider(&lib.use_).ok_or_else(|| {
                format!(
                    "library '{}': unknown library — available libraries: {}",
                    lib.use_,
                    AVAILABLE_LIBRARIES.join(", ")
                )
            })?;

            if has_grants {
                if !allow_capabilities {
                    return Err(format!(
                            "library '{}' grants capabilities but the config does not set `allow_library_capabilities: true` (fail-closed, like allow_file_actions)",
                            lib.use_
                        ));
                }
                // No phase-1 built-in declares capabilities; keep the
                // check provider-driven once one does.
                return Err(format!(
                    "library {} has no capabilities to grant",
                    provider.id()
                ));
            }
            (provider, name.to_string())
        } else {
            if lib.use_.starts_with("https://")
                || lib.use_.starts_with("http://")
                || lib.use_.starts_with("git+")
            {
                return Err(format!(
                        "library '{}': remote sources are fetched by `perfscale install` (phase 3, not yet available) — use a local .wasm path or a built-in @std/* library",
                        lib.use_
                    ));
            }
            if has_grants && !allow_capabilities {
                return Err(format!(
                        "library '{}' grants capabilities but the config does not set `allow_library_capabilities: true` (fail-closed, like allow_file_actions)",
                        lib.use_
                    ));
            }
            resolve_wasm(lib, fs_root)?
        };

        let alias = lib.r#as.clone().unwrap_or(default_alias);
        if !is_valid_alias(&alias) {
            return Err(format!(
                "library '{}': invalid alias '{alias}' — aliases match [a-z][a-z0-9_]*",
                lib.use_
            ));
        }
        if RESERVED_TOKEN_NAMES.contains(&alias.as_str()) {
            return Err(format!(
                "library '{}': alias '{alias}' collides with a built-in token name — pick another `as:`",
                lib.use_
            ));
        }
        if !aliases.insert(alias.clone()) {
            return Err(format!(
                "duplicate library alias '{alias}' — aliases must be unique across imported and importing files"
            ));
        }
        resolved.push(ResolvedLibrary {
            alias,
            provider,
            config: lib.with.clone(),
        });
    }
    Ok(resolved)
}

/// Parse `@std/name@vN` → the name part. Any other shape (unknown namespace,
/// missing/ malformed version) is `None`.
fn parse_std_ref(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("@std/")?;
    let (name, version) = rest.rsplit_once('@')?;
    if name.is_empty() {
        return None;
    }
    let major = version.strip_prefix('v')?;
    if major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(name)
}

/// Alias grammar: `[a-z][a-z0-9_]*` — lowercase identifiers, so tokens stay
/// visually distinct from built-ins and parse unambiguously at the first `.`.
fn is_valid_alias(alias: &str) -> bool {
    let mut chars = alias.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Per-instance seed derivation (RFC 005): `hash(seed, vu_id, conn_seq)` as
/// FNV-1a over the three words — deterministic, dependency-free. The result
/// is forced non-zero (xorshift degenerates at 0).
pub fn derive_seed(seed: u64, vu_id: u64, conn_seq: u64) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for word in [seed, vu_id, conn_seq] {
        for byte in word.to_le_bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn std_random_ref() -> LibraryRef {
        LibraryRef {
            use_: "@std/random@v1".into(),
            r#as: None,
            capabilities: None,
            with: None,
        }
    }

    #[test]
    fn validates_std_random_with_default_alias() {
        let resolved = validate_libraries(&[std_random_ref()], false, None).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].alias, "random");
        assert_eq!(resolved[0].provider.id(), "@std/random@v1");
    }

    #[test]
    fn rejects_remote_refs_as_phase_3() {
        for path in [
            "https://x.test/l.wasm",
            "http://x.test/l.wasm",
            "git+https://t/r@v1",
        ] {
            let mut r = std_random_ref();
            r.use_ = path.into();
            let err = validate_libraries(&[r], true, None).unwrap_err();
            assert!(err.contains("phase 3"), "{path} → {err}");
        }
    }

    #[test]
    fn local_wasm_paths_resolve_or_error_clearly() {
        let mut r = std_random_ref();
        r.use_ = "./libs/missing.wasm".into();
        let err = validate_libraries(&[r], true, None).unwrap_err();
        if cfg!(feature = "wasm-libs") {
            assert!(err.contains("file not found"), "{err}");
        } else {
            assert!(err.contains("no WASM support"), "{err}");
        }
    }

    #[test]
    fn rejects_unknown_std_library_and_wrong_major() {
        for id in [
            "@std/faker@v1",
            "@std/random@v2",
            "@foo/random@v1",
            "@std/random",
            "@std/@v1",
        ] {
            let mut r = std_random_ref();
            r.use_ = id.into();
            let err = validate_libraries(&[r], false, None).unwrap_err();
            assert!(
                err.contains("available libraries: @std/random@v1"),
                "{id} → {err}"
            );
        }
    }

    #[test]
    fn capabilities_need_the_global_gate() {
        let mut r = std_random_ref();
        r.capabilities = Some(vec![Capability::Simple("clock".into())]);
        let err = validate_libraries(&[r], false, None).unwrap_err();
        assert!(err.contains("allow_library_capabilities"), "{err}");
    }

    #[test]
    fn std_random_has_no_capabilities_to_grant() {
        let mut r = std_random_ref();
        r.capabilities = Some(vec![Capability::Simple("clock".into())]);
        let err = validate_libraries(&[r], true, None).unwrap_err();
        assert!(err.contains("has no capabilities to grant"), "{err}");
    }

    #[test]
    fn alias_grammar_is_enforced() {
        for bad in ["Random", "1x", "r-x", "r.x", ""] {
            let mut r = std_random_ref();
            r.r#as = Some(bad.into());
            let err = validate_libraries(&[r], false, None).unwrap_err();
            assert!(err.contains("invalid alias"), "{bad} → {err}");
        }
        let mut r = std_random_ref();
        r.r#as = Some("my_random2".into());
        assert!(validate_libraries(&[r], false, None).is_ok());
    }

    #[test]
    fn alias_must_not_shadow_builtin_tokens() {
        for name in RESERVED_TOKEN_NAMES {
            let mut r = std_random_ref();
            r.r#as = Some((*name).into());
            let err = validate_libraries(&[r], false, None).unwrap_err();
            assert!(err.contains("built-in token"), "{name} → {err}");
        }
    }

    #[test]
    fn duplicate_aliases_are_rejected() {
        let mut a = std_random_ref();
        a.r#as = Some("ids".into());
        let mut b = std_random_ref();
        b.r#as = Some("ids".into());
        let err = validate_libraries(&[a, b], false, None).unwrap_err();
        assert!(err.contains("duplicate library alias 'ids'"), "{err}");
    }

    #[test]
    fn memo_cache_repeats_within_a_message_and_clears_between() {
        let mut memo = MemoCache::new();
        let mut n = 0u64;
        let mut next = || {
            n += 1;
            Ok(n.to_string())
        };
        assert_eq!(memo.get_or(1, "k", &mut next).unwrap(), "1");
        assert_eq!(
            memo.get_or(1, "k", &mut next).unwrap(),
            "1",
            "same seq, same key"
        );
        assert_eq!(
            memo.get_or(1, "other", &mut next).unwrap(),
            "2",
            "other key is independent"
        );
        assert_eq!(
            memo.get_or(2, "k", &mut next).unwrap(),
            "3",
            "next message recomputes"
        );
    }

    #[test]
    fn derive_seed_is_deterministic_and_distinct() {
        assert_eq!(derive_seed(7, 1, 1), derive_seed(7, 1, 1));
        assert_ne!(derive_seed(7, 1, 1), derive_seed(7, 1, 2));
        assert_ne!(derive_seed(7, 1, 1), derive_seed(7, 2, 1));
        assert_ne!(derive_seed(7, 1, 1), derive_seed(8, 1, 1));
        assert_ne!(derive_seed(0, 0, 0) & 1, 0, "forced non-zero");
    }
}
