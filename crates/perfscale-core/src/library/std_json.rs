//! `@std/json@v1` — the built-in JSON-file reading library.
//!
//! Native (no wasmtime), same provider contract as `@std/random@v1`. One
//! function, `read`, for use inside `${…}` generator tokens:
//!
//! ```yaml
//! libraries: [{ use: "@std/json@v1", as: json, capabilities: [] }]
//! # ...
//! body: '{"token": ${json.read("users.json", ".users[0].token")}}'
//! ```
//!
//! - `read(file)` — the whole parsed document as a compact JSON string.
//! - `read(file, query)` — the subvalue at a jq-style path (the shared
//!   [`crate::step::json_path`] grammar: `.posts[0].id` ≡ `posts.0.id`). A
//!   string leaf is returned verbatim, other values as compact JSON.
//!
//! Documents come from the process-wide cache shared with the
//! `std/read-json@v1` step ([`crate::step::json_doc`]), revalidated by
//! `(mtime, size)` — the first call pays disk + parse, the rest hit RAM.
//!
//! Filesystem access is **fail-closed**: the run must set
//! `allow_file_actions: true` (and paths honour `fs_root`), enforced through
//! `CallCtx.allow_file_actions` / `CallCtx.fs_root` — the identical gate the
//! `std/file-*@v1` steps use.
//!
//! Error semantics differ from the step on purpose: a mandatory path miss is
//! a token-expansion **error** naming the failing segment (the step fails
//! the step), while a `?`-tolerant miss resolves to an **empty string**
//! (interpolation-friendly; the step yields a `null` output).

use serde_json::Value;

use super::{CallCtx, FunctionInfo, LibraryInstance, LibraryProvider};

const FUNCTIONS: &[FunctionInfo] = &[FunctionInfo {
    name: "read",
    description: "Read a JSON file (process-wide cached): read(file[, query])",
    secret: false,
}];

pub struct StdJsonProvider;

impl LibraryProvider for StdJsonProvider {
    fn id(&self) -> &str {
        "@std/json@v1"
    }

    fn functions(&self) -> &[FunctionInfo] {
        FUNCTIONS
    }

    fn instantiate(
        &self,
        config: Option<Value>,
        _seed: u64,
    ) -> Result<Box<dyn LibraryInstance>, String> {
        match config {
            None | Some(Value::Null) => Ok(Box::new(StdJson)),
            Some(_) => Err("@std/json@v1 takes no `with:` config".into()),
        }
    }
}

/// Stateless — every call goes through the shared process-wide cache.
struct StdJson;

impl StdJson {
    fn read(&self, ctx: &CallCtx<'_>, args: &[Value]) -> Result<String, String> {
        let file = match args.first() {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => return Err("json.read: expected read(file[, query])".into()),
        };
        if args.len() > 2 {
            return Err(format!(
                "json.read: too many arguments ({} given) — expected read(file[, query])",
                args.len()
            ));
        }
        let doc = crate::step::json_doc::load_sync(&file, ctx.allow_file_actions, ctx.fs_root)?;

        let Some(query) = args.get(1) else {
            return Ok(doc.to_string());
        };
        let owned;
        let query = match query {
            Value::String(s) => s.as_str(),
            other => {
                owned = other.to_string();
                owned.as_str()
            }
        };
        let segments = crate::step::json_path::parse(query)?;
        match crate::step::json_path::get(&doc, &segments)? {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(v) => Ok(v.to_string()),
            // A tolerant (`?`) segment missed → empty string, still a success.
            None => Ok(String::new()),
        }
    }
}

impl LibraryInstance for StdJson {
    fn call(&mut self, ctx: &CallCtx<'_>, func: &str, args: &[Value]) -> Result<String, String> {
        match func {
            "read" => self.read(ctx, args),
            other => Err(format!(
                "json.{other}: unknown function — @std/json@v1 exports: read"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> CallCtx<'static> {
        CallCtx {
            message_seq: 1,
            iteration_seq: 1,
            vu_id: 1,
            seed: 42,
            time_ms: 1_784_160_000_000,
            settings_json: "{}",
            allow_file_actions: true,
            fs_root: None,
        }
    }

    fn instance() -> Box<dyn LibraryInstance> {
        StdJsonProvider.instantiate(None, 1).unwrap()
    }

    fn fixture() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.json");
        std::fs::write(&path, r#"{"posts":[{"id":7},{"id":9}],"title":"hello"}"#).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    #[test]
    fn read_returns_the_whole_document_compact() {
        let (_dir, file) = fixture();
        let mut inst = instance();
        let v = inst.call(&ctx(), "read", &[Value::from(file)]).unwrap();
        assert_eq!(v, r#"{"posts":[{"id":7},{"id":9}],"title":"hello"}"#);
    }

    #[test]
    fn read_query_extracts_dot_and_bracket_paths() {
        let (_dir, file) = fixture();
        let mut inst = instance();
        for query in [".posts[1].id", "posts.1.id"] {
            let v = inst
                .call(
                    &ctx(),
                    "read",
                    &[Value::from(file.clone()), Value::from(query)],
                )
                .unwrap();
            assert_eq!(v, "9", "{query}");
        }
        // A string leaf comes back verbatim, objects as compact JSON.
        let v = inst
            .call(
                &ctx(),
                "read",
                &[Value::from(file.clone()), Value::from(".title")],
            )
            .unwrap();
        assert_eq!(v, "hello");
        let v = inst
            .call(
                &ctx(),
                "read",
                &[Value::from(file), Value::from(".posts[0]")],
            )
            .unwrap();
        assert_eq!(v, r#"{"id":7}"#);
    }

    #[test]
    fn tolerant_miss_is_empty_mandatory_miss_is_an_error() {
        let (_dir, file) = fixture();
        let mut inst = instance();
        let v = inst
            .call(
                &ctx(),
                "read",
                &[Value::from(file.clone()), Value::from(".users?[0]")],
            )
            .unwrap();
        assert_eq!(v, "", "tolerant miss resolves to empty");
        let err = inst
            .call(
                &ctx(),
                "read",
                &[Value::from(file), Value::from(".users[0]")],
            )
            .unwrap_err();
        assert!(err.contains(".users"), "{err}");
    }

    #[test]
    fn invalid_json_and_missing_args_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.json");
        std::fs::write(&path, "{not json").unwrap();
        let mut inst = instance();
        let err = inst
            .call(&ctx(), "read", &[Value::from(path.to_string_lossy())])
            .unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");
        let err = inst.call(&ctx(), "read", &[]).unwrap_err();
        assert!(err.contains("read(file[, query])"), "{err}");
        let err = inst
            .call(
                &ctx(),
                "read",
                &[Value::from("a"), Value::from("b"), Value::from("c")],
            )
            .unwrap_err();
        assert!(err.contains("too many arguments"), "{err}");
        let err = inst.call(&ctx(), "pick", &[]).unwrap_err();
        assert!(err.contains("unknown function"), "{err}");
    }

    #[test]
    fn read_is_fail_closed_without_allow_file_actions() {
        let (_dir, file) = fixture();
        let mut inst = instance();
        let mut gated = ctx();
        gated.allow_file_actions = false;
        let err = inst.call(&gated, "read", &[Value::from(file)]).unwrap_err();
        assert!(err.contains("file actions disabled"), "{err}");
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn cache_is_shared_with_the_read_json_step() {
        let (_dir, file) = fixture();
        let reads =
            || crate::step::json_doc::JSON_DISK_READS.load(std::sync::atomic::Ordering::Relaxed);

        // Step reads first…
        let mut step_ctx = crate::step::context::Context::new();
        step_ctx.allow_file_actions = true;
        let before = reads();
        let step = crate::step::actions::execute_action(
            "std/read-json@v1",
            &serde_json::json!({ "file": file }),
            &step_ctx,
            "step",
        )
        .await;
        assert!(step.success, "{:?}", step.logs);
        assert_eq!(reads() - before, 1, "one disk read for the step");

        // …then the library serves the same file from the shared cache.
        let mut inst = instance();
        let v = inst
            .call(
                &ctx(),
                "read",
                &[Value::from(file), Value::from(".posts[0].id")],
            )
            .unwrap();
        assert_eq!(v, "7");
        assert_eq!(reads() - before, 1, "library call must be a cache hit");
    }

    /// End-to-end alias resolution: a declared `libraries:` entry resolves
    /// `${json.read(...)}` inside a generator token, with the run's
    /// filesystem gate flowing through Context → Gen → CallCtx.
    #[test]
    fn read_resolves_through_real_interpolation() {
        let (_dir, file) = fixture();
        let mut ctx = crate::step::context::Context::new();
        ctx.allow_file_actions = true;
        ctx.libraries = Some(std::sync::Arc::new(crate::library::LibrarySet {
            libraries: crate::library::validate_libraries(
                &[crate::library::LibraryRef {
                    use_: "@std/json@v1".into(),
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
        let mut gen = ctx.new_generator().unwrap();
        gen.begin_message();
        let token = format!("${{json.read(\"{file}\", \".posts[0].id\")}}");
        assert_eq!(gen.expand(&token).unwrap(), "7");
        // The default alias is the library's name part.
        let err = gen.expand("${json.nope()}").unwrap_err();
        assert!(err.contains("unknown function"), "{err}");

        // Fail-closed end to end: same token, gate off → expansion error.
        let mut ctx = crate::step::context::Context::new();
        ctx.libraries = Some(std::sync::Arc::new(crate::library::LibrarySet {
            libraries: crate::library::validate_libraries(
                &[crate::library::LibraryRef {
                    use_: "@std/json@v1".into(),
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
        let mut gen = ctx.new_generator().unwrap();
        gen.begin_message();
        let err = gen.expand(&token).unwrap_err();
        assert!(err.contains("file actions disabled"), "{err}");
    }

    #[test]
    fn with_config_is_rejected() {
        let e = match StdJsonProvider.instantiate(Some(Value::from("x")), 1) {
            Ok(_) => panic!("config must be rejected"),
            Err(e) => e,
        };
        assert!(e.contains("takes no `with:` config"), "{e}");
    }
}
