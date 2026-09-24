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
#[derive(Debug, Default, Clone)]
pub struct Context {
    pub(crate) vars: HashMap<String, Value>,
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
    /// Shared run metrics, seeded by the runner for `after:` steps so
    /// `std/thresholds@v1` can evaluate gates over everything the run
    /// collected. `None` in per-iteration and `before` contexts.
    pub(crate) run_metrics: Option<std::sync::Arc<std::sync::Mutex<crate::step::runner::Metrics>>>,
    /// Which HTTP client shard this VU uses (see the sharded clients in
    /// `step::actions`). Seeded by the runner from the VU id so every VU
    /// keeps exactly one warm connection pool; 0 in hand-built contexts.
    pub(crate) http_client_shard: usize,
    /// Run-scoped registry of resolved `${{ env.NAME }}` values, shared by
    /// every context of the run. Each successful env resolution is recorded
    /// here (see [`Context::resolve_expr`]); the log pipeline masks every
    /// recorded value out of the run log. A hand-built context gets its own
    /// empty registry, which masks nothing.
    pub(crate) secrets: SecretRegistry,
    /// Validated `libraries:` bindings (RFC 005), shared by every context of
    /// the run; each generator built by [`Context::new_generator`] mints its
    /// own instances from these. `None` when no libraries are declared.
    pub(crate) libraries: Option<Arc<crate::library::LibrarySet>>,
    /// Run-scoped per-library metrics recorder (RFC 005), shared by every
    /// generator of the run. `None` when no libraries are declared.
    pub(crate) library_metrics: Option<Arc<crate::library::LibraryMetrics>>,
    /// Run-level determinism seed (`seed:` in the config, RFC 005). When set,
    /// generators derive `hash(seed, vu_id, conn_seq)` instead of a random
    /// seed.
    pub(crate) run_seed: Option<u64>,
    /// This VU's id (1-based) and current loop iteration — carried into
    /// library call contexts. 0 in hand-built contexts.
    pub(crate) vu_id: u64,
    pub(crate) iteration_seq: u64,
    /// Counts generators minted by this context — the `conn_seq` input of the
    /// per-instance seed derivation.
    gen_counter: Arc<std::sync::atomic::AtomicU64>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a step's output under `name`.
    pub fn set(&mut self, name: &str, value: Value) {
        self.vars.insert(name.to_string(), value);
    }

    /// Which HTTP client shard this VU is pinned to (see
    /// [`crate::step::http::client`]). Downstream HTTP-based actions pass this
    /// to `client()` so they share the VU's warm connection pool; 0 in
    /// hand-built contexts.
    pub fn http_client_shard(&self) -> usize {
        self.http_client_shard
    }

    /// A fresh `${…}` generator for a new connection or one-shot action.
    /// Seeded deterministically as `hash(run_seed, vu_id, conn_seq)` when the
    /// config sets `seed:`, randomly otherwise; every declared library is
    /// attached as a fresh instance behind its alias (one instance per
    /// generator owner — RFC 005 instance lifecycle). Fails only when a
    /// library rejects its `with:` config at instantiation.
    pub fn new_generator(&self) -> Result<crate::generate::Gen, String> {
        let conn_seq = self
            .gen_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let seed = match self.run_seed {
            Some(run_seed) => crate::library::derive_seed(run_seed, self.vu_id, conn_seq),
            None => uuid::Uuid::new_v4().as_u128() as u64,
        };
        let mut gen = crate::generate::Gen::new(seed).with_vu(self.vu_id, self.iteration_seq);
        if let Some(metrics) = &self.library_metrics {
            gen = gen.with_library_metrics(Arc::clone(metrics));
        }
        if let Some(set) = &self.libraries {
            for lib in &set.libraries {
                let instance = lib
                    .provider
                    .instantiate(lib.config.clone(), seed)
                    .map_err(|e| format!("library '{}' ({}): {e}", lib.alias, lib.provider.id()))?;
                gen.attach_library(lib.alias.clone(), instance);
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
        let Some(mut current) = self.vars.get(root) else {
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
                r#as: None,
                capabilities: None,
                with: None,
            }],
            false,
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
}
