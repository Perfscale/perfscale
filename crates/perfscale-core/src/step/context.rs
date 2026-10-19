//! Per-VU execution context — variable store and `${{ ... }}` interpolation.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::log_mask::SecretRegistry;
use crate::runner::LogLine;
use crate::step::process::ProcessRegistry;

/// Execution context for a single VU iteration.
///
/// Stores step outputs so later steps can reference them via
/// `${{ var_name.field }}` in string parameter values. Live resources (open
/// WebSockets) are not JSON and live in `resources` instead — steps refer to
/// them by the Connection ID a connect step returned.
#[derive(Debug, Clone)]
pub struct Context {
    /// Stored step outputs (`outputs:` names, `__last__`, `vars`, `config`).
    /// Values are shared, not copied: a step output is stored once behind an
    /// `Arc` and cloned by reference into `__last__` — the per-step double
    /// deep-clone of potentially large response bodies is gone.
    pub(crate) vars: HashMap<String, Arc<Value>>,
    pub(crate) resources: crate::step::resources::Resources,
    /// Master switch for filesystem-touching actions (`std/file-read@v1`,
    /// `std/file-write@v1`, multipart `file` parts). Fail-closed: a fresh
    /// context denies file access until the runner seeds it from
    /// [`crate::step::RunConfig::allow_file_actions`].
    pub(crate) allow_file_actions: bool,
    /// Master switch for process-touching actions (`std/child_process@v1`,
    /// `std/kill_process@v1`). Fail-closed: a fresh context denies process
    /// access until the runner seeds it from
    /// [`crate::step::RunConfig::allow_process_actions`].
    pub(crate) allow_process_actions: bool,
    /// Optional confinement root: file action paths must stay under it
    /// (`std/file-read@v1`, `std/file-write@v1`, multipart `file` parts).
    pub(crate) fs_root: Option<std::path::PathBuf>,
    /// Run-scoped registry of managed child processes, shared by every
    /// context of the run. `None` outside a native run (unit tests that build
    /// a context by hand) — process actions then fail with a clear error.
    pub(crate) processes: Option<Arc<ProcessRegistry>>,
    /// Live log stream of the run, for managed processes to mirror their
    /// output into (prefixed `{step}: ` lines, like the k6 runner).
    pub(crate) log_tx: Option<tokio::sync::mpsc::Sender<LogLine>>,
    /// Shared run metrics, seeded by the runner for every context of the run
    /// (per-VU iteration, `before`, `after`) so actions — built-in and pro —
    /// can record background-sampled metrics, and `std/thresholds@v1` can
    /// evaluate gates over everything the run collected. `None` in contexts
    /// built by hand (unit tests).
    pub(crate) run_metrics: Option<std::sync::Arc<crate::metrics::ShardedMetrics>>,
    /// WebRTC media-plane configuration (RFC 007) mapped from the run's
    /// `webrtc:` block, consumed by the pro `pro/webrtc-*` actions. `None`
    /// when the run does not declare it.
    pub(crate) webrtc: Option<crate::yaml::WebRtcConfig>,
    /// Which HTTP client shard this VU uses (see the sharded clients in
    /// `step::actions`). Seeded by the runner from the VU id so every VU
    /// keeps exactly one warm connection pool; 0 in hand-built contexts.
    pub(crate) http_client_shard: usize,
    /// Response body cap for HTTP-family steps (`max_response_body_bytes` in
    /// the run config). Seeded by the runner; hand-built contexts get the
    /// default.
    pub(crate) max_response_body_bytes: u64,
    /// Run-scoped registry of resolved `${{ env.NAME }}` values, shared by
    /// every context of the run. Each successful env resolution is recorded
    /// here (see [`Context::resolve_expr`]); the log pipeline masks every
    /// recorded value out of the run log. A hand-built context gets its own
    /// empty registry, which masks nothing.
    pub(crate) secrets: SecretRegistry,
    /// Validated `libraries:` bindings (RFC 005), shared by every context of
    /// the run; each generator built by [`Context::new_generator`] mints its
    /// own instances from these — or reuses this context's cached instance
    /// for providers opting into
    /// [`crate::library::LibraryProvider::share_per_context`]. `None` when no
    /// libraries are declared.
    pub(crate) libraries: Option<Arc<crate::library::LibrarySet>>,
    /// Per-context (per-VU) cache of shared library instances, one per alias
    /// of a `share_per_context` provider. Behind `Arc` so a cloned context
    /// (same VU identity) shares it; the inner mutex is uncontended — a VU
    /// runs its steps sequentially.
    pub(crate) shared_lib_instances: Arc<std::sync::Mutex<Vec<crate::generate::SharedLibEntry>>>,
    /// Run-scoped per-library metrics recorder (RFC 005), shared by every
    /// generator of the run. `None` when no libraries are declared.
    pub(crate) library_metrics: Option<Arc<crate::library::LibraryMetrics>>,
    /// Run-level determinism seed (`seed:` in the config, RFC 005). When set,
    /// generators derive `hash(seed, vu_id, conn_seq)` instead of a random
    /// seed.
    pub(crate) run_seed: Option<u64>,
    /// Run settings JSON frozen at run start, handed to every library call
    /// as `CallCtx.settings_json` (RFC 005 settings). `"{}"` in hand-built
    /// contexts.
    pub(crate) settings_json: Arc<str>,
    /// This VU's id (1-based) and current loop iteration — carried into
    /// library call contexts. 0 in hand-built contexts.
    pub(crate) vu_id: u64,
    pub(crate) iteration_seq: u64,
    /// Counts generators minted by this context — the `conn_seq` input of the
    /// per-instance seed derivation.
    gen_counter: Arc<std::sync::atomic::AtomicU64>,
    /// Random base for unseeded runs (`run_seed: None`): drawn once per
    /// context (one `Uuid::new_v4`, not one per generator — a getrandom
    /// syscall per db-query was measurable on the cold-params path) and
    /// combined with `conn_seq`, so every generator still gets its own
    /// random stream while seeded runs are untouched.
    unseeded_gen_base: Arc<std::sync::OnceLock<u64>>,
    /// Derived `<family>_failed` rate-metric names, memoized per context:
    /// the step fold would otherwise format! a fresh String per histogram
    /// metric per step.
    pub(crate) rate_names: HashMap<String, String>,
    /// The run's `--quiet` flag, seeded by the runner. Actions use it to
    /// skip *formatting* per-step success lines the runner would drop
    /// anyway (quiet keeps only `LogTag::Err`) — under a busy loop the
    /// format! itself is the measurable cost.
    pub(crate) quiet: bool,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            vars: HashMap::new(),
            resources: Default::default(),
            allow_file_actions: false,
            allow_process_actions: false,
            fs_root: None,
            processes: None,
            log_tx: None,
            run_metrics: None,
            webrtc: None,
            http_client_shard: 0,
            max_response_body_bytes: crate::step::http::DEFAULT_MAX_RESPONSE_BODY_BYTES,
            secrets: SecretRegistry::new(),
            libraries: None,
            shared_lib_instances: Default::default(),
            library_metrics: None,
            run_seed: None,
            settings_json: Arc::from("{}"),
            vu_id: 0,
            iteration_seq: 0,
            gen_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            unseeded_gen_base: Arc::new(std::sync::OnceLock::new()),
            rate_names: HashMap::new(),
            quiet: false,
        }
    }
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a step's output under `name`.
    pub fn set(&mut self, name: &str, value: Value) {
        self.set_shared(name, Arc::new(value));
    }

    /// Store an already-shared value — the runner's step fold uses this so
    /// `outputs:` and `__last__` share one allocation instead of two
    /// deep clones.
    pub(crate) fn set_shared(&mut self, name: &str, value: Arc<Value>) {
        // get_mut first: the per-step names (`__last__`, every `outputs:`)
        // exist after the first iteration, so the key String must not
        // re-allocate per step.
        match self.vars.get_mut(name) {
            Some(slot) => *slot = value,
            None => {
                self.vars.insert(name.to_string(), value);
            }
        }
    }

    /// Which HTTP client shard this VU is pinned to (see
    /// [`crate::step::http::client`]). Downstream HTTP-based actions pass this
    /// to `client()` so they share the VU's warm connection pool; 0 in
    /// hand-built contexts.
    pub fn http_client_shard(&self) -> usize {
        self.http_client_shard
    }

    /// This VU's id (1-based). Downstream pro modules use it for per-VU
    /// artifact naming (e.g. `pro/webrtc-*` `sink: record` file names); 0 in
    /// hand-built contexts.
    pub fn vu_id(&self) -> u64 {
        self.vu_id
    }

    /// Seed the VU id (the runner does this for every context of a real run).
    /// Public but hidden: a test seam for downstream crates whose actions key
    /// behavior off the VU id (e.g. `pro/webrtc-call@v1` pairing math) without
    /// a full run.
    #[doc(hidden)]
    pub fn set_vu_id(&mut self, vu_id: u64) {
        self.vu_id = vu_id;
    }

    /// Extension connection registries for downstream pro action families
    /// (`pro/webrtc-*`, future `pro/*`). This is THE way a pro module parks
    /// live connections for the duration of a VU iteration:
    /// `ctx.extensions().registry::<MyConn>("rtc")` returns the family's
    /// [`perfscale_connection::ConnectionRegistry`] (same
    /// insert/take/put_back semantics as the built-in ws/grpc/db families),
    /// and the runner's iteration-end drain drops whatever is left parked.
    pub fn extensions(&self) -> &perfscale_connection::ExtensionRegistries {
        self.resources.extras()
    }

    /// Shared run-metrics accumulator of the run this context belongs to
    /// (`None` in hand-built contexts). Pro action families use it for
    /// background-sampled metrics that outlive a single step (e.g. periodic
    /// WebRTC getStats sampling) — the same sharded store the runner folds
    /// step-emitted `value["metrics"]` counters/histograms into.
    pub fn run_metrics(&self) -> Option<&std::sync::Arc<crate::metrics::ShardedMetrics>> {
        self.run_metrics.as_ref()
    }

    /// The run's `webrtc:` config block (RFC 007), mapped into the run config
    /// by the embedding process; consumed by the pro `pro/webrtc-*` actions
    /// (ICE servers, `max_peer_connections`). `None` when the run does not
    /// declare it.
    pub fn webrtc_config(&self) -> Option<&crate::yaml::WebRtcConfig> {
        self.webrtc.as_ref()
    }

    /// Seed the run-metrics handle (the runner does this for every context of
    /// a real run). Public but hidden: a test seam for downstream crates that
    /// need to observe background-sampled metrics without a full run.
    #[doc(hidden)]
    pub fn set_run_metrics(&mut self, metrics: std::sync::Arc<crate::metrics::ShardedMetrics>) {
        self.run_metrics = Some(metrics);
    }

    /// Seed the `webrtc:` config block (the embedding process does this via
    /// `RunConfig.webrtc`). Public but hidden: a test seam for downstream
    /// crates.
    #[doc(hidden)]
    pub fn set_webrtc_config(&mut self, config: crate::yaml::WebRtcConfig) {
        self.webrtc = Some(config);
    }

    /// Seed validated `libraries:` bindings (the runner does this for every
    /// context of a real run). Public but hidden: a test seam for downstream
    /// crates that exercise library-backed steps (e.g. `pro/webrtc-*`
    /// `signal: library`) without a full run.
    #[doc(hidden)]
    pub fn set_libraries(&mut self, libraries: Arc<crate::library::LibrarySet>) {
        self.libraries = Some(libraries);
    }

    /// Invoke a declared library function directly — the RFC 007
    /// `signal: library` seam for pro actions that must hand a computed value
    /// (an SDP offer) to a library mid-step and consume its return value.
    /// `token` is one `${alias.fn(args)}` token; `${…}` tokens inside the
    /// argument text expand first, `extra_args` are prepended to the parsed
    /// arguments. The call runs on a fresh generator (one message context)
    /// with this context's libraries, seed, VU identity, settings, metrics
    /// and secrets — call policy (`allow:`/`deny:`) and result masking apply
    /// exactly as for payload tokens. Errors name the token.
    pub fn call_library(
        &self,
        token: &str,
        extra_args: &[serde_json::Value],
    ) -> Result<String, String> {
        let mut gen = self.new_generator()?;
        gen.begin_message();
        gen.call_library(token, extra_args)
    }

    /// A fresh `${…}` generator for a new connection or one-shot action.
    /// Seeded deterministically as `hash(run_seed, vu_id, conn_seq)` when the
    /// config sets `seed:`, randomly otherwise. Libraries whose providers opt
    /// into [`crate::library::LibraryProvider::share_per_context`] (WASM —
    /// instantiation is expensive) attach this context's cached instance,
    /// created on first use and living until the context (VU) ends; every
    /// other library mints a fresh instance per generator (the historical
    /// lifecycle, RFC 005). Fails only when a library rejects its `with:`
    /// config at instantiation.
    pub fn new_generator(&self) -> Result<crate::generate::Gen, String> {
        let conn_seq = self
            .gen_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let seed = match self.run_seed {
            Some(run_seed) => crate::library::derive_seed(run_seed, self.vu_id, conn_seq),
            // One random base per context; conn_seq keeps every generator's
            // stream distinct. (Previously a fresh `Uuid::new_v4` per
            // generator — a getrandom syscall per query on token-heavy db
            // scenarios.)
            None => (*self
                .unseeded_gen_base
                .get_or_init(|| uuid::Uuid::new_v4().as_u128() as u64))
            .wrapping_add(conn_seq),
        };
        let mut gen = crate::generate::Gen::new(seed)
            .with_vu(self.vu_id, self.iteration_seq)
            .with_settings(Arc::clone(&self.settings_json))
            .with_secrets(self.secrets.clone());
        if let Some(metrics) = &self.library_metrics {
            gen = gen.with_library_metrics(Arc::clone(metrics));
        }
        // Shared instances persist across generators: the memo base keeps
        // `CallCtx.message_seq` unique per message so per-message
        // memoization still never leaks.
        gen.set_memo_base(conn_seq << 32);
        if let Some(set) = &self.libraries {
            for lib in &set.libraries {
                if lib.provider.share_per_context() {
                    let mut cache = self.shared_lib_instances.lock().unwrap();
                    let entry = match cache.iter().find(|e| e.alias == lib.alias) {
                        Some(e) => e,
                        None => {
                            let instance = lib
                                .provider
                                .instantiate(lib.config.clone(), seed)
                                .map_err(|e| {
                                    format!("library '{}' ({}): {e}", lib.alias, lib.provider.id())
                                })?;
                            cache.push(crate::generate::SharedLibEntry {
                                alias: lib.alias.clone(),
                                rules: lib.rules.clone(),
                                functions: lib.provider.functions().to_vec(),
                                instance: Arc::new(std::sync::Mutex::new(instance)),
                            });
                            cache.last().expect("just pushed")
                        }
                    };
                    gen.attach_shared_library(
                        entry.alias.clone(),
                        Arc::clone(&entry.instance),
                        entry.rules.clone(),
                        entry.functions.clone(),
                    );
                } else {
                    let instance =
                        lib.provider
                            .instantiate(lib.config.clone(), seed)
                            .map_err(|e| {
                                format!("library '{}' ({}): {e}", lib.alias, lib.provider.id())
                            })?;
                    gen.attach_library(
                        lib.alias.clone(),
                        instance,
                        lib.rules.clone(),
                        lib.provider.functions().to_vec(),
                    );
                }
            }
        }
        Ok(gen)
    }

    /// A lazily-built `${…}` expander for one-shot actions (http, tcp/udp,
    /// llm, pub/sub, file-write): every expansion done through it shares one
    /// message context — one `begin_message`, so `${seq}` reads 1 and library
    /// `memo` keys see one message. The generator (and its library instances)
    /// is only constructed on the first token-bearing string, so token-free
    /// executions pay nothing. See [`TokenExpander`].
    pub(crate) fn token_expander(&self) -> TokenExpander<'_> {
        TokenExpander {
            ctx: self,
            gen: None,
        }
    }

    /// Interpolate `${{ expr }}` placeholders in a string.
    ///
    /// Supported forms:
    /// - `${{ name }}`                → whole stored value as string
    /// - `${{ name.field }}`          → field of a stored JSON object
    /// - `${{ name.a.b }}`            → nested path, one JSON level per `.`
    ///   (e.g. `${{ resp.headers.x-request-id }}`)
    /// - `${{ env.NAME }}`            → process environment variable `NAME`;
    ///   a missing variable resolves to an empty string here — callers that
    ///   can surface errors should use [`Context::try_interpolate`], which
    ///   fails instead (a silently empty `api_key` is a confusing 401).
    pub fn interpolate(&self, s: &str) -> String {
        self.interpolate_inner(s, false)
            .unwrap_or_else(|_| unreachable!("non-strict interpolation never fails"))
    }

    /// Fallible variant of [`Context::interpolate`]: a `${{ env.NAME }}`
    /// placeholder whose variable is not set is an error
    /// (`env var 'NAME' is not set`) instead of an empty string. Stored-var
    /// misses stay empty-string either way, for backward compatibility.
    pub fn try_interpolate(&self, s: &str) -> Result<String, String> {
        self.interpolate_inner(s, true)
    }

    fn interpolate_inner(&self, s: &str, strict: bool) -> Result<String, String> {
        // Single pass: literals stream into one output buffer, so each
        // placeholder costs a lookup and a push instead of a `replace_range`
        // (memmove + possible realloc) per placeholder; a placeholder-free
        // string costs one scan and no extra copies. Text produced by a
        // substitution is never re-scanned, as before.
        let mut rest = s;
        let Some(mut start) = rest.find("${{") else {
            return Ok(s.to_string());
        };
        let mut out = String::with_capacity(s.len() + 16);
        loop {
            out.push_str(&rest[..start]);
            let after = &rest[start + 3..];
            let Some(end) = after.find("}}") else {
                // Unterminated opener: the remainder stays verbatim.
                out.push_str(&rest[start..]);
                return Ok(out);
            };
            match self.resolve_expr(after[..end].trim()) {
                Ok(v) => out.push_str(&v),
                Err(e) if strict => return Err(e),
                Err(_) => {} // non-strict: a miss resolves to empty, as before
            }
            rest = &after[end + 2..];
            match rest.find("${{") {
                Some(next) => start = next,
                None => {
                    out.push_str(rest);
                    return Ok(out);
                }
            }
        }
    }

    /// Resolve an expression like `"resp"`, `"resp.status"`, or a nested
    /// path like `"resp.headers.x-request-id"` — each `.` descends one JSON
    /// object level. Missing variables or fields resolve to an empty string.
    ///
    /// The `env.` prefix is special: `${{ env.NAME }}` reads the process
    /// environment variable `NAME` (everything after `env.` is the variable
    /// name), and a missing variable is an `Err`, never a silent empty — the
    /// non-strict wrappers downgrade that error to an empty string. A
    /// successfully resolved env value is recorded in the context's
    /// run-scoped [`SecretRegistry`] (`secrets`), and the native runner's
    /// log pipeline masks every recorded value out of the run log — the
    /// "env values never appear in logs" contract is enforced there, not by
    /// withholding the value from steps that legitimately need it.
    fn resolve_expr(&self, expr: &str) -> Result<String, String> {
        if let Some(name) = expr.strip_prefix("env.") {
            if name.is_empty() {
                return Err("env placeholder needs a variable name: ${{ env.NAME }}".into());
            }
            return std::env::var(name)
                .inspect(|value| self.secrets.record(value))
                .map_err(|_| format!("env var '{name}' is not set"));
        }
        let mut segments = expr.split('.');
        let root = segments.next().unwrap_or("");
        let Some(mut current) = self.vars.get(root).map(|v| &**v) else {
            return Ok(String::new());
        };
        for segment in segments {
            match current.get(segment) {
                Some(v) => current = v,
                None => return Ok(String::new()),
            }
        }
        Ok(value_to_string(current))
    }

    /// Apply interpolation to every string leaf of a JSON `Value`.
    ///
    /// Non-strict: a missing `${{ env.NAME }}` resolves to an empty string.
    /// Prefer [`Context::try_interpolate_value`] where a failure can be
    /// surfaced (step execution).
    pub fn interpolate_value(&self, v: &Value) -> Value {
        self.interpolate_value_inner(v, false)
            .unwrap_or_else(|_| unreachable!("non-strict interpolation never fails"))
    }

    /// Fallible variant of [`Context::interpolate_value`]: the first missing
    /// `${{ env.NAME }}` anywhere in the tree fails the whole value.
    pub fn try_interpolate_value(&self, v: &Value) -> Result<Value, String> {
        self.interpolate_value_inner(v, true)
    }

    fn interpolate_value_inner(&self, v: &Value, strict: bool) -> Result<Value, String> {
        Ok(match v {
            Value::String(s) => Value::String(self.interpolate_inner(s, strict)?),
            Value::Object(m) => {
                let mut out = serde_json::Map::new();
                for (k, val) in m {
                    out.insert(k.clone(), self.interpolate_value_inner(val, strict)?);
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(
                a.iter()
                    .map(|x| self.interpolate_value_inner(x, strict))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            other => other.clone(),
        })
    }
}

/// One message's worth of `${…}` expansion for a one-shot action, built by
/// [`Context::token_expander`]. Strings expand through one lazily-built
/// [`crate::generate::Gen`], so every field of the request shares one
/// `begin_message` (one `${seq}` value, one library message context) while a
/// token-free execution never constructs a generator — or its library
/// instances — at all. `Err` from any expand call is a step failure (known
/// alias + unknown function / bad args / library error — see
/// [`crate::generate::Gen::expand`]); unknown tokens stay verbatim.
pub(crate) struct TokenExpander<'a> {
    ctx: &'a Context,
    gen: Option<crate::generate::Gen>,
}

impl TokenExpander<'_> {
    /// Expand one string. Token-free strings return unchanged without
    /// building the generator.
    pub(crate) fn expand(&mut self, s: &str) -> Result<String, String> {
        if !s.contains("${") {
            return Ok(s.to_string());
        }
        self.gen()?.expand(s)
    }

    /// Expand every string leaf of a JSON value (keys untouched — see
    /// [`crate::generate::expand_tokens`]).
    pub(crate) fn expand_value(&mut self, v: &Value) -> Result<Value, String> {
        if !crate::generate::contains_token(v) {
            return Ok(v.clone());
        }
        crate::generate::expand_tokens(v, self.gen()?)
    }

    fn gen(&mut self) -> Result<&mut crate::generate::Gen, String> {
        if self.gen.is_none() {
            let mut gen = self.ctx.new_generator()?;
            // One execution is one message: every expansion through this
            // expander shares one `${seq}` value and one library message
            // context (memo keys repeat within the request).
            gen.begin_message();
            self.gen = Some(gen);
        }
        Ok(self.gen.as_mut().expect("generator set above"))
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interpolate_field() {
        let mut ctx = Context::new();
        ctx.set("resp", json!({ "status": 200, "body": "ok" }));
        assert_eq!(
            ctx.interpolate("status is ${{ resp.status }}"),
            "status is 200"
        );
        assert_eq!(ctx.interpolate("${{ resp.body }}"), "ok");
    }

    #[test]
    fn interpolate_missing_is_empty() {
        let ctx = Context::new();
        assert_eq!(ctx.interpolate("${{ missing.field }}"), "");
    }

    #[test]
    fn interpolate_nested_path_descends_json_levels() {
        let mut ctx = Context::new();
        ctx.set(
            "resp",
            json!({ "headers": { "x-request-id": "abc-123", "content-type": "application/json" } }),
        );
        assert_eq!(
            ctx.interpolate("id=${{ resp.headers.x-request-id }}"),
            "id=abc-123"
        );
        // Whole nested object stringifies like any other value.
        assert_eq!(
            ctx.interpolate("${{ resp.headers }}"),
            r#"{"content-type":"application/json","x-request-id":"abc-123"}"#
        );
        // A miss at any depth resolves to empty, same as one-level misses.
        assert_eq!(ctx.interpolate("${{ resp.headers.nope }}"), "");
        assert_eq!(ctx.interpolate("${{ resp.nope.deeper }}"), "");
    }

    #[test]
    fn interpolate_multiple() {
        let mut ctx = Context::new();
        ctx.set("a", json!({ "x": "hello" }));
        ctx.set("b", json!({ "y": "world" }));
        assert_eq!(ctx.interpolate("${{ a.x }} ${{ b.y }}"), "hello world");
    }

    #[test]
    fn interpolate_whole_value_without_field() {
        let mut ctx = Context::new();
        ctx.set("name", json!("world"));
        assert_eq!(ctx.interpolate("hello ${{ name }}"), "hello world");
    }

    #[test]
    fn interpolate_number_and_bool_and_null_leaves() {
        let mut ctx = Context::new();
        ctx.set("n", json!(42));
        ctx.set("b", json!(true));
        ctx.set("z", json!(null));
        assert_eq!(ctx.interpolate("${{ n }}"), "42");
        assert_eq!(ctx.interpolate("${{ b }}"), "true");
        assert_eq!(ctx.interpolate("${{ z }}"), "null");
    }

    #[test]
    fn interpolate_no_placeholder_is_unchanged() {
        let ctx = Context::new();
        assert_eq!(ctx.interpolate("plain text"), "plain text");
    }

    #[test]
    fn interpolate_unterminated_placeholder_is_left_as_is() {
        let ctx = Context::new();
        assert_eq!(ctx.interpolate("broken ${{ oops"), "broken ${{ oops");
    }

    #[test]
    fn interpolate_value_recurses_into_objects_and_arrays() {
        let mut ctx = Context::new();
        ctx.set("x", json!("val"));
        let input = json!({
            "a": "${{ x }}",
            "list": ["${{ x }}", "plain", 3],
        });
        let out = ctx.interpolate_value(&input);
        assert_eq!(out["a"], "val");
        assert_eq!(out["list"][0], "val");
        assert_eq!(out["list"][1], "plain");
        assert_eq!(out["list"][2], 3);
    }

    #[test]
    fn interpolate_value_leaves_non_string_leaves_untouched() {
        let ctx = Context::new();
        let input = json!({ "n": 1, "b": true, "z": null });
        assert_eq!(ctx.interpolate_value(&input), input);
    }

    #[test]
    fn set_overwrites_previous_value() {
        let mut ctx = Context::new();
        ctx.set("v", json!("first"));
        ctx.set("v", json!("second"));
        assert_eq!(ctx.interpolate("${{ v }}"), "second");
    }

    // -----------------------------------------------------------------
    // ${{ env.NAME }}
    // -----------------------------------------------------------------
    //
    // Unique variable names per test: env is process-global and tests run in
    // parallel threads, so shared names would race.

    #[test]
    fn env_placeholder_resolves_from_process_env() {
        std::env::set_var("PERFSCALE_TEST_CTX_ENV_RESOLVE", "s3cret");
        let ctx = Context::new();
        assert_eq!(
            ctx.try_interpolate("key=${{ env.PERFSCALE_TEST_CTX_ENV_RESOLVE }}")
                .unwrap(),
            "key=s3cret"
        );
        std::env::remove_var("PERFSCALE_TEST_CTX_ENV_RESOLVE");
    }

    #[test]
    fn env_resolution_records_the_value_for_log_masking() {
        std::env::set_var("PERFSCALE_TEST_CTX_ENV_RECORD", "rec0rded-secret");
        let ctx = Context::new();
        ctx.interpolate("key=${{ env.PERFSCALE_TEST_CTX_ENV_RECORD }}");
        // The value the step received is now masked out of any log line.
        assert_eq!(
            ctx.secrets.mask("saw rec0rded-secret in a header"),
            "saw *** in a header"
        );
        // A failed resolution records nothing.
        let err = ctx.try_interpolate("${{ env.PERFSCALE_TEST_CTX_ENV_RECORD_UNSET }}");
        assert!(err.is_err());
        assert!(matches!(
            ctx.secrets.mask("nothing recorded"),
            std::borrow::Cow::Borrowed("nothing recorded")
        ));
        std::env::remove_var("PERFSCALE_TEST_CTX_ENV_RECORD");
    }

    #[test]
    fn env_missing_is_an_error_in_strict_mode() {
        std::env::remove_var("PERFSCALE_TEST_CTX_ENV_MISSING");
        let ctx = Context::new();
        let err = ctx
            .try_interpolate("${{ env.PERFSCALE_TEST_CTX_ENV_MISSING }}")
            .unwrap_err();
        assert_eq!(err, "env var 'PERFSCALE_TEST_CTX_ENV_MISSING' is not set");
        // The error names the variable but can never contain its value.
    }

    #[test]
    fn env_missing_is_empty_in_non_strict_mode() {
        std::env::remove_var("PERFSCALE_TEST_CTX_ENV_LENIENT");
        let ctx = Context::new();
        assert_eq!(
            ctx.interpolate("x${{ env.PERFSCALE_TEST_CTX_ENV_LENIENT }}y"),
            "xy"
        );
    }

    #[test]
    fn env_placeholder_without_name_is_an_error() {
        let ctx = Context::new();
        let err = ctx.try_interpolate("${{ env. }}").unwrap_err();
        assert!(err.contains("env.NAME"), "{err}");
    }

    #[test]
    fn env_resolves_inside_nested_structures() {
        std::env::set_var("PERFSCALE_TEST_CTX_ENV_NESTED", "nested-value");
        let ctx = Context::new();
        let input = json!({
            "headers": { "authorization": "Bearer ${{ env.PERFSCALE_TEST_CTX_ENV_NESTED }}" },
            "params": ["${{ env.PERFSCALE_TEST_CTX_ENV_NESTED }}", 3],
        });
        let out = ctx.try_interpolate_value(&input).unwrap();
        assert_eq!(out["headers"]["authorization"], "Bearer nested-value");
        assert_eq!(out["params"][0], "nested-value");
        assert_eq!(out["params"][1], 3);
        std::env::remove_var("PERFSCALE_TEST_CTX_ENV_NESTED");
    }

    #[test]
    fn env_missing_anywhere_fails_the_whole_value() {
        std::env::set_var("PERFSCALE_TEST_CTX_ENV_PRESENT", "ok");
        std::env::remove_var("PERFSCALE_TEST_CTX_ENV_ABSENT");
        let ctx = Context::new();
        let input = json!({
            "a": "${{ env.PERFSCALE_TEST_CTX_ENV_PRESENT }}",
            "b": { "c": "${{ env.PERFSCALE_TEST_CTX_ENV_ABSENT }}" },
        });
        let err = ctx.try_interpolate_value(&input).unwrap_err();
        assert!(err.contains("PERFSCALE_TEST_CTX_ENV_ABSENT"), "{err}");
        std::env::remove_var("PERFSCALE_TEST_CTX_ENV_PRESENT");
    }

    #[test]
    fn stored_vars_still_miss_to_empty_in_strict_mode() {
        let ctx = Context::new();
        assert_eq!(ctx.try_interpolate("${{ missing.field }}").unwrap(), "");
    }

    // -----------------------------------------------------------------
    // new_generator (RFC 005 wiring)
    // -----------------------------------------------------------------

    #[test]
    fn new_generator_attaches_declared_libraries() {
        let mut ctx = Context::new();
        let libraries = crate::library::validate_libraries(
            &[crate::library::LibraryRef {
                use_: "@std/random@v1".into(),
                sha256: None,
                r#as: None,
                capabilities: Some(vec![]),
                with: None,
                secret: None,
                allow: None,
                deny: None,
                log: None,
            }],
            false,
            None,
        )
        .unwrap();
        ctx.libraries = Some(Arc::new(crate::library::LibrarySet { libraries }));
        let mut gen = ctx.new_generator().unwrap();
        gen.begin_message();
        let v = gen.expand("${random.nanoid(8)}").unwrap();
        assert_eq!(v.len(), 8, "{v}");
    }

    #[test]
    fn new_generator_is_deterministic_with_a_run_seed() {
        let make = || {
            let mut ctx = Context::new();
            ctx.run_seed = Some(42);
            ctx.vu_id = 3;
            ctx.new_generator().unwrap()
        };
        let mut a = make();
        let mut b = make();
        assert_eq!(
            a.expand("${rand(1,1000000)}").unwrap(),
            b.expand("${rand(1,1000000)}").unwrap(),
            "same run seed + vu + conn_seq → same generator stream"
        );
        // The next generator from the same context draws a different
        // conn_seq and therefore a different stream.
        let mut ctx = Context::new();
        ctx.run_seed = Some(42);
        ctx.vu_id = 3;
        let mut first = ctx.new_generator().unwrap();
        let mut second = ctx.new_generator().unwrap();
        assert_ne!(
            first.expand("${rand(1,1000000)}").unwrap(),
            second.expand("${rand(1,1000000)}").unwrap(),
        );
    }

    #[test]
    fn call_library_invokes_a_declared_library_with_prepended_args() {
        let mut ctx = Context::new();
        ctx.vu_id = 3;
        ctx.libraries = Some(Arc::new(crate::library::LibrarySet {
            libraries: crate::library::validate_libraries(
                &[crate::library::LibraryRef {
                    use_: "@std/random@v1".into(),
                    sha256: None,
                    r#as: None,
                    capabilities: Some(vec![]),
                    with: None,
                    secret: None,
                    allow: None,
                    deny: None,
                    log: None,
                }],
                false,
                None,
            )
            .unwrap(),
        }));
        // Extra args come first, token args follow the RFC 005 mapping.
        let n: i64 = ctx
            .call_library("${random.int(10,20)}", &[])
            .unwrap()
            .parse()
            .unwrap();
        assert!((10..=20).contains(&n), "{n}");
        // Unknown alias is a hard error here (not verbatim as in payloads).
        let err = ctx.call_library("${faker.email()}", &[]).unwrap_err();
        assert!(err.contains("unknown library alias 'faker'"), "{err}");
        // No libraries at all: clear error.
        let bare = Context::new();
        let err = bare.call_library("${random.int(1,2)}", &[]).unwrap_err();
        assert!(err.contains("no libraries are declared"), "{err}");
    }

    // -----------------------------------------------------------------
    // Shared (per-context) library instances
    // -----------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// Provider that counts instantiations; its instances record every
    /// `message_seq` they see, so tests can observe instance reuse and
    /// memo-scope uniqueness.
    struct CountingProvider {
        share: bool,
        instantiated: Arc<AtomicUsize>,
        seen_seqs: Arc<std::sync::Mutex<Vec<u64>>>,
    }

    impl crate::library::LibraryProvider for CountingProvider {
        fn id(&self) -> &str {
            "test/counting@v1"
        }
        fn functions(&self) -> &[crate::library::FunctionInfo] {
            &[crate::library::FunctionInfo {
                name: "ping",
                description: "test",
                secret: false,
            }]
        }
        fn instantiate(
            &self,
            _config: Option<Value>,
            _seed: u64,
        ) -> Result<Box<dyn crate::library::LibraryInstance>, String> {
            self.instantiated.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(Box::new(CountingInstance {
                seen: Arc::clone(&self.seen_seqs),
            }))
        }
        fn share_per_context(&self) -> bool {
            self.share
        }
    }

    struct CountingInstance {
        seen: Arc<std::sync::Mutex<Vec<u64>>>,
    }

    impl crate::library::LibraryInstance for CountingInstance {
        fn call(
            &mut self,
            ctx: &crate::library::CallCtx<'_>,
            _func: &str,
            _args: &[Value],
        ) -> Result<String, String> {
            self.seen.lock().unwrap().push(ctx.message_seq);
            Ok("pong".into())
        }
    }

    fn ctx_with_counting_library(
        share: bool,
    ) -> (Context, Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<u64>>>) {
        let instantiated = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut ctx = Context::new();
        ctx.libraries = Some(Arc::new(crate::library::LibrarySet {
            libraries: vec![crate::library::ResolvedLibrary {
                alias: "c".into(),
                provider: Arc::new(CountingProvider {
                    share,
                    instantiated: Arc::clone(&instantiated),
                    seen_seqs: Arc::clone(&seen),
                }),
                config: None,
                rules: Default::default(),
            }],
        }));
        (ctx, instantiated, seen)
    }

    /// A share_per_context provider is instantiated once per context no
    /// matter how many generators (messages) the context mints — and every
    /// message still gets a distinct `message_seq` (memo isolation), with
    /// both tokens of one message sharing it.
    #[test]
    fn shared_provider_instantiates_once_per_context() {
        let (ctx, instantiated, seen) = ctx_with_counting_library(true);
        for _ in 0..3 {
            let mut g = ctx.new_generator().unwrap();
            g.begin_message();
            // Two tokens in ONE message → one message_seq; next generator →
            // a different one.
            g.expand("${c.ping()};${c.ping()}").unwrap();
        }
        assert_eq!(
            instantiated.load(AtomicOrdering::Relaxed),
            1,
            "shared instance minted once per context"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 6);
        assert_eq!(seen[0], seen[1], "one message, one message_seq");
        assert_ne!(seen[0], seen[2], "next message, new message_seq");
        assert_ne!(seen[2], seen[4], "third message, new message_seq");
    }

    /// Default lifecycle unchanged: a non-sharing provider mints a fresh
    /// instance per generator.
    #[test]
    fn non_shared_provider_instantiates_per_generator() {
        let (ctx, instantiated, _seen) = ctx_with_counting_library(false);
        for _ in 0..3 {
            let mut g = ctx.new_generator().unwrap();
            g.begin_message();
            g.expand("${c.ping()}").unwrap();
        }
        assert_eq!(instantiated.load(AtomicOrdering::Relaxed), 3);
    }

    /// Seeded derivation is untouched by sharing: two contexts with the same
    /// run seed produce the same per-generator seeds, which is what makes
    /// seeded runs reproducible.
    #[test]
    fn shared_instances_keep_seeded_derivation() {
        let expand = || {
            let (ctx, ..) = ctx_with_counting_library(true);
            // `run_seed` + `vu_id` are what base_context seeds per VU.
            let mut ctx = ctx;
            ctx.run_seed = Some(42);
            ctx.vu_id = 3;
            (0..3)
                .map(|_| {
                    let mut g = ctx.new_generator().unwrap();
                    g.begin_message();
                    g.expand("${rand(1000,9999)}").unwrap()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(expand(), expand(), "same seed → same values");
    }
}
