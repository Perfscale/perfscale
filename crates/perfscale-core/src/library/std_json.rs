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
//! - `pick(file)` / `pick(file, index)` — an element of the JSON array in
//!   `file`: uniformly random per message, or deterministic with `index`
//!   (wraps modulo the array length). A string element is returned verbatim,
//!   other values as compact JSON.
//!
//! The random pick derives its index from the same deterministic pipeline as
//! the rest of the engine ([`super::derive_seed`]): FNV-1a over
//! `(CallCtx.seed, CallCtx.message_seq, file)` modulo the array length.
//! `CallCtx.seed` already folds in the run `seed:`, the VU id and the
//! generator sequence — or the context's random base in unseeded runs — so a
//! seeded run replays the exact pick sequence and an unseeded run is random.
//! Repeat calls within one message (`message_seq` unchanged) pick the same
//! element.
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

const FUNCTIONS: &[FunctionInfo] = &[
    FunctionInfo {
        name: "read",
        description: "Read a JSON file (process-wide cached): read(file[, query])",
        secret: false,
    },
    FunctionInfo {
        name: "pick",
        description: "Element of a JSON array file: pick(file[, index]) — random per message, or index modulo the length",
        secret: false,
    },
];

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

    fn pick(&self, ctx: &CallCtx<'_>, args: &[Value]) -> Result<String, String> {
        let file = match args.first() {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => return Err("json.pick: expected pick(file[, index])".into()),
        };
        if args.len() > 2 {
            return Err(format!(
                "json.pick: too many arguments ({} given) — expected pick(file[, index])",
                args.len()
            ));
        }
        let doc = crate::step::json_doc::load_sync(&file, ctx.allow_file_actions, ctx.fs_root)?;
        let Value::Array(items) = &*doc else {
            return Err(format!("json.pick: file '{file}' is not a JSON array"));
        };
        if items.is_empty() {
            return Err(format!("json.pick: file '{file}' is an empty array"));
        }
        let index = match args.get(1) {
            None => random_index(ctx, &file, items.len()),
            Some(v) => {
                let raw = match v {
                    Value::Number(n) => n.as_u64().ok_or_else(|| {
                        format!("json.pick: index must be a non-negative integer, got {n}")
                    })?,
                    Value::String(s) => s.trim().parse::<u64>().map_err(|_| {
                        format!("json.pick: index must be a non-negative integer, got {s:?}")
                    })?,
                    other => {
                        return Err(format!(
                            "json.pick: index must be a non-negative integer, got {other}"
                        ))
                    }
                };
                (raw % items.len() as u64) as usize
            }
        };
        match &items[index] {
            Value::String(s) => Ok(s.clone()),
            v => Ok(v.to_string()),
        }
    }
}

/// Deterministic per-message index (RFC 005 determinism): FNV-1a over
/// `(seed, message_seq, file)` — the same hash idiom as
/// [`super::derive_seed`]. `seed` already carries the run seed / VU /
/// generator sequence (seeded runs) or the context's random base (unseeded
/// runs), so no extra entropy source is needed.
fn random_index(ctx: &CallCtx<'_>, file: &str, len: usize) -> usize {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for word in [ctx.seed, ctx.message_seq] {
        for byte in word.to_le_bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    for byte in file.as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h % len as u64) as usize
}

impl LibraryInstance for StdJson {
    fn call(&mut self, ctx: &CallCtx<'_>, func: &str, args: &[Value]) -> Result<String, String> {
        match func {
            "read" => self.read(ctx, args),
            "pick" => self.pick(ctx, args),
            other => Err(format!(
                "json.{other}: unknown function — @std/json@v1 exports: {}",
                FUNCTIONS
                    .iter()
                    .map(|f| f.name)
                    .collect::<Vec<_>>()
                    .join(", ")
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
    #[serial_test::file_serial(file_actions)]
    fn read_returns_the_whole_document_compact() {
        let (_dir, file) = fixture();
        let mut inst = instance();
        let v = inst.call(&ctx(), "read", &[Value::from(file)]).unwrap();
        assert_eq!(v, r#"{"posts":[{"id":7},{"id":9}],"title":"hello"}"#);
    }

    #[test]
    #[serial_test::file_serial(file_actions)]
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
    #[serial_test::file_serial(file_actions)]
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
    #[serial_test::file_serial(file_actions)]
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
        let err = inst.call(&ctx(), "nope", &[]).unwrap_err();
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
    #[serial_test::file_serial(file_actions)]
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

    // --- pick ---------------------------------------------------------------

    fn array_fixture() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        std::fs::write(&path, r#"[{"token":"a"},{"token":"b"}]"#).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    #[test]
    #[serial_test::file_serial(file_actions)]
    fn pick_by_index_wraps_modulo_the_length() {
        let (_dir, file) = array_fixture();
        let mut inst = instance();
        // String index (e.g. from another token) wraps: 5 % 2 == 1.
        let v = inst
            .call(
                &ctx(),
                "pick",
                &[Value::from(file.clone()), Value::from("5")],
            )
            .unwrap();
        assert_eq!(v, r#"{"token":"b"}"#);
        let v = inst
            .call(&ctx(), "pick", &[Value::from(file), Value::from(0)])
            .unwrap();
        assert_eq!(v, r#"{"token":"a"}"#);
        // String elements come back verbatim.
        let dir = tempfile::tempdir().unwrap();
        let strings = dir.path().join("s.json");
        std::fs::write(&strings, r#"["x","y"]"#).unwrap();
        let v = inst
            .call(
                &ctx(),
                "pick",
                &[Value::from(strings.to_string_lossy()), Value::from(3)],
            )
            .unwrap();
        assert_eq!(v, "y");
    }

    #[test]
    #[serial_test::file_serial(file_actions)]
    fn pick_rejects_bad_sources_and_indexes() {
        let mut inst = instance();
        // Non-array document.
        let (_d, file) = fixture();
        let err = inst.call(&ctx(), "pick", &[Value::from(file)]).unwrap_err();
        assert!(err.contains("not a JSON array"), "{err}");
        // Empty array.
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, "[]").unwrap();
        let err = inst
            .call(&ctx(), "pick", &[Value::from(empty.to_string_lossy())])
            .unwrap_err();
        assert!(err.contains("empty array"), "{err}");
        // Non-integer index.
        let (_d2, file) = array_fixture();
        for bad in [Value::from("x"), Value::from(-1), Value::from(1.5)] {
            let err = inst
                .call(&ctx(), "pick", &[Value::from(file.clone()), bad])
                .unwrap_err();
            assert!(err.contains("non-negative integer"), "{err}");
        }
        // Missing file arg / too many args.
        let err = inst.call(&ctx(), "pick", &[]).unwrap_err();
        assert!(err.contains("pick(file[, index])"), "{err}");
        // Fail-closed without allow_file_actions.
        let mut gated = ctx();
        gated.allow_file_actions = false;
        let err = inst.call(&gated, "pick", &[Value::from(file)]).unwrap_err();
        assert!(err.contains("file actions disabled"), "{err}");
    }

    #[test]
    #[serial_test::file_serial(file_actions)]
    fn random_pick_is_deterministic_per_seed_and_message() {
        let (_dir, file) = array_fixture();
        let mut inst = instance();
        let call = |inst: &mut Box<dyn LibraryInstance>, seed, seq| {
            let mut c = ctx();
            c.seed = seed;
            c.message_seq = seq;
            inst.call(&c, "pick", &[Value::from(file.clone())]).unwrap()
        };
        // Same seed + message → same element; a different message sequence
        // (or seed) eventually yields a different one.
        assert_eq!(call(&mut inst, 7, 1), call(&mut inst, 7, 1));
        let first = call(&mut inst, 7, 1);
        assert!(
            (2..50).any(|seq| call(&mut inst, 7, seq) != first),
            "pick must vary with the message sequence"
        );
        assert!(
            (8..50).any(|seed| call(&mut inst, seed, 1) != first),
            "pick must vary with the seed"
        );
        for _ in 0..50 {
            let v = call(&mut inst, 99, 1);
            assert!(v.contains("token"), "{v}");
        }
    }

    /// Through real generators: two contexts with the same run `seed:` and
    /// VU id derive the same per-instance seed, so `${json.pick(...)}`
    /// replays exactly.
    #[test]
    #[serial_test::file_serial(file_actions)]
    fn random_pick_replays_in_a_seeded_run() {
        let (_dir, file) = array_fixture();
        let make_gen = || {
            let mut ctx = crate::step::context::Context::new();
            ctx.run_seed = Some(42);
            ctx.vu_id = 3;
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
            ctx.new_generator().unwrap()
        };
        let token = format!("${{json.pick(\"{file}\")}}");
        let mut a = make_gen();
        let mut b = make_gen();
        a.begin_message();
        b.begin_message();
        assert_eq!(
            a.expand(&token).unwrap(),
            b.expand(&token).unwrap(),
            "same run seed → same pick"
        );
    }

    #[tokio::test]
    #[serial_test::file_serial(file_actions)]
    async fn pick_shares_the_read_json_cache() {
        let (_dir, file) = array_fixture();
        let reads =
            || crate::step::json_doc::JSON_DISK_READS.load(std::sync::atomic::Ordering::Relaxed);
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
        assert_eq!(reads() - before, 1);
        let mut inst = instance();
        let v = inst
            .call(&ctx(), "pick", &[Value::from(file), Value::from(1)])
            .unwrap();
        assert_eq!(v, r#"{"token":"b"}"#);
        assert_eq!(reads() - before, 1, "pick after read must be a cache hit");
    }
}
