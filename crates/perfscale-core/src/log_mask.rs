//! Run-log masking for resolved `${{ env.NAME }}` values.
//!
//! Native-engine test YAML pulls secrets (API keys, tokens, DSNs) from the
//! process environment via `${{ env.NAME }}` placeholders. The documented
//! contract is that such values never appear in logs — but after
//! interpolation they are plain strings, and steps would happily print them
//! (`std/log@v1` messages, `std/check@v1` expectations, request URLs with
//! credentials, managed child-process output, ...).
//!
//! [`SecretRegistry`] closes that hole: `Context::resolve_expr` records every
//! successfully resolved env value into one run-scoped registry, and every
//! line the native engine sends to the run log passes through
//! [`SecretRegistry::mask`] first (see `step::runner::emit` and the
//! managed-process output mirror in `step::process`).
//!
//! The matching rules mirror the platform's masker:
//!
//! - empty / whitespace-only values are never recorded, so they are never
//!   masked;
//! - values of 4+ bytes are plain substring-replaced, which also covers a
//!   secret embedded in a larger string such as a DSN URL
//!   (`postgres://user:s3cret@host/db`);
//! - shorter values are replaced only as whole tokens — bounded on both
//!   sides by a non-alphanumeric character or a string edge — so a short
//!   secret cannot redact its way through unrelated words (`xabc` survives
//!   when the secret is `abc`);
//! - longer values are applied first, so overlapping secrets mask the longer
//!   one.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// Replacement text for every masked value.
const MASK: &str = "***";
/// Values at least this long match anywhere in a line; shorter ones match
/// only as whole tokens (see the module docs).
const SUBSTRING_MIN_LEN: usize = 4;

/// Run-scoped registry of resolved env values that must stay out of the run
/// log.
///
/// Cheap to clone (an `Arc` handle): every `Context` of a run shares one
/// registry, so the log pipeline masks against the union of everything any
/// VU resolved so far. Thread-safe — VUs interpolate concurrently.
#[derive(Debug, Clone, Default)]
pub struct SecretRegistry {
    values: Arc<RwLock<HashSet<String>>>,
}

impl SecretRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember a resolved env value for masking. Empty and whitespace-only
    /// values are skipped: masking them would mangle every line (or be a
    /// no-op), and they carry no secret anyway.
    pub fn record(&self, value: &str) {
        if value.trim().is_empty() {
            return;
        }
        self.values.write().unwrap().insert(value.to_string());
    }

    /// Replace every recorded value in `line` with `***`.
    ///
    /// Borrows the line untouched when nothing matched — and bails out
    /// before any scanning when the registry is empty (runs without
    /// `${{ env.* }}` placeholders pay nothing).
    pub fn mask<'a>(&self, line: &'a str) -> Cow<'a, str> {
        let values = self.values.read().unwrap();
        if values.is_empty() {
            return Cow::Borrowed(line);
        }
        // Longest first: when two recorded values overlap, the longer one
        // must win — masking the shorter first would leave the tail of the
        // longer one behind in plain text.
        let mut values: Vec<&str> = values.iter().map(String::as_str).collect();
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));

        let mut out = Cow::Borrowed(line);
        for value in values {
            if value.len() >= SUBSTRING_MIN_LEN {
                if out.contains(value) {
                    out = Cow::Owned(out.replace(value, MASK));
                }
            } else if let Some(masked) = mask_whole_token(&out, value) {
                out = Cow::Owned(masked);
            }
        }
        out
    }
}

/// Replace every whole-token occurrence of `value` in `line`: an occurrence
/// counts only when both sides are bounded by a non-alphanumeric character
/// or a string edge (checked against the original line, so a rejected
/// occurrence never becomes a "boundary" for an overlapping one). Returns
/// `None` when no occurrence qualifies, so the caller can keep borrowing
/// the original line.
fn mask_whole_token(line: &str, value: &str) -> Option<String> {
    let mut out = String::new();
    let mut matched = false;
    let mut copied = 0; // bytes of `line` already flushed into `out`
    let mut scan = 0; // where the next search starts
    while let Some(at) = line[scan..].find(value) {
        let start = scan + at;
        let end = start + value.len();
        let left_ok = line[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let right_ok = line[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if left_ok && right_ok {
            out.push_str(&line[copied..start]);
            out.push_str(MASK);
            copied = end;
            matched = true;
        }
        scan = end;
    }
    if !matched {
        return None;
    }
    out.push_str(&line[copied..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_is_masked() {
        let reg = SecretRegistry::new();
        reg.record("s3cret-token");
        assert_eq!(reg.mask("token is s3cret-token ok"), "token is *** ok");
        // The whole line being the secret masks down to just the marker.
        assert_eq!(reg.mask("s3cret-token"), MASK);
    }

    #[test]
    fn empty_and_whitespace_values_are_never_recorded() {
        let reg = SecretRegistry::new();
        reg.record("");
        reg.record("   ");
        reg.record("\t\n ");
        // Nothing was recorded, so the registry is still in its cheap
        // bail-out state and every line borrows untouched.
        assert!(matches!(reg.mask("a b  c"), Cow::Borrowed("a b  c")));
    }

    #[test]
    fn short_values_mask_only_as_whole_tokens() {
        let reg = SecretRegistry::new();
        reg.record("abc"); // 3 bytes — below the substring threshold
                           // Bounded by string edges and non-alphanumeric characters.
        assert_eq!(reg.mask("abc"), MASK);
        assert_eq!(reg.mask(" abc "), " *** ");
        assert_eq!(reg.mask("(abc)"), "(***)");
        assert_eq!(reg.mask("abc,abc"), "***,***");
        // Inside a larger word it must survive — both sides alphanumeric.
        assert!(matches!(reg.mask("xabc"), Cow::Borrowed("xabc")));
        assert!(matches!(reg.mask("abcx"), Cow::Borrowed("abcx")));
        assert!(matches!(reg.mask("xabcx"), Cow::Borrowed("xabcx")));
        // Mixed: only the bounded occurrence masks.
        assert_eq!(reg.mask("xabc abc"), "xabc ***");
    }

    #[test]
    fn multiple_values_in_one_line_all_mask() {
        let reg = SecretRegistry::new();
        reg.record("alpha-secret");
        reg.record("beta-secret");
        assert_eq!(reg.mask("alpha-secret and beta-secret"), "*** and ***");
    }

    #[test]
    fn secret_inside_dsn_url_is_masked() {
        let reg = SecretRegistry::new();
        reg.record("p4ssw0rd");
        assert_eq!(
            reg.mask("connecting to postgres://user:p4ssw0rd@db.internal:5432/app"),
            "connecting to postgres://user:***@db.internal:5432/app"
        );
    }

    #[test]
    fn repeated_occurrences_all_mask() {
        let reg = SecretRegistry::new();
        reg.record("tok123");
        assert_eq!(reg.mask("tok123 tok123 tok123"), "*** *** ***");
    }

    #[test]
    fn no_match_borrows_the_line() {
        let reg = SecretRegistry::new();
        reg.record("recorded-secret");
        assert!(matches!(
            reg.mask("nothing here"),
            Cow::Borrowed("nothing here")
        ));
    }

    #[test]
    fn empty_registry_borrows_without_scanning() {
        let reg = SecretRegistry::new();
        assert!(matches!(
            reg.mask("anything at all"),
            Cow::Borrowed("anything at all")
        ));
    }

    #[test]
    fn overlapping_secrets_mask_the_longer_one() {
        let reg = SecretRegistry::new();
        reg.record("secret");
        reg.record("secret-key");
        // Longest-first: masking `secret` first would leak `-key`.
        assert_eq!(reg.mask("the secret-key here"), "the *** here");
        // The shorter one still masks when it stands alone.
        assert_eq!(reg.mask("the secret here"), "the *** here");
    }

    #[test]
    fn cloned_registries_share_the_same_values() {
        let reg = SecretRegistry::new();
        let clone = reg.clone();
        reg.record("shared-secret");
        assert_eq!(clone.mask("a shared-secret b"), "a *** b");
    }

    #[test]
    fn multibyte_neighbours_do_not_confuse_token_boundaries() {
        let reg = SecretRegistry::new();
        reg.record("ab");
        // `ä` is alphanumeric, so the occurrence inside `äab` must survive;
        // the one after the space is a whole token.
        assert_eq!(reg.mask("äab ab"), "äab ***");
    }

    #[test]
    fn overlapping_occurrences_are_not_whole_tokens() {
        let reg = SecretRegistry::new();
        reg.record("aa");
        // Every occurrence sits inside a run of `a`s — none is bounded, so
        // nothing masks (a rejected occurrence must not become the boundary
        // for the next one).
        assert!(matches!(reg.mask("aaaa"), Cow::Borrowed("aaaa")));
        assert!(matches!(reg.mask("aaa"), Cow::Borrowed("aaa")));
        // But a genuinely bounded occurrence masks.
        assert_eq!(reg.mask("aa aa"), "*** ***");
    }
}
