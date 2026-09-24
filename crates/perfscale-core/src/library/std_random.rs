//! `@std/random@v1` — the built-in value-generation library (RFC 005).
//!
//! Native (no wasmtime) and dogfooding the exact [`LibraryProvider`] /
//! [`LibraryInstance`] contract external WASM libraries will implement in
//! phase 2. All randomness comes from one xorshift64 PRNG seeded per
//! instance (`hash(config.seed, vu_id, conn_seq)` with `seed:`, random
//! otherwise), so a seeded run reproduces the exact value sequence.
//!
//! Every function accepts an optional trailing `key` argument: within one
//! `message_seq`, repeat calls with the same key return the same value
//! (memoized — see [`MemoCache`]). `${random.uuid4()}` is always fresh;
//! `${random.uuid4(order)}` appearing twice in one message yields one id.

use serde_json::Value;

use super::{CallCtx, FunctionInfo, LibraryInstance, LibraryProvider, MemoCache};
use crate::generate::{civil_from_millis, days_from_civil};

const FUNCTIONS: &[FunctionInfo] = &[
    FunctionInfo {
        name: "uuid4",
        description: "Random RFC 4122 v4 UUID",
        secret: false,
    },
    FunctionInfo {
        name: "uuid7",
        description: "Time-ordered RFC 9562 v7 UUID (millisecond prefix)",
        secret: false,
    },
    FunctionInfo {
        name: "ulid",
        description: "26-char Crockford-base32 ULID (millisecond prefix)",
        secret: false,
    },
    FunctionInfo {
        name: "nanoid",
        description: "URL-safe random id, nanoid alphabet (default len 21)",
        secret: false,
    },
    FunctionInfo {
        name: "int",
        description: "Random integer in [a, b] inclusive",
        secret: false,
    },
    FunctionInfo {
        name: "float",
        description: "Random float in [a, b] with `dp` decimals (default 2)",
        secret: false,
    },
    FunctionInfo {
        name: "pick",
        description: "Random pick among |-separated options",
        secret: false,
    },
    FunctionInfo {
        name: "weighted",
        description: "Weighted random pick among value:weight pairs",
        secret: false,
    },
    FunctionInfo {
        name: "seq",
        description: "Named monotonic counter per instance, starting at 1",
        secret: false,
    },
    FunctionInfo {
        name: "pattern",
        description: "Template fill: # digit, ? a-z, ^ A-Z, * alphanumeric",
        secret: false,
    },
    FunctionInfo {
        name: "first_name",
        description: "Random first name",
        secret: false,
    },
    FunctionInfo {
        name: "last_name",
        description: "Random last name",
        secret: false,
    },
    FunctionInfo {
        name: "name",
        description: "Random full name",
        secret: false,
    },
    FunctionInfo {
        name: "username",
        description: "Random username",
        secret: false,
    },
    FunctionInfo {
        name: "email",
        description: "Random email address",
        secret: false,
    },
    FunctionInfo {
        name: "company",
        description: "Random company name",
        secret: false,
    },
    FunctionInfo {
        name: "lorem",
        description: "Lorem-ipsum words (default 5)",
        secret: false,
    },
    FunctionInfo {
        name: "phone",
        description: "Random phone number",
        secret: false,
    },
    FunctionInfo {
        name: "date",
        description: "Random date in [a, b], YYYY-MM-DD",
        secret: false,
    },
    FunctionInfo {
        name: "timestamp",
        description: "Random unix-ms timestamp in [a, b]",
        secret: false,
    },
    FunctionInfo {
        name: "datetime",
        description: "Random RFC 3339 datetime between dates a and b",
        secret: false,
    },
];

pub struct StdRandomProvider;

impl LibraryProvider for StdRandomProvider {
    fn id(&self) -> &str {
        "@std/random@v1"
    }

    fn functions(&self) -> &[FunctionInfo] {
        FUNCTIONS
    }

    fn instantiate(
        &self,
        config: Option<Value>,
        seed: u64,
    ) -> Result<Box<dyn LibraryInstance>, String> {
        match config {
            None | Some(Value::Null) => Ok(Box::new(StdRandom::new(seed))),
            Some(_) => Err("@std/random@v1 takes no `with:` config".into()),
        }
    }
}

struct StdRandom {
    rng: u64,
    memo: MemoCache,
    /// Named counters for `seq(name)` — per instance, starting at 1.
    counters: std::collections::HashMap<String, u64>,
}

impl StdRandom {
    fn new(seed: u64) -> Self {
        Self {
            rng: seed | 1,
            memo: MemoCache::new(),
            counters: std::collections::HashMap::new(),
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Random integer in `[lo, hi]` inclusive (`lo` if the range is empty).
    fn rand_range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let span = (hi - lo + 1) as u64;
        lo + (self.next_u64() % span) as i64
    }

    fn rand_below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.next_u64() % n
    }

    /// Run `f` through the memo cache when a `key` was passed. The cache is
    /// taken out for the duration so `f` can borrow `self` freely.
    fn memoized(
        &mut self,
        ctx: &CallCtx,
        func: &str,
        key: Option<String>,
        f: impl FnOnce(&mut Self) -> Result<String, String>,
    ) -> Result<String, String> {
        let Some(k) = key else { return f(self) };
        let mut memo = std::mem::take(&mut self.memo);
        let memo_key = format!("{func}:{k}");
        let result = memo.get_or(ctx.message_seq, &memo_key, || f(self));
        self.memo = memo;
        result
    }

    // --- value generators (self-contained so they can pass through memoized) ---

    fn uuid4(&mut self) -> String {
        let b = self.next_u64().to_be_bytes();
        let c = self.next_u64().to_be_bytes();
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&b);
        bytes[8..].copy_from_slice(&c);
        bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
        bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10xx
        format_uuid(&bytes)
    }

    /// RFC 9562 v7: 48-bit unix ms | ver 7 | 12-bit rand | var 10 | 62-bit rand.
    fn uuid7(&mut self, time_ms: u64) -> String {
        let ms = time_ms & 0xffff_ffff_ffff;
        let rand_a = self.next_u64() & 0x0fff;
        let rand_b = self.next_u64() & 0x3fff_ffff_ffff_ffff;
        let hi: u64 = (ms << 16) | (0x7 << 12) | rand_a;
        let lo: u64 = (0b10 << 62) | rand_b;
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&hi.to_be_bytes());
        bytes[8..].copy_from_slice(&lo.to_be_bytes());
        format_uuid(&bytes)
    }

    /// 48-bit unix ms + 80-bit random, Crockford base32, 26 chars.
    fn ulid(&mut self, time_ms: u64) -> String {
        const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut out = String::with_capacity(26);
        // 10 time chars: 48 bits big-endian, 5 bits per char (top 2 bits zero).
        let ms = time_ms & 0xffff_ffff_ffff;
        for i in (0..10).rev() {
            out.push(CROCKFORD[((ms >> (i * 5)) & 0x1f) as usize] as char);
        }
        // 16 random chars = 80 bits, drawn in two chunks (10 chars, then 6 —
        // a u64 yields 12 full 5-bit groups; 10 keeps the math obvious).
        let mut bits = self.next_u64();
        for i in 0..16 {
            if i == 10 {
                bits = self.next_u64();
            }
            out.push(CROCKFORD[(bits & 0x1f) as usize] as char);
            bits >>= 5;
        }
        out
    }

    fn nanoid(&mut self, len: usize) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
        (0..len)
            .map(|_| ALPHABET[self.rand_below(64) as usize] as char)
            .collect()
    }

    fn pick(&mut self, options: &str) -> Result<String, String> {
        let opts: Vec<&str> = options.split('|').map(|s| s.trim()).collect();
        if opts.len() == 1 && opts[0].is_empty() {
            return Err("random.pick: expected pick(a|b|c) with |-separated options".into());
        }
        Ok(opts[self.rand_below(opts.len() as u64) as usize].to_string())
    }

    fn weighted(&mut self, spec: &str) -> Result<String, String> {
        let mut entries: Vec<(&str, f64)> = Vec::new();
        let mut total = 0.0f64;
        for part in spec.split('|') {
            let (value, weight) = part.rsplit_once(':').ok_or_else(|| {
                format!("random.weighted: '{part}' is not a value:weight pair — expected weighted(a:10|b:90)")
            })?;
            let w: f64 = weight
                .trim()
                .parse()
                .map_err(|_| format!("random.weighted: '{part}' has a non-numeric weight"))?;
            if w < 0.0 {
                return Err(format!("random.weighted: '{part}' has a negative weight"));
            }
            total += w;
            entries.push((value.trim(), w));
        }
        if entries.is_empty() || total <= 0.0 {
            return Err("random.weighted: weights must sum to more than zero".into());
        }
        // Draw in [0, total) at milliweight resolution.
        let draw = self.rand_below((total * 1000.0).max(1.0) as u64) as f64 / 1000.0;
        let mut acc = 0.0;
        for (value, w) in &entries {
            acc += w;
            if draw < acc {
                return Ok((*value).to_string());
            }
        }
        Ok(entries.last().unwrap().0.to_string())
    }

    fn pattern(&mut self, template: &str) -> String {
        const ALNUM: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        template
            .chars()
            .map(|c| match c {
                '#' => char::from(b'0' + self.rand_below(10) as u8),
                '?' => char::from(b'a' + self.rand_below(26) as u8),
                '^' => char::from(b'A' + self.rand_below(26) as u8),
                '*' => ALNUM[self.rand_below(62) as usize] as char,
                other => other,
            })
            .collect()
    }

    fn username(&mut self) -> String {
        let first = FIRST_NAMES[self.rand_below(FIRST_NAMES.len() as u64) as usize];
        let last = LAST_NAMES[self.rand_below(LAST_NAMES.len() as u64) as usize];
        format!(
            "{}.{}{}",
            first.to_lowercase(),
            last.to_lowercase(),
            self.rand_range(1, 99)
        )
    }

    fn phone(&mut self) -> String {
        format!(
            "+1-{:03}-{:03}-{:04}",
            self.rand_range(200, 999),
            self.rand_range(200, 999),
            self.rand_range(0, 9999)
        )
    }

    fn lorem(&mut self, words: usize) -> String {
        (0..words.max(1))
            .map(|_| LOREM[self.rand_below(LOREM.len() as u64) as usize])
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl LibraryInstance for StdRandom {
    fn call(&mut self, ctx: &CallCtx, func: &str, args: &[Value]) -> Result<String, String> {
        match func {
            "uuid4" => {
                let key = optional_key(args, 0, func)?;
                self.memoized(ctx, func, key, |s| Ok(s.uuid4()))
            }
            "uuid7" => {
                let key = optional_key(args, 0, func)?;
                self.memoized(ctx, func, key, |s| Ok(s.uuid7(ctx.time_ms)))
            }
            "ulid" => {
                let key = optional_key(args, 0, func)?;
                self.memoized(ctx, func, key, |s| Ok(s.ulid(ctx.time_ms)))
            }
            "nanoid" => {
                let (len, key) = optional_number_then_key(args, func, 21)?;
                self.memoized(ctx, func, key, |s| Ok(s.nanoid(len)))
            }
            "int" => {
                let (a, b) = two_ints(args, func)?;
                let key = optional_key(args, 2, func)?;
                self.memoized(ctx, func, key, |s| Ok(s.rand_range(a, b).to_string()))
            }
            "float" => {
                let (a, b, dp, key) = float_args(args)?;
                self.memoized(ctx, func, key, |s| {
                    let scale = 10f64.powi(dp as i32);
                    let lo = (a * scale) as i64;
                    let hi = (b * scale) as i64;
                    let v = s.rand_range(lo, hi) as f64 / scale;
                    Ok(format!("{v:.dp$}"))
                })
            }
            "pick" => {
                let spec = string_arg(args, 0, func, "pick(a|b|c[, key])")?;
                let key = optional_key(args, 1, func)?;
                self.memoized(ctx, func, key, |s| s.pick(&spec))
            }
            "weighted" => {
                let spec = string_arg(args, 0, func, "weighted(a:10|b:90[, key])")?;
                let key = optional_key(args, 1, func)?;
                self.memoized(ctx, func, key, |s| s.weighted(&spec))
            }
            "seq" => {
                let name = string_arg(args, 0, func, "seq(name)")?;
                if args.len() > 1 {
                    return Err(
                        "random.seq: expected seq(name) — counters are never memoized".to_string(),
                    );
                }
                let next = self.counters.entry(name).or_insert(0);
                *next += 1;
                Ok(next.to_string())
            }
            "pattern" => {
                let template = string_arg(args, 0, func, "pattern(\"ORD-####-????\")")?;
                let key = optional_key(args, 1, func)?;
                self.memoized(ctx, func, key, |s| Ok(s.pattern(&template)))
            }
            "first_name" => {
                let key = optional_key(args, 0, func)?;
                self.memoized(ctx, func, key, |s| {
                    Ok(FIRST_NAMES[s.rand_below(FIRST_NAMES.len() as u64) as usize].to_string())
                })
            }
            "last_name" => {
                let key = optional_key(args, 0, func)?;
                self.memoized(ctx, func, key, |s| {
                    Ok(LAST_NAMES[s.rand_below(LAST_NAMES.len() as u64) as usize].to_string())
                })
            }
            "name" => {
                let key = optional_key(args, 0, func)?;
                self.memoized(ctx, func, key, |s| {
                    let f = FIRST_NAMES[s.rand_below(FIRST_NAMES.len() as u64) as usize];
                    let l = LAST_NAMES[s.rand_below(LAST_NAMES.len() as u64) as usize];
                    Ok(format!("{f} {l}"))
                })
            }
            "username" => {
                let key = optional_key(args, 0, func)?;
                self.memoized(ctx, func, key, |s| Ok(s.username()))
            }
            "email" => {
                let key = optional_key(args, 0, func)?;
                self.memoized(ctx, func, key, |s| {
                    let u = s.username();
                    let d = DOMAINS[s.rand_below(DOMAINS.len() as u64) as usize];
                    Ok(format!("{u}@{d}"))
                })
            }
            "company" => {
                let key = optional_key(args, 0, func)?;
                self.memoized(ctx, func, key, |s| {
                    Ok(COMPANIES[s.rand_below(COMPANIES.len() as u64) as usize].to_string())
                })
            }
            "lorem" => {
                let (words, key) = optional_number_then_key(args, func, 5)?;
                self.memoized(ctx, func, key, |s| Ok(s.lorem(words)))
            }
            "phone" => {
                let key = optional_key(args, 0, func)?;
                self.memoized(ctx, func, key, |s| Ok(s.phone()))
            }
            "date" => {
                let (a, b) = two_dates(args, func)?;
                let key = optional_key(args, 2, func)?;
                self.memoized(ctx, func, key, |s| {
                    let day = s.rand_range(a, b);
                    let (y, m, d, ..) = civil_from_millis((day as u128) * 86_400_000);
                    Ok(format!("{y:04}-{m:02}-{d:02}"))
                })
            }
            "timestamp" => {
                let (a, b) = two_ints(args, func)?;
                let key = optional_key(args, 2, func)?;
                self.memoized(ctx, func, key, |s| Ok(s.rand_range(a, b).to_string()))
            }
            "datetime" => {
                let (a, b) = two_dates(args, func)?;
                let key = optional_key(args, 2, func)?;
                self.memoized(ctx, func, key, |s| {
                    // Inclusive of the whole end day: [a 00:00:00.000, b 23:59:59.999].
                    let lo = a * 86_400_000;
                    let hi = (b + 1) * 86_400_000 - 1;
                    let ms = s.rand_range(lo, hi) as u128;
                    let (y, mo, d, hh, mm, ss, millis) = civil_from_millis(ms);
                    Ok(format!(
                        "{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z"
                    ))
                })
            }
            other => Err(format!(
                "random.{other}: unknown function — @std/random@v1 exports: {}",
                FUNCTIONS
                    .iter()
                    .map(|f| f.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn format_uuid(bytes: &[u8; 16]) -> String {
    let h = |v: u8| format!("{v:02x}");
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h(bytes[0]),
        h(bytes[1]),
        h(bytes[2]),
        h(bytes[3]),
        h(bytes[4]),
        h(bytes[5]),
        h(bytes[6]),
        h(bytes[7]),
        h(bytes[8]),
        h(bytes[9]),
        h(bytes[10]),
        h(bytes[11]),
        h(bytes[12]),
        h(bytes[13]),
        h(bytes[14]),
        h(bytes[15]),
    )
}

/// Stringify a key argument (strings pass through, anything else JSON-encodes).
fn key_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The optional trailing memo key after `positional` arguments.
fn optional_key(args: &[Value], positional: usize, func: &str) -> Result<Option<String>, String> {
    match args.len() {
        n if n <= positional => Ok(None),
        n if n == positional + 1 => Ok(Some(key_of(&args[positional]))),
        _ => Err(format!(
            "random.{func}: too many arguments ({} given) — the only optional trailing argument is the memo key",
            args.len()
        )),
    }
}

/// `[len|words][, key]` for nanoid/lorem: an optional positive-integer first
/// argument, then the optional memo key.
fn optional_number_then_key(
    args: &[Value],
    func: &str,
    default: usize,
) -> Result<(usize, Option<String>), String> {
    let n = match args.first() {
        None => default,
        Some(Value::Number(n)) => n
            .as_u64()
            .filter(|v| *v > 0)
            .ok_or_else(|| format!("random.{func}: expected a positive integer, got {n}"))?
            as usize,
        // A non-numeric first argument is the memo key.
        Some(v) => return Ok((default, Some(key_of(v)))),
    };
    let key = optional_key(args, 1, func)?;
    Ok((n, key))
}

fn string_arg(args: &[Value], i: usize, func: &str, usage: &str) -> Result<String, String> {
    match args.get(i) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(v) => Ok(key_of(v)),
        None => Err(format!("random.{func}: expected {usage}")),
    }
}

fn two_ints(args: &[Value], func: &str) -> Result<(i64, i64), String> {
    let parse = |v: Option<&Value>| -> Result<i64, String> {
        match v {
            Some(Value::Number(n)) => n
                .as_i64()
                .ok_or_else(|| format!("random.{func}: {n} is not an integer")),
            Some(Value::String(s)) => s
                .trim()
                .parse()
                .map_err(|_| format!("random.{func}: '{s}' is not an integer")),
            _ => Err(format!(
                "random.{func}: expected {func}(a, b[, key]) with integer a, b"
            )),
        }
    };
    Ok((parse(args.first())?, parse(args.get(1))?))
}

/// `float(a, b[, dp][, key])` — `dp` defaults to 2; a non-numeric third
/// argument is the memo key.
fn float_args(args: &[Value]) -> Result<(f64, f64, usize, Option<String>), String> {
    let num = |v: Option<&Value>, what: &str| -> Result<f64, String> {
        match v {
            Some(Value::Number(n)) => Ok(n.as_f64().unwrap_or(0.0)),
            Some(Value::String(s)) => s
                .trim()
                .parse()
                .map_err(|_| format!("random.float: '{s}' is not a number ({what})")),
            _ => {
                Err("random.float: expected float(a, b[, dp][, key]) with numeric a, b".to_string())
            }
        }
    };
    let a = num(args.first(), "a")?;
    let b = num(args.get(1), "b")?;
    let mut dp = 2usize;
    let mut key_index = 2;
    if let Some(v) = args.get(2) {
        match v {
            Value::Number(n) => {
                dp = n
                    .as_u64()
                    .filter(|d| *d <= 9)
                    .ok_or_else(|| format!("random.float: dp must be 0..=9, got {n}"))?
                    as usize;
                key_index = 3;
            }
            _ => key_index = 2, // non-numeric third arg is the memo key
        }
    }
    let key = args.get(key_index).map(key_of);
    if args.len() > key_index + 1 {
        return Err("random.float: too many arguments — expected float(a, b[, dp][, key])".into());
    }
    Ok((a, b, dp, key))
}

/// Parse `YYYY-MM-DD` into days since the unix epoch.
fn parse_date(s: &str, func: &str) -> Result<i64, String> {
    let bad = || format!("random.{func}: '{s}' is not a YYYY-MM-DD date");
    let mut parts = s.split('-');
    let y: i64 = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    let m: u32 = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    let d: u32 = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(bad());
    }
    Ok(days_from_civil(y, m, d))
}

fn two_dates(args: &[Value], func: &str) -> Result<(i64, i64), String> {
    let date = |v: Option<&Value>| -> Result<i64, String> {
        match v {
            Some(Value::String(s)) => parse_date(s, func),
            _ => Err(format!(
                "random.{func}: expected {func}(a, b[, key]) with YYYY-MM-DD dates"
            )),
        }
    };
    Ok((date(args.first())?, date(args.get(1))?))
}

// ---------------------------------------------------------------------------
// Embedded corpora (small, deterministic — phase 2 WASM libraries can ship
// full faker datasets; these cover smoke-test-shaped needs)
// ---------------------------------------------------------------------------

const FIRST_NAMES: &[&str] = &[
    "Ada", "Alan", "Alice", "Amara", "Boris", "Carlos", "Chen", "Clara", "Dmitri", "Elena",
    "Elias", "Fatima", "Finn", "Grace", "Hana", "Hugo", "Ida", "Igor", "Ingrid", "Ivan", "James",
    "Jane", "Jonas", "Katya", "Kenji", "Lars", "Lea", "Linus", "Lucia", "Margaret", "Maria",
    "Mateo", "Mira", "Nadia", "Nils", "Nina", "Omar", "Oscar", "Priya", "Ravi", "Rosa", "Sofia",
    "Sven", "Tara", "Theo", "Turing", "Vera", "Viktor", "Yuki", "Zoe",
];

const LAST_NAMES: &[&str] = &[
    "Almeida",
    "Andersson",
    "Bakker",
    "Berger",
    "Bianchi",
    "Brown",
    "Chen",
    "Costa",
    "Dubois",
    "Fischer",
    "Garcia",
    "Gruber",
    "Hansen",
    "Hoffman",
    "Ivanov",
    "Jensen",
    "Johnson",
    "Kato",
    "Keller",
    "Kim",
    "Kowalski",
    "Larsen",
    "Lindqvist",
    "Lovelace",
    "Martinez",
    "Meyer",
    "Miller",
    "Moreau",
    "Nakamura",
    "Nguyen",
    "Novak",
    "Nowak",
    "Petrov",
    "Rossi",
    "Santos",
    "Schmidt",
    "Silva",
    "Smith",
    "Sokolov",
    "Tanaka",
    "Virtanen",
    "Weber",
    "Weiss",
    "Williams",
    "Yamamoto",
    "Zhang",
];

const COMPANIES: &[&str] = &[
    "Acme Corp",
    "Aperture Labs",
    "Black Mesa",
    "Blue Sun",
    "Cyberdyne",
    "DataDyne",
    "Echelon Systems",
    "Globex",
    "Hooli",
    "Initech",
    "Initrode",
    "Kramerica",
    "Massive Dynamic",
    "MomCorp",
    "Nakatomi Trading",
    "Northwind",
    "Oscorp",
    "Pied Piper",
    "Prestige Worldwide",
    "Raviga",
    "Soylent Corp",
    "Stark Industries",
    "Tyrell Corp",
    "Umbrella Corp",
    "Vandelay Industries",
    "Virtucon",
    "Wayne Enterprises",
    "Weyland-Yutani",
    "Wonka Industries",
    "Yoyodyne",
];

const DOMAINS: &[&str] = &[
    "example.com",
    "example.org",
    "example.net",
    "mail.test",
    "acme.test",
    "demo.dev",
    "sample.io",
    "loadtest.dev",
    "staging.example.com",
    "perfscale.dev",
];

const LOREM: &[&str] = &[
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
    "eiusmod",
    "tempor",
    "incididunt",
    "ut",
    "labore",
    "et",
    "dolore",
    "magna",
    "aliqua",
    "enim",
    "ad",
    "minim",
    "veniam",
    "quis",
    "nostrud",
    "exercitation",
    "ullamco",
    "laboris",
    "nisi",
    "aliquip",
    "ex",
    "ea",
    "commodo",
    "consequat",
    "duis",
    "aute",
    "irure",
    "in",
    "reprehenderit",
    "voluptate",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(seed: u64) -> Box<dyn LibraryInstance> {
        StdRandomProvider.instantiate(None, seed).unwrap()
    }

    fn ctx() -> CallCtx {
        CallCtx {
            message_seq: 1,
            iteration_seq: 1,
            vu_id: 1,
            seed: 42,
            time_ms: 1_784_160_000_000, // 2026-07-16T00:00:00.000Z
        }
    }

    fn call(inst: &mut Box<dyn LibraryInstance>, func: &str, args: &[Value]) -> String {
        inst.call(&ctx(), func, args).unwrap()
    }

    #[test]
    fn uuid4_has_version_and_variant_bits() {
        let mut inst = instance(7);
        for _ in 0..100 {
            let v = call(&mut inst, "uuid4", &[]);
            assert_eq!(v.len(), 36, "{v}");
            assert_eq!(&v[14..15], "4", "version nibble: {v}");
            assert!(matches!(&v[19..20], "8" | "9" | "a" | "b"), "variant: {v}");
            assert!(v.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{v}");
        }
    }

    #[test]
    fn uuid7_starts_with_the_timestamp_and_has_v7_bits() {
        let mut inst = instance(7);
        let v = call(&mut inst, "uuid7", &[]);
        assert_eq!(&v[14..15], "7", "{v}");
        assert!(matches!(&v[19..20], "8" | "9" | "a" | "b"), "{v}");
        // 48-bit ms → the first 12 hex digits (dashes stripped).
        let expected: String = format!("{:012x}", 1_784_160_000_000u64);
        assert_eq!(v.replace('-', "")[..12], expected, "{v}");
    }

    #[test]
    fn ulid_is_26_crockford_chars_with_time_prefix() {
        let mut inst = instance(7);
        let v = call(&mut inst, "ulid", &[]);
        assert_eq!(v.len(), 26, "{v}");
        assert!(
            v.chars()
                .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c)),
            "{v}"
        );
        // Same timestamp → same first 10 chars across instances.
        let mut inst2 = instance(999);
        let v2 = call(&mut inst2, "ulid", &[]);
        assert_eq!(&v[..10], &v2[..10]);
    }

    #[test]
    fn nanoid_defaults_and_respects_len() {
        let mut inst = instance(7);
        let v = call(&mut inst, "nanoid", &[]);
        assert_eq!(v.len(), 21, "{v}");
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "{v}"
        );
        let v = call(&mut inst, "nanoid", &[Value::from(8)]);
        assert_eq!(v.len(), 8, "{v}");
    }

    #[test]
    fn int_stays_in_range() {
        let mut inst = instance(7);
        for _ in 0..500 {
            let v: i64 = call(&mut inst, "int", &[Value::from(-5), Value::from(5)])
                .parse()
                .unwrap();
            assert!((-5..=5).contains(&v), "{v}");
        }
    }

    #[test]
    fn float_respects_bounds_and_dp() {
        let mut inst = instance(7);
        for _ in 0..200 {
            let s = call(
                &mut inst,
                "float",
                &[Value::from(1.5), Value::from(2.5), Value::from(3)],
            );
            let v: f64 = s.parse().unwrap();
            assert!((1.5..=2.5).contains(&v), "{v}");
            assert_eq!(s.split('.').nth(1).unwrap().len(), 3, "{s}");
        }
        // Default dp is 2.
        let s = call(&mut inst, "float", &[Value::from(0), Value::from(1)]);
        assert_eq!(s.split('.').nth(1).unwrap().len(), 2, "{s}");
    }

    #[test]
    fn pick_chooses_among_options() {
        let mut inst = instance(7);
        for _ in 0..100 {
            let v = call(&mut inst, "pick", &[Value::from("a|b|c")]);
            assert!(["a", "b", "c"].contains(&v.as_str()), "{v}");
        }
    }

    #[test]
    fn weighted_roughly_follows_the_weights() {
        let mut inst = instance(7);
        let mut a = 0;
        let mut b = 0;
        for _ in 0..2000 {
            match call(&mut inst, "weighted", &[Value::from("a:10|b:90")]).as_str() {
                "a" => a += 1,
                "b" => b += 1,
                other => panic!("unexpected: {other}"),
            }
        }
        // 10% of 2000 = 200; a modulo-based PRNG should land well within ±5%.
        assert!(a > 100 && a < 300, "a={a} b={b}");
    }

    #[test]
    fn weighted_rejects_bad_specs() {
        let mut inst = instance(7);
        let e = inst
            .call(&ctx(), "weighted", &[Value::from("a|b")])
            .unwrap_err();
        assert!(e.contains("value:weight"), "{e}");
        let e = inst
            .call(&ctx(), "weighted", &[Value::from("a:0|b:0")])
            .unwrap_err();
        assert!(e.contains("more than zero"), "{e}");
    }

    #[test]
    fn seq_counts_per_name_from_one() {
        let mut inst = instance(7);
        assert_eq!(call(&mut inst, "seq", &[Value::from("order")]), "1");
        assert_eq!(call(&mut inst, "seq", &[Value::from("order")]), "2");
        assert_eq!(call(&mut inst, "seq", &[Value::from("fill")]), "1");
    }

    #[test]
    fn pattern_maps_each_marker() {
        let mut inst = instance(7);
        for _ in 0..50 {
            let v = call(&mut inst, "pattern", &[Value::from("ORD-####-????-^^-**")]);
            let parts: Vec<&str> = v.split('-').collect();
            assert_eq!(parts[0], "ORD", "{v}");
            assert!(parts[1].chars().all(|c| c.is_ascii_digit()), "{v}");
            assert!(parts[2].chars().all(|c| c.is_ascii_lowercase()), "{v}");
            assert!(parts[3].chars().all(|c| c.is_ascii_uppercase()), "{v}");
            assert!(parts[4].chars().all(|c| c.is_ascii_alphanumeric()), "{v}");
        }
    }

    #[test]
    fn faker_functions_have_the_right_shape() {
        let mut inst = instance(7);
        let first = call(&mut inst, "first_name", &[]);
        assert!(FIRST_NAMES.contains(&first.as_str()), "{first}");
        let name = call(&mut inst, "name", &[]);
        assert_eq!(name.split(' ').count(), 2, "{name}");
        let email = call(&mut inst, "email", &[]);
        assert!(
            email.contains('@') && DOMAINS.contains(&email.split('@').nth(1).unwrap()),
            "{email}"
        );
        let company = call(&mut inst, "company", &[]);
        assert!(COMPANIES.contains(&company.as_str()), "{company}");
        let lorem = call(&mut inst, "lorem", &[]);
        assert_eq!(lorem.split(' ').count(), 5, "{lorem}");
        let lorem = call(&mut inst, "lorem", &[Value::from(3)]);
        assert_eq!(lorem.split(' ').count(), 3, "{lorem}");
        let phone = call(&mut inst, "phone", &[]);
        assert!(phone.starts_with("+1-") && phone.len() == 15, "{phone}");
    }

    #[test]
    fn date_stays_in_range() {
        let mut inst = instance(7);
        for _ in 0..100 {
            let v = call(
                &mut inst,
                "date",
                &[Value::from("2026-01-01"), Value::from("2026-01-31")],
            );
            assert!(v.starts_with("2026-01-"), "{v}");
            let day: u32 = v[8..10].parse().unwrap();
            assert!((1..=31).contains(&day), "{v}");
        }
    }

    #[test]
    fn timestamp_stays_in_range() {
        let mut inst = instance(7);
        for _ in 0..100 {
            let v: i64 = call(
                &mut inst,
                "timestamp",
                &[Value::from(1000), Value::from(2000)],
            )
            .parse()
            .unwrap();
            assert!((1000..=2000).contains(&v), "{v}");
        }
    }

    #[test]
    fn datetime_is_rfc3339_between_the_dates() {
        let mut inst = instance(7);
        for _ in 0..100 {
            let v = call(
                &mut inst,
                "datetime",
                &[Value::from("2026-07-15"), Value::from("2026-07-16")],
            );
            assert_eq!(v.len(), 24, "{v}");
            assert!(
                v.starts_with("2026-07-15T") || v.starts_with("2026-07-16T"),
                "{v}"
            );
            assert!(v.ends_with('Z'), "{v}");
        }
    }

    #[test]
    fn memo_key_repeats_within_a_message_only() {
        let mut inst = instance(7);
        let mut c = ctx();
        let args = [Value::from("order")];
        let a = inst.call(&c, "uuid4", &args).unwrap();
        assert_eq!(
            inst.call(&c, "uuid4", &args).unwrap(),
            a,
            "same message, same key"
        );
        let b = inst.call(&c, "uuid4", &[Value::from("other")]).unwrap();
        assert_ne!(a, b, "different key → different value");
        c.message_seq = 2;
        assert_ne!(
            inst.call(&c, "uuid4", &args).unwrap(),
            a,
            "next message → fresh"
        );
    }

    #[test]
    fn same_seed_reproduces_the_sequence() {
        let mut a = instance(123);
        let mut b = instance(123);
        for func in ["uuid4", "ulid", "nanoid", "first_name", "company"] {
            assert_eq!(
                call(&mut a, func, &[]),
                call(&mut b, func, &[]),
                "{func} diverged"
            );
        }
    }

    #[test]
    fn unknown_function_lists_the_exports() {
        let mut inst = instance(7);
        let e = inst.call(&ctx(), "uuid9", &[]).unwrap_err();
        assert!(e.contains("unknown function") && e.contains("uuid4"), "{e}");
    }

    #[test]
    fn bad_args_are_helpful_errors() {
        let mut inst = instance(7);
        let e = inst.call(&ctx(), "int", &[Value::from("x")]).unwrap_err();
        assert!(e.contains("random.int"), "{e}");
        let e = inst
            .call(
                &ctx(),
                "date",
                &[Value::from("July"), Value::from("2026-01-01")],
            )
            .unwrap_err();
        assert!(e.contains("YYYY-MM-DD"), "{e}");
        let e = inst.call(&ctx(), "pick", &[]).unwrap_err();
        assert!(e.contains("pick(a|b|c"), "{e}");
    }

    #[test]
    fn with_config_is_rejected() {
        let e = match StdRandomProvider.instantiate(Some(Value::from("x")), 1) {
            Ok(_) => panic!("config must be rejected"),
            Err(e) => e,
        };
        assert!(e.contains("takes no `with:` config"), "{e}");
    }
}
