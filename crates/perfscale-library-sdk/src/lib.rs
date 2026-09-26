//! Authoring SDK for perfscale WASM value-generator libraries (RFC 005).
//!
//! A library is a type implementing [`Library`], exported as a WASI Preview 2
//! component with one macro:
//!
//! ```ignore
//! use perfscale_library_sdk::{export_library, Ctx, Error, FunctionInfo, Library};
//!
//! #[derive(Default)]
//! struct MyLib;
//!
//! impl Library for MyLib {
//!     fn functions(&self) -> Vec<FunctionInfo> { /* … */ }
//!     fn call(&mut self, ctx: &mut Ctx, func: &str, args: Vec<serde_json::Value>)
//!         -> Result<String, Error>
//!     { /* … */ }
//! }
//!
//! export_library!(MyLib);
//! ```
//!
//! Build with `cargo build --release --target wasm32-wasip2` (Rust ≥ 1.82
//! emits components natively — no cargo-component needed) and reference the
//! produced `.wasm` from YAML: `libraries: - use: ./target/wasm32-wasip2/release/mylib.wasm`.
//!
//! # Sandboxing notes for authors
//!
//! - Draw all randomness from [`Ctx::prng`] (seeded per instance, so `seed:`
//!   runs are reproducible). `wasi:random` is **not** provided to guests —
//!   avoid `HashMap`/`RandomState` (they pull it in); this SDK deliberately
//!   uses `Vec`-backed maps.
//! - Avoid `println!`/`eprintln!` (imports `wasi:cli/*`, never granted).
//! - `fs` / `clock` / `net` access must be granted explicitly in the YAML
//!   `capabilities:` list; the engine refuses to load a component importing
//!   more than granted.

use serde_json::Value;

// The ABI bindings, generated from the workspace's `wit/library.wit` — the
// single source of truth shared with the host side (`perfscale-core`).
// Hidden: authors use the `Library` trait and `export_library!` instead.
#[doc(hidden)]
#[cfg(target_arch = "wasm32")]
pub mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "perfscale-library",
        // `export_library!` expands in the *author's* crate, so the export
        // macro and the bindings module must both be reachable through
        // `$crate` (= perfscale_library_sdk).
        pub_export_macro: true,
        default_bindings_module: "perfscale_library_sdk::bindings",
    });
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Library error: a message the engine surfaces as the step failure cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self(format!("invalid JSON: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// The context every library call receives (RFC 005 "Call context").
///
/// The SDK keeps one `Ctx` per component instance and refreshes the public
/// fields before every call, so [`Ctx::memo`] state persists across calls of
/// one message and resets when `message_seq` moves on.
pub struct Ctx {
    /// Same counter as the `${seq}` token; bumped per message send.
    pub message_seq: u64,
    /// VU loop iteration.
    pub iteration_seq: u64,
    /// Virtual-user id, for per-VU partitioning (id ranges, comp ids).
    pub vu_id: u64,
    /// Per-instance deterministic seed (`hash(config.seed, vu_id, conn_seq)`).
    pub seed: u64,
    /// Wall clock, unix milliseconds — the only time source a library gets.
    pub time_ms: u64,
    /// Run settings as a JSON object, frozen once at run start — the string
    /// is identical for every call of the run:
    /// `{"vus": N|null, "duration_ms": N|null, "seed": N|null,
    ///   "stages": [...]|null, "arrival": {...}|null, "variables": {...}}`.
    /// `vus`/`duration_ms` are only set for the fixed load profile; staged
    /// and arrival-rate runs carry the profile in `stages`/`arrival`.
    /// `"{}"` for components built against the 0.1 ABI (they never see
    /// settings) and in hand-built contexts. See [`Ctx::settings`].
    pub settings_json: String,
    // --- SDK-managed state (not part of the ABI) ---
    memo_seq: u64,
    // Vec-backed, not HashMap: std's RandomState pulls in `wasi:random`,
    // which the sandbox never provides. Memo maps are tiny.
    memo: Vec<(String, String)>,
    prng: Option<Prng>,
}

impl Ctx {
    /// A fresh context for unit tests / harness use (see [`test_call`]).
    pub fn new(seed: u64) -> Self {
        Self {
            message_seq: 0,
            iteration_seq: 0,
            vu_id: 0,
            seed,
            time_ms: 0,
            settings_json: "{}".to_string(),
            memo_seq: 0,
            memo: Vec::new(),
            prng: None,
        }
    }

    /// The parsed run settings — a convenience over parsing
    /// [`Ctx::settings_json`] yourself. `Value::Null` when the string does
    /// not parse (never a panic).
    pub fn settings(&self) -> Value {
        serde_json::from_str(&self.settings_json).unwrap_or(Value::Null)
    }

    /// The instance's seeded PRNG, created from `seed` on first use. All
    /// randomness a library produces must come from here — that is what makes
    /// `seed:` runs reproducible.
    pub fn prng(&mut self) -> &mut Prng {
        let seed = self.seed;
        self.prng.get_or_insert_with(|| Prng::new(seed))
    }

    /// Keyed reuse within one message (RFC 005 "memo semantics"): returns the
    /// cached value for `key`, computing and caching it via `f` on first use.
    /// The cache is keyed by `(message_seq, key)` and resets as soon as
    /// `message_seq` changes, so a memoized value never leaks into the next
    /// message.
    pub fn memo(
        &mut self,
        key: &str,
        f: impl FnOnce(&mut Self) -> Result<String, Error>,
    ) -> Result<String, Error> {
        if self.message_seq != self.memo_seq {
            self.memo_seq = self.message_seq;
            self.memo.clear();
        }
        if let Some((_, v)) = self.memo.iter().find(|(k, _)| k == key) {
            return Ok(v.clone());
        }
        let v = f(self)?;
        self.memo.push((key.to_string(), v.clone()));
        Ok(v)
    }
}

/// One exported function, as reported by `info()`.
#[derive(Debug, Clone, Copy)]
pub struct FunctionInfo {
    /// Function name as used in `${alias.name(...)}`.
    pub name: &'static str,
    /// One-line description for docs/lint output.
    pub description: &'static str,
    /// Results are secrets: the engine registers them for log masking.
    pub secret: bool,
}

/// The library contract. The engine holds one instance per (library ×
/// generator owner): `init` runs once per instance, `call` serves tokens.
pub trait Library {
    /// Exported functions, surfaced by `info()` for lint/validation.
    fn functions(&self) -> Vec<FunctionInfo>;

    /// Initialize with the YAML `with:` block. The default accepts (and
    /// ignores) any config — override to validate. Failure is fatal to the
    /// run.
    fn init(&mut self, _config: Value) -> Result<(), Error> {
        Ok(())
    }

    /// Invoke `func` with the token's parsed JSON arguments. Unknown
    /// functions and bad arguments are `Err` — the engine fails the step.
    fn call(&mut self, ctx: &mut Ctx, func: &str, args: Vec<Value>) -> Result<String, Error>;
}

/// Call a library directly, with no WASM runtime — the unit-test harness for
/// authors. Build a [`Ctx`], set the fields the test needs, and drive calls:
///
/// ```
/// # use perfscale_library_sdk::{Ctx, Library, FunctionInfo, Error, test_call};
/// # #[derive(Default)] struct L;
/// # impl Library for L {
/// #     fn functions(&self) -> Vec<FunctionInfo> { vec![] }
/// #     fn call(&mut self, _: &mut Ctx, _: &str, _: Vec<serde_json::Value>) -> Result<String, Error> { Ok("x".into()) }
/// # }
/// let mut lib = L;
/// let mut ctx = Ctx::new(42);
/// ctx.message_seq = 1;
/// assert_eq!(test_call(&mut lib, &mut ctx, "f", vec![]).unwrap(), "x");
/// ```
pub fn test_call<L: Library>(
    lib: &mut L,
    ctx: &mut Ctx,
    func: &str,
    args: Vec<Value>,
) -> Result<String, Error> {
    lib.call(ctx, func, args)
}

// ---------------------------------------------------------------------------
// Seeded PRNG — xorshift64, bit-identical to the engine's generator
// ---------------------------------------------------------------------------

/// xorshift64 PRNG — the same algorithm the engine's built-in generator and
/// `@std/random` use, seeded per instance from [`Ctx::seed`]. Not
/// cryptographic; plenty for load data.
pub struct Prng(u64);

impl Prng {
    /// `seed` is forced non-zero (xorshift degenerates at 0).
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform draw in `[0, n)` (0 for `n == 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    /// Uniform integer in `[lo, hi]` inclusive (`lo` if the range is empty).
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let span = (hi - lo + 1) as u64;
        lo + (self.next_u64() % span) as i64
    }

    /// Uniform float in `[0, 1)`.
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

/// Token arguments arrive as a JSON array — these helpers extract and
/// convert, producing author-friendly errors the engine fails the step with.
pub mod args {
    use super::{Error, Value};

    fn got(v: Option<&Value>) -> String {
        match v {
            None => "missing".into(),
            Some(v) => v.to_string(),
        }
    }

    /// Argument `i` as a string (JSON strings unquote; anything else
    /// JSON-encodes). `Err` when absent.
    pub fn string(args: &[Value], i: usize, func: &str) -> Result<String, Error> {
        match args.get(i) {
            Some(Value::String(s)) => Ok(s.clone()),
            other => Err(Error::new(format!(
                "{func}: argument {} must be a string, got {}",
                i + 1,
                got(other)
            ))),
        }
    }

    /// Argument `i` as an integer; strings are parsed leniently.
    pub fn int(args: &[Value], i: usize, func: &str) -> Result<i64, Error> {
        match args.get(i) {
            Some(Value::Number(n)) => n.as_i64().ok_or_else(|| {
                Error::new(format!("{func}: argument {}: {n} is not an integer", i + 1))
            }),
            Some(Value::String(s)) => s.trim().parse().map_err(|_| {
                Error::new(format!(
                    "{func}: argument {}: '{s}' is not an integer",
                    i + 1
                ))
            }),
            other => Err(Error::new(format!(
                "{func}: argument {} must be an integer, got {}",
                i + 1,
                got(other)
            ))),
        }
    }

    /// Argument `i` as a float; strings are parsed leniently.
    pub fn float(args: &[Value], i: usize, func: &str) -> Result<f64, Error> {
        match args.get(i) {
            Some(Value::Number(n)) => Ok(n.as_f64().unwrap_or(0.0)),
            Some(Value::String(s)) => s.trim().parse().map_err(|_| {
                Error::new(format!("{func}: argument {}: '{s}' is not a number", i + 1))
            }),
            other => Err(Error::new(format!(
                "{func}: argument {} must be a number, got {}",
                i + 1,
                got(other)
            ))),
        }
    }

    /// Argument `i` as an optional string — `None` when absent. Use for
    /// trailing memo keys: `token(args::optional_string(&args, 1)?)`.
    pub fn optional_string(args: &[Value], i: usize) -> Option<String> {
        args.get(i).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Export glue (used by `export_library!`; not public API)
// ---------------------------------------------------------------------------

/// Export a [`Library`] implementation as the `perfscale:library` component.
/// The type must implement [`Default`] — the engine constructs it on first
/// use, then calls `init`.
///
/// ```ignore
/// export_library!(MyLib);
/// ```
#[cfg(target_arch = "wasm32")]
#[macro_export]
macro_rules! export_library {
    ($t:ty) => {
        struct __PerfscaleLibraryComponent;

        impl $crate::bindings::exports::perfscale::library::library::Guest
            for __PerfscaleLibraryComponent
        {
            fn info() -> ::std::string::String {
                $crate::__info_json::<$t>(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
            }

            fn init(
                config_json: ::std::string::String,
            ) -> ::std::result::Result<(), ::std::string::String> {
                $crate::__init::<$t>(&config_json)
            }

            fn call(
                ctx: $crate::bindings::exports::perfscale::library::library::Context,
                func_name: ::std::string::String,
                args_json: ::std::string::String,
            ) -> ::std::result::Result<::std::string::String, ::std::string::String> {
                $crate::__call::<$t>(ctx, &func_name, &args_json)
            }
        }

        $crate::bindings::export!(__PerfscaleLibraryComponent with_types_in $crate::bindings);
    };
}

/// Host (non-wasm) builds export nothing — the component export only exists
/// for `wasm32-wasip2`. This is deliberately a no-op rather than a compile
/// error so a library crate stays host-compilable for unit tests via
/// [`test_call`](crate::test_call); the README's build command carries the
/// `--target wasm32-wasip2` requirement.
#[cfg(not(target_arch = "wasm32"))]
#[macro_export]
macro_rules! export_library {
    ($t:ty) => {};
}

// Instance state: one Library + its long-lived Ctx (memo state lives in Ctx,
// so it must survive across calls). wasm32-wasip2 components are
// single-threaded; the thread_local is the instance.
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub mod __rt {
    use super::{Ctx, Error, Library, Value};
    use std::cell::RefCell;

    type State = (Box<dyn Library>, Ctx);

    thread_local! {
        static INSTANCE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    /// A trap (e.g. fuel exhaustion) aborts a call without unwinding it, so
    /// the borrow guard leaks: report the instance as poisoned instead of
    /// panicking across the FFI boundary.
    fn with<R>(f: impl FnOnce(&mut dyn Library, &mut Ctx) -> Result<R, Error>) -> Result<R, Error> {
        INSTANCE.with(|i| {
            let mut guard = i.try_borrow_mut().map_err(|_| {
                Error::new(
                    "library instance is poisoned by an earlier trapped call — the engine should discard it",
                )
            })?;
            let (lib, ctx) = guard
                .as_mut()
                .expect("perfscale library used before construction");
            f(&mut **lib, ctx)
        })
    }

    fn construct<L: Library + Default + 'static>() {
        INSTANCE.with(|i| {
            let mut guard = i.borrow_mut();
            if guard.is_none() {
                *guard = Some((Box::new(L::default()), Ctx::new(0)));
            }
        });
    }

    pub fn info_json<L: Library + Default + 'static>(name: &str, version: &str) -> String {
        let lib = L::default();
        let functions: Vec<Value> = lib
            .functions()
            .iter()
            .map(|f| {
                serde_json::json!({
                    "name": f.name,
                    "description": f.description,
                    "secret": f.secret,
                })
            })
            .collect();
        serde_json::json!({
            // The crate name becomes the default token alias, which must be a
            // lowercase identifier ([a-z][a-z0-9_]*) — dashes are not valid.
            "name": name.replace('-', "_"),
            "version": version,
            "functions": functions,
        })
        .to_string()
    }

    pub fn init<L: Library + Default + 'static>(config_json: &str) -> Result<(), String> {
        let config: Value = if config_json.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(config_json)
                .map_err(|e| format!("init: invalid config JSON: {e}"))?
        };
        construct::<L>();
        with(|lib, _| lib.init(config)).map_err(|e| e.message().to_string())
    }

    pub fn call<L: Library + Default + 'static>(
        ctx: super::bindings::exports::perfscale::library::library::Context,
        func: &str,
        args_json: &str,
    ) -> Result<String, String> {
        let args: Vec<Value> = serde_json::from_str(args_json)
            .map_err(|e| format!("{func}: invalid args JSON: {e}"))?;
        construct::<L>();
        with(|lib, c| {
            c.message_seq = ctx.message_seq;
            c.iteration_seq = ctx.iteration_seq;
            c.vu_id = ctx.vu_id;
            c.seed = ctx.seed;
            c.time_ms = ctx.time_ms;
            c.settings_json = ctx.settings_json;
            lib.call(c, func, args)
        })
        .map_err(|e: Error| e.message().to_string())
    }
}

#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub fn __info_json<L: Library + Default + 'static>(name: &str, version: &str) -> String {
    __rt::info_json::<L>(name, version)
}

#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub fn __init<L: Library + Default + 'static>(config_json: &str) -> Result<(), String> {
    __rt::init::<L>(config_json)
}

#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub fn __call<L: Library + Default + 'static>(
    ctx: bindings::exports::perfscale::library::library::Context,
    func: &str,
    args_json: &str,
) -> Result<String, String> {
    __rt::call::<L>(ctx, func, args_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prng_matches_the_engine_algorithm() {
        // Same xorshift64 as perfscale-core's Gen / @std/random: seeded with
        // 42, these are the first draws every implementation must produce.
        let mut a = Prng::new(42);
        let mut b = Prng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_eq!(Prng::new(0).0, 1, "seed forced non-zero");
    }

    #[test]
    fn prng_helpers_stay_in_range() {
        let mut p = Prng::new(7);
        for _ in 0..1000 {
            assert!(p.below(10) < 10);
            assert!((-5..=5).contains(&p.range(-5, 5)));
            let f = p.f64();
            assert!((0.0..1.0).contains(&f), "{f}");
        }
    }

    #[derive(Default)]
    struct MemoLib;

    impl Library for MemoLib {
        fn functions(&self) -> Vec<FunctionInfo> {
            vec![FunctionInfo {
                name: "id",
                description: "memoized counter",
                secret: false,
            }]
        }

        fn call(&mut self, ctx: &mut Ctx, func: &str, _args: Vec<Value>) -> Result<String, Error> {
            assert_eq!(func, "id");
            ctx.memo("k", |c| Ok(c.prng().next_u64().to_string()))
        }
    }

    #[test]
    fn memo_repeats_within_a_message_and_clears_between() {
        let mut ctx = Ctx::new(1);
        ctx.message_seq = 1;
        let mut n = 0u64;
        let mut next = |_c: &mut Ctx| {
            n += 1;
            Ok(n.to_string())
        };
        let a = ctx.memo("k", &mut next).unwrap();
        assert_eq!(ctx.memo("k", &mut next).unwrap(), a, "same seq, same key");
        assert_ne!(
            ctx.memo("other", &mut next).unwrap(),
            a,
            "other key is independent"
        );
        ctx.message_seq = 2;
        assert_ne!(
            ctx.memo("k", &mut next).unwrap(),
            a,
            "next message recomputes"
        );
    }

    #[test]
    fn test_call_drives_the_trait_directly() {
        let mut lib = MemoLib;
        let mut ctx = Ctx::new(9);
        ctx.message_seq = 1;
        let a = test_call(&mut lib, &mut ctx, "id", vec![]).unwrap();
        let b = test_call(&mut lib, &mut ctx, "id", vec![]).unwrap();
        assert_eq!(a, b, "memoized within one message_seq");
        ctx.message_seq = 2;
        let c = test_call(&mut lib, &mut ctx, "id", vec![]).unwrap();
        assert_ne!(a, c, "fresh after message_seq bumps");
    }

    #[test]
    fn args_helpers_convert_and_err() {
        let v = vec![Value::from("42"), Value::from(2.5), Value::from(true)];
        assert_eq!(args::string(&v, 0, "f").unwrap(), "42");
        assert_eq!(args::int(&v, 0, "f").unwrap(), 42);
        assert_eq!(args::float(&v, 1, "f").unwrap(), 2.5);
        assert!(args::string(&v, 2, "f").is_err());
        assert!(args::int(&v, 5, "f")
            .unwrap_err()
            .message()
            .contains("argument 6"));
        assert_eq!(args::optional_string(&v, 1), Some("2.5".into()));
        assert_eq!(args::optional_string(&v, 9), None);
    }

    #[test]
    fn errors_display_their_message() {
        let e = Error::from("boom");
        assert_eq!(e.to_string(), "boom");
        let e: Error = serde_json::from_str::<Value>("{").unwrap_err().into();
        assert!(e.message().starts_with("invalid JSON"));
    }
}
