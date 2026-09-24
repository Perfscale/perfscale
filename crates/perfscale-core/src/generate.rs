//! Per-send dynamic value generation for protocol messages.
//!
//! Message payloads may embed **single-brace** tokens that are expanded anew
//! each time a message is sent — so a repeated order gets a fresh id, a live
//! timestamp, a random price, and so on. Single-brace `${…}` is deliberately
//! distinct from the engine's `${{ … }}` interpolation, which is resolved once
//! before the action runs and left untouched here.
//!
//! Used by `std/ws@v1`/`std/ws-send@v1` for streaming unique messages from one
//! template and, uniformly, by every action that carries string payloads —
//! `std/http` (URL, headers, body), raw TCP/UDP, LLM prompts, DB bind
//! parameters, and pub/sub messages all expand through the same code path
//! (RFC 005 "Expansion coverage"); the proprietary FIX actions share the same
//! expander so `${…}` means exactly one thing across all protocols.
//!
//! | Token | Expands to |
//! |-------|------------|
//! | `${seq}` | Monotonic counter, unique per message send (shared by all expansions in that message) |
//! | `${uuid}` | A random 32-hex-char id |
//! | `${now}` | Current UTC time in FIX format `YYYYMMDD-HH:MM:SS.sss` |
//! | `${now_ms}` | Current unix time in milliseconds |
//! | `${now_iso}` | Current UTC time as RFC 3339 `YYYY-MM-DDTHH:MM:SS.sssZ` |
//! | `${rand(a,b)}` | Random integer in `[a, b]` |
//! | `${randf(a,b)}` / `${randf(a,b,dp)}` | Random float in `[a, b]`, `dp` decimals (default 2) |
//! | `${choice(x\|y\|z)}` | A random pick among the `\|`-separated options |
//!
//! Unknown tokens are left verbatim.
//!
//! ## Library tokens (RFC 005)
//!
//! Libraries declared under `libraries:` in the test/config file register
//! behind an alias and extend the token set:
//!
//! | Token | Expands to |
//! |-------|------------|
//! | `${alias.fn(args)}` | The result of calling function `fn` of the library bound to `alias` |
//!
//! Built-in tokens match first and keep their exact behavior. After a
//! built-in miss, a token containing a `.` resolves as a library call: an
//! **unknown alias** leaves the token verbatim (as any unknown token); a
//! **known alias with an unknown function — or a failed call — is a hard
//! error** and fails the step (the alias is a declared contract; a load test
//! must never send wrong data and report green).
//!
//! ### Argument mapping (interface contract)
//!
//! Token args arrive as text (`${random.int(1,100)}`) but libraries speak
//! JSON; this mapping is the contract every library implementation relies
//! on, in exactly this order: split the args text on commas **not inside
//! double quotes** → trim each part → strip one pair of surrounding double
//! quotes if present → try `serde_json::from_str` (numbers, booleans, null,
//! arrays, objects parse as JSON) → anything that does not parse becomes a
//! JSON string. So `pick(a|b|c)` passes one string `"a|b|c"`,
//! `int(1,100)` passes `[1, 100]`, and `pattern("ORD-####-????")` passes
//! `"ORD-####-????"` with the quotes removed.

/// Per-session generator state: a message-send counter plus a small PRNG.
///
/// Not cryptographic — an xorshift64 seeded per session is plenty for load
/// data (unique ids, varied prices) and avoids a dependency.
pub struct Gen {
    seq: u64,
    rng: u64,
    /// `${alias.fn(...)}` resolvers: alias → library instance (RFC 005).
    /// Built-in tokens match first; libraries are only consulted after a
    /// built-in miss on a token containing a `.`.
    libraries: Vec<(String, Box<dyn crate::library::LibraryInstance>)>,
    /// The seed this generator was built with — reported to libraries as
    /// `CallCtx.seed`.
    seed: u64,
    /// VU id / loop iteration for the library call context; 0 in hand-built
    /// generators (unit tests, one-shot actions without a VU context).
    vu_id: u64,
    iteration_seq: u64,
    /// Run-scoped per-library metrics recorder (RFC 005), shared by every
    /// generator of the run; `None` in hand-built generators.
    library_metrics: Option<std::sync::Arc<crate::library::LibraryMetrics>>,
}

impl Gen {
    /// New generator. `seed` is forced non-zero (xorshift degenerates at 0).
    pub fn new(seed: u64) -> Self {
        Gen {
            seq: 0,
            rng: seed | 1,
            libraries: Vec::new(),
            seed,
            vu_id: 0,
            iteration_seq: 0,
            library_metrics: None,
        }
    }

    /// Set the VU identity carried into library call contexts.
    pub fn with_vu(mut self, vu_id: u64, iteration_seq: u64) -> Self {
        self.vu_id = vu_id;
        self.iteration_seq = iteration_seq;
        self
    }

    /// Attach the run's shared per-library metrics recorder: every library
    /// call this generator makes is counted and timed into it (RFC 005).
    pub fn with_library_metrics(
        mut self,
        metrics: std::sync::Arc<crate::library::LibraryMetrics>,
    ) -> Self {
        self.library_metrics = Some(metrics);
        self
    }

    /// Bind a library instance behind `alias` for `${alias.fn(...)}` tokens.
    pub fn attach_library(
        &mut self,
        alias: impl Into<String>,
        instance: Box<dyn crate::library::LibraryInstance>,
    ) {
        self.libraries.push((alias.into(), instance));
    }

    /// Advance to the next message: bumps the `${seq}` counter so every
    /// expansion in one message shares the same sequence value.
    pub fn begin_message(&mut self) {
        self.seq += 1;
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

    /// Expand every `${…}` token in `template`. Called once per payload per
    /// send; `begin_message` must have been called first for `${seq}`.
    ///
    /// Fails only on a library call error (known alias, unknown function or
    /// bad arguments — see the module docs); the caller must fail the step.
    pub fn expand(&mut self, template: &str) -> Result<String, String> {
        if !template.contains("${") {
            return Ok(template.to_string());
        }
        let bytes = template.as_bytes();
        let mut out = String::with_capacity(template.len());
        let mut i = 0;
        while i < bytes.len() {
            // A `${{` belongs to the engine's interpolation layer — leave it
            // (and its content) alone by copying the first char and moving on.
            if bytes[i] == b'$'
                && i + 1 < bytes.len()
                && bytes[i + 1] == b'{'
                && bytes.get(i + 2) != Some(&b'{')
            {
                if let Some(close) = template[i + 2..].find('}') {
                    let token = &template[i + 2..i + 2 + close];
                    match self.eval(token)? {
                        Some(v) => out.push_str(&v),
                        None => out.push_str(&format!("${{{token}}}")),
                    }
                    i = i + 2 + close + 1;
                    continue;
                }
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        Ok(out)
    }

    /// Evaluate one token's inner text (without the `${` `}`). `Ok(None)` →
    /// unknown token, left verbatim by the caller. `Err` → a library call on
    /// a known alias failed (unknown function or bad arguments); the step
    /// must fail with the message.
    fn eval(&mut self, token: &str) -> Result<Option<String>, String> {
        Ok(match token {
            "seq" => Some(self.seq.to_string()),
            "uuid" => Some(format!("{:016x}{:016x}", self.next_u64(), self.next_u64())),
            "now" => Some(now_fix()),
            "now_ms" => Some(now_unix_millis().to_string()),
            "now_iso" => Some(now_iso()),
            _ => {
                let Some((name, args)) = parse_call(token) else {
                    return self.eval_library(token);
                };
                match name {
                    "rand" => {
                        let Some((a, b)) = two_ints(&args) else {
                            return self.eval_library(token);
                        };
                        Some(self.rand_range(a, b).to_string())
                    }
                    "randf" => {
                        let Some(v) = self.eval_randf(&args) else {
                            return self.eval_library(token);
                        };
                        Some(v)
                    }
                    "choice" => {
                        let opts: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                        if opts.is_empty() {
                            return self.eval_library(token);
                        }
                        let idx = (self.next_u64() % opts.len() as u64) as usize;
                        Some(opts[idx].trim().to_string())
                    }
                    _ => return self.eval_library(token),
                }
            }
        })
    }

    fn eval_randf(&mut self, args: &[String]) -> Option<String> {
        let a: f64 = args.first()?.trim().parse().ok()?;
        let b: f64 = args.get(1)?.trim().parse().ok()?;
        let dp: usize = args.get(2).and_then(|s| s.trim().parse().ok()).unwrap_or(2);
        // Scale to integer units at `dp` precision, pick, rescale.
        let scale = 10f64.powi(dp as i32);
        let lo = (a * scale) as i64;
        let hi = (b * scale) as i64;
        let v = self.rand_range(lo, hi) as f64 / scale;
        Some(format!("{v:.*}", dp))
    }

    /// Library token resolution after every built-in missed: `alias.fn(args)`
    /// splits at the first `.`. Unknown alias → `Ok(None)` (verbatim, like
    /// any unknown token). Known alias → the call runs; unknown functions and
    /// call failures are `Err` (step failure).
    fn eval_library(&mut self, token: &str) -> Result<Option<String>, String> {
        let Some(dot) = token.find('.') else {
            return Ok(None);
        };
        let alias = &token[..dot];
        if !self.libraries.iter().any(|(a, _)| a == alias) {
            return Ok(None);
        }
        let (func, args) = parse_library_call(&token[dot + 1..]).ok_or_else(|| {
            format!("invalid library call '${{{token}}}' — expected ${{alias.fn(args)}}")
        })?;
        let call_ctx = crate::library::CallCtx {
            message_seq: self.seq,
            iteration_seq: self.iteration_seq,
            vu_id: self.vu_id,
            seed: self.seed,
            time_ms: now_unix_millis() as u64,
        };
        let instance = &mut self
            .libraries
            .iter_mut()
            .find(|(a, _)| a == alias)
            .expect("alias checked above")
            .1;
        match &self.library_metrics {
            Some(recorder) => {
                let started = std::time::Instant::now();
                let result = instance.call(&call_ctx, func, &args);
                recorder.record(alias, started.elapsed(), result.is_ok());
                result
            }
            None => instance.call(&call_ctx, func, &args),
        }
        .map(Some)
        .map_err(|e| format!("${{{token}}}: {e}"))
    }
}

// ---------------------------------------------------------------------------
// JSON payload expansion — shared by every protocol family
// ---------------------------------------------------------------------------

/// True when any string leaf of `v` contains a `${` token start. Keys are
/// never expanded, so only values are scanned — the cheap gate callers use to
/// skip generator construction on token-free payloads.
pub fn contains_token(v: &serde_json::Value) -> bool {
    use serde_json::Value;
    match v {
        Value::String(s) => s.contains("${"),
        Value::Array(a) => a.iter().any(contains_token),
        Value::Object(m) => m.values().any(contains_token),
        _ => false,
    }
}

/// Expand `${…}` tokens in every string leaf of a JSON value (object keys are
/// never expanded). Shared by the gRPC payloads, GraphQL variables, and any
/// future protocol whose parameters carry generator tokens. Fails on the
/// first library call error (see [`Gen::expand`]).
pub fn expand_tokens(
    v: &serde_json::Value,
    generator: &mut Gen,
) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    Ok(match v {
        Value::String(s) => Value::String(generator.expand(s)?),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| expand_tokens(x, generator))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| Ok((k.clone(), expand_tokens(x, generator)?)))
                .collect::<Result<serde_json::Map<_, _>, String>>()?,
        ),
        other => other.clone(),
    })
}

// ---------------------------------------------------------------------------
// Wall-clock formatting (no chrono dependency)
// ---------------------------------------------------------------------------

pub(crate) fn now_unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Split unix milliseconds into (y, m, d, hh, mm, ss, ms) in UTC using the
/// days-from-civil inverse (Howard Hinnant's algorithm) — exact for the whole
/// Gregorian range, no leap-second handling (unix time has none).
pub(crate) fn civil_from_millis(ms: u128) -> (i64, u32, u32, u32, u32, u32, u32) {
    let secs = (ms / 1000) as i64;
    let millis = (ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, hh, mm, ss, millis)
}

/// Current UTC time in FIX SendingTime format `YYYYMMDD-HH:MM:SS.sss`.
fn now_fix() -> String {
    let (y, mo, d, hh, mm, ss, ms) = civil_from_millis(now_unix_millis());
    format!("{y:04}{mo:02}{d:02}-{hh:02}:{mm:02}:{ss:02}.{ms:03}")
}

/// Current UTC time as RFC 3339 `YYYY-MM-DDTHH:MM:SS.sssZ`.
fn now_iso() -> String {
    let (y, mo, d, hh, mm, ss, ms) = civil_from_millis(now_unix_millis());
    format!("{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{ms:03}Z")
}

/// Days since the unix epoch for a Gregorian (y, m, d) — Howard Hinnant's
/// days_from_civil, the inverse of [`civil_from_millis`]'s date part. Shared
/// with the library module for date-range generators.
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse `name(a,b,c)` → `("name", ["a","b","c"])`. `choice` splits on `|`,
/// everything else on `,`.
fn parse_call(token: &str) -> Option<(&str, Vec<String>)> {
    let open = token.find('(')?;
    if !token.ends_with(')') {
        return None;
    }
    let name = &token[..open];
    let inner = &token[open + 1..token.len() - 1];
    let sep = if name == "choice" { '|' } else { ',' };
    let args = inner.split(sep).map(|s| s.to_string()).collect();
    Some((name, args))
}

fn two_ints(args: &[String]) -> Option<(i64, i64)> {
    let a = args.first()?.trim().parse().ok()?;
    let b = args.get(1)?.trim().parse().ok()?;
    Some((a, b))
}

/// Parse the call part of a library token (`fn(args)` — the alias is already
/// stripped) into the function name and JSON args. The text-to-JSON mapping
/// is the interface contract documented at the top of this module.
fn parse_library_call(call: &str) -> Option<(&str, Vec<serde_json::Value>)> {
    let open = call.find('(')?;
    if !call.ends_with(')') {
        return None;
    }
    let name = &call[..open];
    if name.is_empty() {
        return None;
    }
    let inner = &call[open + 1..call.len() - 1];
    if inner.trim().is_empty() {
        return Some((name, Vec::new()));
    }
    Some((
        name,
        split_args(inner).iter().map(|s| arg_to_json(s)).collect(),
    ))
}

/// Split on commas not inside double quotes (quotes stay in the parts —
/// [`arg_to_json`] strips them).
fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in text.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(ch);
            }
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    out.push(cur);
    out
}

/// One argument text → JSON value: trim, strip one pair of surrounding
/// double quotes, parse as JSON, fall back to a plain string.
fn arg_to_json(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    let inner = if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    serde_json::from_str(inner).unwrap_or_else(|_| serde_json::Value::String(inner.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_string_passes_through() {
        let mut g = Gen::new(1);
        assert_eq!(g.expand("EURUSD").unwrap(), "EURUSD");
    }

    #[test]
    fn seq_is_shared_within_a_message_and_bumps_per_message() {
        let mut g = Gen::new(1);
        g.begin_message();
        assert_eq!(g.expand("order-${seq}").unwrap(), "order-1");
        assert_eq!(g.expand("dup-${seq}").unwrap(), "dup-1"); // same message → same seq
        g.begin_message();
        assert_eq!(g.expand("order-${seq}").unwrap(), "order-2");
    }

    #[test]
    fn rand_stays_in_range() {
        let mut g = Gen::new(42);
        for _ in 0..1000 {
            let v: i64 = g.expand("${rand(10,20)}").unwrap().parse().unwrap();
            assert!((10..=20).contains(&v), "out of range: {v}");
        }
    }

    #[test]
    fn randf_respects_bounds_and_decimals() {
        let mut g = Gen::new(7);
        for _ in 0..500 {
            let s = g.expand("${randf(1.0,2.0,3)}").unwrap();
            let v: f64 = s.parse().unwrap();
            assert!((1.0..=2.0).contains(&v), "out of range: {v}");
            // 3 decimal places in the rendered string.
            assert_eq!(s.split('.').nth(1).unwrap().len(), 3, "dp: {s}");
        }
    }

    #[test]
    fn choice_picks_one_option() {
        let mut g = Gen::new(3);
        for _ in 0..100 {
            let v = g.expand("${choice(1|2)}").unwrap();
            assert!(v == "1" || v == "2", "unexpected: {v}");
        }
    }

    #[test]
    fn uuid_is_hex_and_varies() {
        let mut g = Gen::new(9);
        let a = g.expand("${uuid}").unwrap();
        let b = g.expand("${uuid}").unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn now_has_fix_timestamp_shape() {
        let mut g = Gen::new(1);
        let ts = g.expand("${now}").unwrap();
        // YYYYMMDD-HH:MM:SS.sss
        assert_eq!(ts.len(), 21, "{ts}");
        assert_eq!(&ts[8..9], "-");
    }

    #[test]
    fn now_ms_is_plausible_unix_millis() {
        let mut g = Gen::new(1);
        let v: u128 = g.expand("${now_ms}").unwrap().parse().unwrap();
        // After 2020-01-01 and before 2100-01-01.
        assert!(v > 1_577_836_800_000 && v < 4_102_444_800_000, "{v}");
    }

    #[test]
    fn now_iso_has_rfc3339_shape() {
        let mut g = Gen::new(1);
        let ts = g.expand("${now_iso}").unwrap();
        // YYYY-MM-DDTHH:MM:SS.sssZ
        assert_eq!(ts.len(), 24, "{ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
        assert!(ts.ends_with('Z'));
    }

    #[test]
    fn civil_from_millis_known_dates() {
        // 2026-07-16 00:00:00.000 UTC
        assert_eq!(
            civil_from_millis(1_784_160_000_000),
            (2026, 7, 16, 0, 0, 0, 0)
        );
        // Epoch.
        assert_eq!(civil_from_millis(0), (1970, 1, 1, 0, 0, 0, 0));
        // Leap day 2024-02-29 12:34:56.789.
        assert_eq!(
            civil_from_millis(1_709_210_096_789),
            (2024, 2, 29, 12, 34, 56, 789)
        );
    }

    #[test]
    fn days_from_civil_round_trips_through_civil_from_millis() {
        for (y, m, d) in [(1970, 1, 1), (2024, 2, 29), (2026, 7, 16), (2000, 12, 31)] {
            let days = days_from_civil(y, m, d);
            let (yy, mm, dd, hh, mi, ss, ms) = civil_from_millis(days as u128 * 86_400_000);
            assert_eq!((yy, mm, dd), (y, m, d), "{y}-{m}-{d}");
            assert_eq!((hh, mi, ss, ms), (0, 0, 0, 0));
        }
    }

    #[test]
    fn unknown_token_left_verbatim() {
        let mut g = Gen::new(1);
        assert_eq!(g.expand("${bogus}").unwrap(), "${bogus}");
        assert_eq!(g.expand("a ${nope(1)} b").unwrap(), "a ${nope(1)} b");
    }

    #[test]
    fn double_brace_engine_placeholders_are_untouched() {
        let mut g = Gen::new(1);
        // `${{ … }}` is the engine's job; the generator must not eat it.
        assert_eq!(g.expand("${{ config.x }}").unwrap(), "${{ config.x }}");
    }

    #[test]
    fn mixed_literal_and_tokens() {
        let mut g = Gen::new(5);
        g.begin_message();
        let s = g.expand("ORD-${seq}-${rand(1,1)}").unwrap();
        assert_eq!(s, "ORD-1-1");
    }

    // -----------------------------------------------------------------
    // Library tokens (RFC 005)
    // -----------------------------------------------------------------

    fn gen_with_random(seed: u64) -> Gen {
        let mut g = Gen::new(seed);
        let provider = crate::library::builtin_provider("@std/random@v1").unwrap();
        g.attach_library("random", provider.instantiate(None, seed).unwrap());
        g
    }

    #[test]
    fn library_token_resolves_through_the_alias() {
        let mut g = gen_with_random(11);
        g.begin_message();
        let v = g.expand("${random.int(10,20)}").unwrap();
        let n: i64 = v.parse().unwrap();
        assert!((10..=20).contains(&n), "{v}");
        // Alias composes with literals and built-ins in one template.
        let s = g.expand("id-${seq}-${random.pick(a|b)}").unwrap();
        assert!(s == "id-1-a" || s == "id-1-b", "{s}");
    }

    #[test]
    fn unknown_alias_stays_verbatim() {
        let mut g = gen_with_random(11);
        assert_eq!(g.expand("${faker.email()}").unwrap(), "${faker.email()}");
        assert_eq!(
            g.expand("${randomish.int(1,2)}").unwrap(),
            "${randomish.int(1,2)}"
        );
    }

    #[test]
    fn known_alias_unknown_function_is_a_hard_error() {
        let mut g = gen_with_random(11);
        let err = g.expand("${random.uuid9()}").unwrap_err();
        assert!(err.contains("unknown function"), "{err}");
        let err = g.expand("${random.int(nope)}").unwrap_err();
        assert!(err.contains("random.int"), "{err}");
    }

    #[test]
    fn memo_key_repeats_within_one_message() {
        let mut g = gen_with_random(11);
        g.begin_message();
        let a = g.expand("${random.uuid4(order)}").unwrap();
        assert_eq!(
            g.expand("${random.uuid4(order)}").unwrap(),
            a,
            "same message, same key"
        );
        assert_ne!(g.expand("${random.uuid4()}").unwrap(), a, "no key → fresh");
        g.begin_message();
        assert_ne!(
            g.expand("${random.uuid4(order)}").unwrap(),
            a,
            "next message → fresh"
        );
    }

    #[test]
    fn args_mapping_contract() {
        // Numbers/bools parse as JSON; quoted strings lose one quote pair;
        // pipes survive as one string; commas inside quotes don't split.
        let (name, args) = parse_library_call("int(1, 100)").unwrap();
        assert_eq!(name, "int");
        assert_eq!(args, vec![serde_json::json!(1), serde_json::json!(100)]);

        let (_, args) = parse_library_call("pick(a|b|c)").unwrap();
        assert_eq!(args, vec![serde_json::json!("a|b|c")]);

        let (_, args) = parse_library_call("pattern(\"ORD-####-????\")").unwrap();
        assert_eq!(args, vec![serde_json::json!("ORD-####-????")]);

        let (_, args) = parse_library_call("f(\"a,b\", true, [1], {\"k\": 1})").unwrap();
        assert_eq!(
            args,
            vec![
                serde_json::json!("a,b"),
                serde_json::json!(true),
                serde_json::json!([1]),
                serde_json::json!({ "k": 1 }),
            ]
        );
        // Only double quotes shield commas — `[1,2]` is two arguments
        // ("[1" and "2]"), by contract.
        let (_, args) = parse_library_call("f([1,2])").unwrap();
        assert_eq!(args.len(), 2);

        let (name, args) = parse_library_call("uuid4()").unwrap();
        assert_eq!(name, "uuid4");
        assert!(args.is_empty());
    }

    #[test]
    fn library_calls_feed_the_shared_metrics_recorder() {
        use crate::library::{CallCtx, LibraryInstance};

        struct Stub {
            fail: bool,
        }
        impl LibraryInstance for Stub {
            fn call(
                &mut self,
                _ctx: &CallCtx,
                func: &str,
                _args: &[serde_json::Value],
            ) -> Result<String, String> {
                if self.fail || func == "boom" {
                    Err("stub failure".into())
                } else {
                    Ok("ok".into())
                }
            }
        }

        let recorder = std::sync::Arc::new(crate::library::LibraryMetrics::new([
            "stub".to_string(),
            "quiet".to_string(),
        ]));
        let mut g = Gen::new(1).with_library_metrics(std::sync::Arc::clone(&recorder));
        g.attach_library("stub", Box::new(Stub { fail: false }));
        g.begin_message();
        g.expand("${stub.fn(1)}").unwrap();
        g.expand("${stub.fn(2)}").unwrap();
        assert!(g.expand("${stub.boom()}").is_err());

        let summary = recorder.summary().unwrap();
        let stub = &summary["stub"];
        assert_eq!(stub.calls, 3);
        assert_eq!(stub.errors, 1);
        assert!(stub.p50_ms > 0.0);
        assert!(stub.max_ms >= stub.p95_ms);
        assert!(!summary.contains_key("quiet"), "no calls → no entry");
    }

    #[test]
    fn library_sequence_is_deterministic_per_seed() {
        let mut a = gen_with_random(77);
        let mut b = gen_with_random(77);
        // (ulid/uuid7 carry wall-clock time and are covered with a fixed
        // CallCtx in the std_random tests instead.)
        for token in [
            "${random.nanoid()}",
            "${random.first_name()}",
            "${random.int(1,1000000)}",
        ] {
            assert_eq!(
                a.expand(token).unwrap(),
                b.expand(token).unwrap(),
                "{token}"
            );
        }
    }
}
