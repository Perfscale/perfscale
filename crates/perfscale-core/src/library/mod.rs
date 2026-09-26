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
//! under the fail-closed capability model (see the `wasm` module). Phase 3
//! adds distribution: HTTPS (`https://…/lib.wasm` + `sha256:`) and git
//! (`git+<repo>@<ref>#<path>`) sources are fetched once by
//! `perfscale install`, pinned in `perfscale.lock` (see the [`lockfile`]
//! module) next to the declaring file, and served from the content-addressed
//! cache at run/lint time — runs stay fully offline.
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

pub mod lockfile;
pub mod metrics;
pub mod std_random;
#[cfg(feature = "wasm-libs")]
pub mod burn;
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
    /// Library reference:
    ///
    /// - `@std/random@v1` — built-in, native;
    /// - a local `.wasm` path — WASM component, relative to the declaring
    ///   file's directory (resolved by `import:` handling; requires a build
    ///   with the `wasm-libs` feature, which the CLI enables);
    /// - `https://…/lib.wasm` — fetched by `perfscale install`; requires
    ///   `sha256:` and is pinned in `perfscale.lock`;
    /// - `git+<repo-url>@<ref>#<path>` — a `.wasm` artifact inside a git
    ///   repository at a tag/branch/commit, fetched by `perfscale install`
    ///   and pinned to the resolved commit in `perfscale.lock`.
    ///
    /// Remote sources are resolved to the local cache at load time, so
    /// `run`/`lint` stay fully offline.
    #[serde(rename = "use")]
    pub use_: String,

    /// SHA-256 of the artifact (64 lowercase hex characters). Required for
    /// `https://` sources — `perfscale install` refuses to fetch without it
    /// and a mismatch with the fetched bytes is a hard error. Optional for
    /// `git+` sources (the commit pin in `perfscale.lock` is the integrity
    /// anchor); when present it is verified against the fetched artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,

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

    /// Mark **every** result of this library as secret: each value a call
    /// returns is recorded in the run's secret registry and masked (`***`)
    /// in the run log. Additive on top of function-level `secret` flags —
    /// there is deliberately no way to *un*mask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<bool>,

    /// Whitelist of callable functions (`${alias.fn(...)}`). When present,
    /// calling a function not listed fails the step. `deny:` wins over
    /// `allow:` when a name appears in both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,

    /// Blacklist of functions that must not be called; a call fails the
    /// step. Wins over `allow:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,

    /// Functions whose results are always masked in the run log (same
    /// effect as the function declaring `secret: true` in `info()`, but
    /// decided by the config author).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<Vec<String>>,
}

/// Resolved call policy of one validated library entry (RFC 005
/// "secrets and policy rules"). Carried from [`validate_libraries`] to the
/// generator, which enforces it per call: `deny` first, then `allow`, and
/// result masking is the union of `secret` / `log` / the function's own
/// `secret` flag.
#[derive(Debug, Clone, Default)]
pub struct LibraryRules {
    /// Mask every result of the library.
    pub secret: bool,
    /// Whitelist (`None` = everything callable).
    pub allow: Option<Vec<String>>,
    /// Blacklist — wins over `allow`.
    pub deny: Vec<String>,
    /// Functions whose results are masked.
    pub log: Vec<String>,
}

impl LibraryRules {
    /// Why a call to `func` is blocked, or `None` when it may run.
    pub fn blocked(&self, alias: &str, func: &str) -> Option<String> {
        if self.deny.iter().any(|f| f == func) {
            return Some(format!(
                "function '{alias}.{func}' is blocked by the library entry's `deny:` list"
            ));
        }
        if let Some(allow) = &self.allow {
            if !allow.iter().any(|f| f == func) {
                return Some(format!(
                    "function '{alias}.{func}' is not in the library entry's `allow:` list"
                ));
            }
        }
        None
    }

    /// Whether results of `func` must be masked, given the function's own
    /// `secret` declaration.
    pub fn masks(&self, func: &str, function_secret: bool) -> bool {
        self.secret || function_secret || self.log.iter().any(|f| f == func)
    }
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
pub struct CallCtx<'a> {
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
    /// Run settings as a JSON object, frozen once at run start (same string
    /// for every call of the run):
    /// `{"vus": N|null, "duration_ms": N|null, "seed": N|null,
    ///   "stages": [...]|null, "arrival": {...}|null, "variables": {...}}`.
    /// `vus`/`duration_ms` are only set for the fixed load profile; staged
    /// and arrival-rate runs carry the profile in `stages`/`arrival`
    /// (durations normalized to milliseconds). `variables` are the config's
    /// `variables:` with `${{ env.* }}` resolved. `"{}"` when the caller
    /// wired nothing (hand-built generators, WIT 0.1 components).
    pub settings_json: &'a str,
}

impl CallCtx<'_> {
    /// The parsed run settings (`serde_json::Value::Null` when the frozen
    /// string does not parse — never a panic, but logged).
    pub fn settings(&self) -> serde_json::Value {
        match serde_json::from_str(self.settings_json) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "library CallCtx settings_json is invalid JSON ({e}); reporting null"
                );
                serde_json::Value::Null
            }
        }
    }
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
        ctx: &CallCtx<'_>,
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
    /// Call policy resolved from the entry's `secret:`/`allow:`/`deny:`/`log:`
    /// fields (defaults: everything callable, nothing extra masked).
    pub rules: LibraryRules,
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
/// [`crate::import`]); the file must exist and carry a `.wasm` extension —
/// unless a burned binary ([`burn::embedded`]) carries the library under
/// this exact `use:` string, in which case the file need not exist at all.
#[cfg(feature = "wasm-libs")]
fn resolve_wasm(
    lib: &LibraryRef,
    fs_root: Option<&std::path::Path>,
) -> Result<(Arc<dyn LibraryProvider>, String), String> {
    if let Some(embedded) = burn::embedded().as_ref().and_then(|s| s.get(&lib.use_)) {
        // Embedded libraries have no fallback: a header mismatch means the
        // binary was burned by another perfscale build.
        let component = burn::load_burned(&embedded.artifact, &embedded.source_sha256)
            .ok_or_else(|| {
                format!(
                    "library '{}': the embedded burn artifact does not match this perfscale build — re-create the binary with `perfscale burn` using this perfscale version",
                    lib.use_
                )
            })?;
        let provider =
            wasm::WasmLibraryProvider::load_embedded(&lib.use_, component, lib.capabilities.as_deref(), fs_root)?;
        let name = provider.library_name().to_string();
        return Ok((Arc::new(provider), name));
    }
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
            if is_remote_ref(&lib.use_) {
                return Err(format!(
                    "library '{}': remote sources must be resolved through perfscale.lock (perfscale install) — embedders: resolve via import::load_document",
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

        // Policy rules (RFC 005 phase 3.5): every name in allow/deny/log
        // must be a function the library actually exports — a typo'd rule
        // would silently never fire.
        let rules = LibraryRules {
            secret: lib.secret.unwrap_or(false),
            allow: lib.allow.clone(),
            deny: lib.deny.clone().unwrap_or_default(),
            log: lib.log.clone().unwrap_or_default(),
        };
        let exported: Vec<&str> = provider.functions().iter().map(|f| f.name).collect();
        for (field, names) in [
            ("allow", rules.allow.as_deref().unwrap_or(&[])),
            ("deny", rules.deny.as_slice()),
            ("log", rules.log.as_slice()),
        ] {
            for name in names {
                if !exported.contains(&name.as_str()) {
                    return Err(format!(
                        "library '{alias}': unknown function '{name}' in `{field}:` — available functions: {}",
                        exported.join(", ")
                    ));
                }
            }
        }

        resolved.push(ResolvedLibrary {
            alias,
            provider,
            config: lib.with.clone(),
            rules,
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

/// A `use:` source that lives on the network and therefore resolves through
/// `perfscale.lock` + the artifact cache (RFC 005 phase 3).
pub fn is_remote_ref(use_: &str) -> bool {
    use_.starts_with("https://") || use_.starts_with("http://") || use_.starts_with("git+")
}

/// Validate a declared `sha256:` digest, returning it normalized to
/// lowercase. Anything but 64 hex characters is an error naming the ref.
pub fn normalize_sha256(use_: &str, digest: &str) -> Result<String, String> {
    let lower = digest.to_ascii_lowercase();
    if lower.len() == 64 && lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(lower)
    } else {
        Err(format!(
            "library '{use_}': invalid sha256 '{digest}' — expected 64 hex characters"
        ))
    }
}

/// A parsed `git+<repo-url>@<ref>#<path>` library ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLibraryRef {
    /// Repository URL/remote (anything `git clone` accepts).
    pub repo: String,
    /// Tag, branch, or commit SHA.
    pub git_ref: String,
    /// Artifact path inside the repository (relative, confined to the repo).
    pub path: String,
}

/// Parse a `git+` library ref. The split is `rsplit_once('@')` so a
/// userinfo `@` inside the URL (`git@host:org/repo.git`,
/// `https://user@host/…`) is not mistaken for the ref separator. The
/// `#path` part is mandatory — a bare repo does not name an artifact.
pub fn parse_git_library_ref(use_: &str) -> Result<GitLibraryRef, String> {
    let Some(rest) = use_.strip_prefix("git+") else {
        return Err(format!(
            "library '{use_}': git refs use the form git+URL@ref#path/to/lib.wasm"
        ));
    };
    let Some((repo, ref_and_path)) = rest.rsplit_once('@') else {
        return Err(format!(
            "library '{use_}': git refs must pin a ref: git+URL@ref#path/to/lib.wasm"
        ));
    };
    let Some((git_ref, path)) = ref_and_path.split_once('#') else {
        return Err(format!(
            "library '{use_}': git library refs must name an artifact path: git+URL@ref#path/to/lib.wasm"
        ));
    };
    if repo.is_empty() || git_ref.is_empty() {
        return Err(format!(
            "library '{use_}': git refs must pin a ref: git+URL@ref#path/to/lib.wasm"
        ));
    }
    let rel = std::path::Path::new(path);
    if path.is_empty()
        || rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!(
            "library '{use_}': artifact path '{path}' escapes the repository root — \
             it must be a relative path inside the repository"
        ));
    }
    Ok(GitLibraryRef {
        repo: repo.to_string(),
        git_ref: git_ref.to_string(),
        path: path.to_string(),
    })
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
            sha256: None,
            r#as: None,
            capabilities: None,
            with: None,
            secret: None,
            allow: None,
            deny: None,
            log: None,
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
    fn rejects_unresolved_remote_refs() {
        // A remote ref reaching validate_libraries means the caller skipped
        // lock resolution (import::load_document) — a hard error, since the
        // engine itself never fetches.
        for path in [
            "https://x.test/l.wasm",
            "http://x.test/l.wasm",
            "git+https://t/r@v1#l.wasm",
        ] {
            let mut r = std_random_ref();
            r.use_ = path.into();
            let err = validate_libraries(&[r], true, None).unwrap_err();
            assert!(err.contains("perfscale.lock"), "{path} → {err}");
        }
    }

    #[test]
    fn git_library_ref_parses_userinfo_and_pins() {
        let r =
            parse_git_library_ref("git+https://github.com/org/repo.git@v1.2.3#lib.wasm").unwrap();
        assert_eq!(r.repo, "https://github.com/org/repo.git");
        assert_eq!(r.git_ref, "v1.2.3");
        assert_eq!(r.path, "lib.wasm");

        // SCP-style remote: the userinfo `@` must survive (rsplit at the
        // ref separator, not the first `@`).
        let r = parse_git_library_ref("git+git@gitlab.example.com:group/repo.git@main#libs/a.wasm")
            .unwrap();
        assert_eq!(r.repo, "git@gitlab.example.com:group/repo.git");
        assert_eq!(r.git_ref, "main");
        assert_eq!(r.path, "libs/a.wasm");

        // HTTPS with userinfo.
        let r = parse_git_library_ref("git+https://user@host/repo.git@v1#x.wasm").unwrap();
        assert_eq!(r.repo, "https://user@host/repo.git");
        assert_eq!(r.git_ref, "v1");
    }

    #[test]
    fn git_library_ref_rejects_bad_shapes() {
        // Missing #path.
        let err = parse_git_library_ref("git+https://t/r@v1").unwrap_err();
        assert!(err.contains("artifact path"), "{err}");
        // Missing @ref.
        let err = parse_git_library_ref("git+https://t/r#lib.wasm").unwrap_err();
        assert!(err.contains("pin a ref"), "{err}");
        // Empty path.
        let err = parse_git_library_ref("git+https://t/r@v1#").unwrap_err();
        assert!(err.contains("escapes the repository root"), "{err}");
        // `..` escape and absolute paths.
        for bad in [
            "git+https://t/r@v1#../x.wasm",
            "git+https://t/r@v1#a/../../x.wasm",
            "git+https://t/r@v1#/abs/x.wasm",
        ] {
            let err = parse_git_library_ref(bad).unwrap_err();
            assert!(err.contains("escapes the repository root"), "{bad} → {err}");
        }
    }

    #[test]
    fn sha256_normalization() {
        let upper = "A1".repeat(32);
        assert_eq!(
            normalize_sha256("https://x/l.wasm", &upper).unwrap(),
            "a1".repeat(32)
        );
        for bad in ["xyz", &"a".repeat(63), &"g".repeat(64)] {
            assert!(normalize_sha256("https://x/l.wasm", bad).is_err(), "{bad}");
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

    // --- policy rules: secret / allow / deny / log (RFC 005 phase 3.5) ----

    #[test]
    fn policy_rules_resolve_from_the_entry() {
        let mut r = std_random_ref();
        r.secret = Some(true);
        r.allow = Some(vec!["uuid4".into(), "ulid".into()]);
        r.deny = Some(vec!["uuid4".into()]);
        r.log = Some(vec!["ulid".into()]);
        let resolved = validate_libraries(&[r], false, None).unwrap();
        let rules = &resolved[0].rules;
        assert!(rules.secret);
        // deny wins over allow.
        assert!(rules.blocked("random", "uuid4").unwrap().contains("deny"));
        assert!(rules.blocked("random", "email").unwrap().contains("allow"));
        assert!(rules.blocked("random", "ulid").is_none());
        // Masking is the union of entry secret/log and the function flag.
        assert!(rules.masks("ulid", false));
        assert!(rules.masks("anything", false), "entry secret masks all");
    }

    #[test]
    fn policy_rule_names_must_be_exported_functions() {
        // @std/random@v1 exports uuid4/ulid/… but not 'nope' — a typo'd rule
        // would silently never fire, so it is a hard validation error.
        for field in ["allow", "deny", "log"] {
            let mut r = std_random_ref();
            match field {
                "allow" => r.allow = Some(vec!["nope".into()]),
                "deny" => r.deny = Some(vec!["nope".into()]),
                _ => r.log = Some(vec!["nope".into()]),
            }
            let err = validate_libraries(&[r], false, None).unwrap_err();
            assert!(
                err.contains(&format!("unknown function 'nope' in `{field}:`")),
                "{field} → {err}"
            );
        }
    }
}
