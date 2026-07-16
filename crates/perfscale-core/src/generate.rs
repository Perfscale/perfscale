//! Per-send dynamic value generation for protocol messages.
//!
//! Message payloads may embed **single-brace** tokens that are expanded anew
//! each time a message is sent — so a repeated order gets a fresh id, a live
//! timestamp, a random price, and so on. Single-brace `${…}` is deliberately
//! distinct from the engine's `${{ … }}` interpolation, which is resolved once
//! before the action runs and left untouched here.
//!
//! Used by `std/ws@v1`/`std/ws-send@v1` for streaming unique messages from one
//! template; the proprietary FIX actions share the same expander so `${…}`
//! means exactly one thing across all protocols.
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

/// Per-session generator state: a message-send counter plus a small PRNG.
///
/// Not cryptographic — an xorshift64 seeded per session is plenty for load
/// data (unique ids, varied prices) and avoids a dependency.
pub struct Gen {
    seq: u64,
    rng: u64,
}

impl Gen {
    /// New generator. `seed` is forced non-zero (xorshift degenerates at 0).
    pub fn new(seed: u64) -> Self {
        Gen {
            seq: 0,
            rng: seed | 1,
        }
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
    pub fn expand(&mut self, template: &str) -> String {
        if !template.contains("${") {
            return template.to_string();
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
                    out.push_str(&self.eval(token).unwrap_or_else(|| format!("${{{token}}}")));
                    i = i + 2 + close + 1;
                    continue;
                }
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    /// Evaluate one token's inner text (without the `${` `}`). `None` → unknown
    /// token, left verbatim by the caller.
    fn eval(&mut self, token: &str) -> Option<String> {
        match token {
            "seq" => Some(self.seq.to_string()),
            "uuid" => Some(format!("{:016x}{:016x}", self.next_u64(), self.next_u64())),
            "now" => Some(now_fix()),
            "now_ms" => Some(now_unix_millis().to_string()),
            "now_iso" => Some(now_iso()),
            _ => {
                let (name, args) = parse_call(token)?;
                match name {
                    "rand" => {
                        let (a, b) = two_ints(&args)?;
                        Some(self.rand_range(a, b).to_string())
                    }
                    "randf" => {
                        let a: f64 = args.first()?.trim().parse().ok()?;
                        let b: f64 = args.get(1)?.trim().parse().ok()?;
                        let dp: usize =
                            args.get(2).and_then(|s| s.trim().parse().ok()).unwrap_or(2);
                        // Scale to integer units at `dp` precision, pick, rescale.
                        let scale = 10f64.powi(dp as i32);
                        let lo = (a * scale) as i64;
                        let hi = (b * scale) as i64;
                        let v = self.rand_range(lo, hi) as f64 / scale;
                        Some(format!("{v:.*}", dp))
                    }
                    "choice" => {
                        let opts: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                        if opts.is_empty() {
                            return None;
                        }
                        let idx = (self.next_u64() % opts.len() as u64) as usize;
                        Some(opts[idx].trim().to_string())
                    }
                    _ => None,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wall-clock formatting (no chrono dependency)
// ---------------------------------------------------------------------------

fn now_unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Split unix milliseconds into (y, m, d, hh, mm, ss, ms) in UTC using the
/// days-from-civil inverse (Howard Hinnant's algorithm) — exact for the whole
/// Gregorian range, no leap-second handling (unix time has none).
fn civil_from_millis(ms: u128) -> (i64, u32, u32, u32, u32, u32, u32) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_string_passes_through() {
        let mut g = Gen::new(1);
        assert_eq!(g.expand("EURUSD"), "EURUSD");
    }

    #[test]
    fn seq_is_shared_within_a_message_and_bumps_per_message() {
        let mut g = Gen::new(1);
        g.begin_message();
        assert_eq!(g.expand("order-${seq}"), "order-1");
        assert_eq!(g.expand("dup-${seq}"), "dup-1"); // same message → same seq
        g.begin_message();
        assert_eq!(g.expand("order-${seq}"), "order-2");
    }

    #[test]
    fn rand_stays_in_range() {
        let mut g = Gen::new(42);
        for _ in 0..1000 {
            let v: i64 = g.expand("${rand(10,20)}").parse().unwrap();
            assert!((10..=20).contains(&v), "out of range: {v}");
        }
    }

    #[test]
    fn randf_respects_bounds_and_decimals() {
        let mut g = Gen::new(7);
        for _ in 0..500 {
            let s = g.expand("${randf(1.0,2.0,3)}");
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
            let v = g.expand("${choice(1|2)}");
            assert!(v == "1" || v == "2", "unexpected: {v}");
        }
    }

    #[test]
    fn uuid_is_hex_and_varies() {
        let mut g = Gen::new(9);
        let a = g.expand("${uuid}");
        let b = g.expand("${uuid}");
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn now_has_fix_timestamp_shape() {
        let mut g = Gen::new(1);
        let ts = g.expand("${now}");
        // YYYYMMDD-HH:MM:SS.sss
        assert_eq!(ts.len(), 21, "{ts}");
        assert_eq!(&ts[8..9], "-");
    }

    #[test]
    fn now_ms_is_plausible_unix_millis() {
        let mut g = Gen::new(1);
        let v: u128 = g.expand("${now_ms}").parse().unwrap();
        // After 2020-01-01 and before 2100-01-01.
        assert!(v > 1_577_836_800_000 && v < 4_102_444_800_000, "{v}");
    }

    #[test]
    fn now_iso_has_rfc3339_shape() {
        let mut g = Gen::new(1);
        let ts = g.expand("${now_iso}");
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
    fn unknown_token_left_verbatim() {
        let mut g = Gen::new(1);
        assert_eq!(g.expand("${bogus}"), "${bogus}");
        assert_eq!(g.expand("a ${nope(1)} b"), "a ${nope(1)} b");
    }

    #[test]
    fn double_brace_engine_placeholders_are_untouched() {
        let mut g = Gen::new(1);
        // `${{ … }}` is the engine's job; the generator must not eat it.
        assert_eq!(g.expand("${{ config.x }}"), "${{ config.x }}");
    }

    #[test]
    fn mixed_literal_and_tokens() {
        let mut g = Gen::new(5);
        g.begin_message();
        let s = g.expand("ORD-${seq}-${rand(1,1)}");
        assert_eq!(s, "ORD-1-1");
    }
}
