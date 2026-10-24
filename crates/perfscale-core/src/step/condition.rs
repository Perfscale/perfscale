//! Step `if:` conditions.
//!
//! A condition string expands `${{ … }}` placeholders and `${…}` generator
//! tokens first, then evaluates as either one comparison (`a == b`, `!=`,
//! `<`, `<=`, `>`, `>=` — numeric when both sides parse as numbers, string
//! otherwise) or a bare value, which is false when empty, `false`, `0` or
//! `null`. Clauses combine with `&&` and `||` (`&&` binds tighter, no
//! parentheses).
//!
//! ```yaml
//! - use: std/sleep@v1
//!   if: "${{ __iter__ }} == 0"           # first iteration only
//!   with: { ms: "${rand(0,5000)}" }
//! - use: std/http@v1
//!   if: "${randf(0,1,4)} < 0.15"         # 15% of iterations
//!   with: { method: POST, url: "…/like" }
//! ```

use super::context::Context;

/// Operators in match order: two-character ones first, so `<=` is never
/// read as `<` followed by `=b`.
const OPERATORS: [&str; 6] = ["==", "!=", "<=", ">=", "<", ">"];

/// Evaluate a step condition against the VU's context.
pub(crate) fn evaluate(condition: &str, ctx: &Context) -> Result<bool, String> {
    let interpolated = ctx.try_interpolate(condition)?;
    let expanded = ctx.token_expander().expand(&interpolated)?;
    Ok(evaluate_expanded(&expanded))
}

fn evaluate_expanded(s: &str) -> bool {
    s.split("||")
        .any(|any| any.split("&&").all(evaluate_clause))
}

fn evaluate_clause(s: &str) -> bool {
    for op in OPERATORS {
        if let Some((lhs, rhs)) = s.split_once(op) {
            return compare(lhs.trim(), op, rhs.trim());
        }
    }
    truthy(s.trim())
}

fn compare(lhs: &str, op: &str, rhs: &str) -> bool {
    if let (Ok(a), Ok(b)) = (lhs.parse::<f64>(), rhs.parse::<f64>()) {
        return match op {
            "==" => a == b,
            "!=" => a != b,
            "<=" => a <= b,
            ">=" => a >= b,
            "<" => a < b,
            _ => a > b,
        };
    }
    let (a, b) = (unquote(lhs), unquote(rhs));
    match op {
        "==" => a == b,
        "!=" => a != b,
        "<=" => a <= b,
        ">=" => a >= b,
        "<" => a < b,
        _ => a > b,
    }
}

/// `'ok'` / `"ok"` compare as `ok`, so a literal can be quoted for clarity.
fn unquote(s: &str) -> &str {
    let quoted = s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')));
    if quoted {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn truthy(s: &str) -> bool {
    !matches!(s, "" | "false" | "0" | "null")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numeric_comparisons() {
        assert!(evaluate_expanded("200 == 200"));
        assert!(evaluate_expanded("200 != 404"));
        assert!(evaluate_expanded("0.1 < 0.15"));
        assert!(!evaluate_expanded("0.2 < 0.15"));
        assert!(evaluate_expanded("5 >= 5"));
        assert!(evaluate_expanded("4 <= 5"));
        assert!(evaluate_expanded("10 > 9"));
        // Numeric, not lexicographic: "10" < "9" as strings.
        assert!(!evaluate_expanded("10 < 9"));
    }

    #[test]
    fn string_comparisons_and_quotes() {
        assert!(evaluate_expanded("ok == 'ok'"));
        assert!(evaluate_expanded("\"a\" != b"));
        assert!(!evaluate_expanded(" == x"));
    }

    #[test]
    fn and_binds_tighter_than_or() {
        assert!(evaluate_expanded("200 == 200 && 0.1 < 0.15"));
        assert!(!evaluate_expanded("404 == 200 && 0.1 < 0.15"));
        assert!(evaluate_expanded("404 == 200 || 1 == 1"));
        // a || b && c  ==  a || (b && c)
        assert!(evaluate_expanded("1 == 1 || 0 == 1 && 0 == 1"));
        assert!(!evaluate_expanded("0 == 1 || 1 == 1 && 0 == 1"));
    }

    #[test]
    fn bare_values() {
        assert!(evaluate_expanded("yes"));
        assert!(evaluate_expanded("1"));
        for falsy in ["", "  ", "false", "0", "null"] {
            assert!(!evaluate_expanded(falsy), "{falsy:?} should be false");
        }
    }

    #[test]
    fn evaluates_placeholders_vu_identity_and_tokens() {
        let mut ctx = Context::new();
        ctx.set("feed", json!({ "status": 200 }));
        ctx.set_vu_id(3);
        ctx.iteration_seq = 1;
        assert!(evaluate("${{ feed.status }} == 200", &ctx).unwrap());
        assert!(evaluate("${{ __vu__ }} == 3", &ctx).unwrap());
        assert!(evaluate("${{ __iter__ }} == 0", &ctx).unwrap());
        assert!(evaluate("${rand(1,1)} == 1", &ctx).unwrap());
        assert!(!evaluate("${{ missing.status }} == 200", &ctx).unwrap());
    }

    #[test]
    fn missing_env_var_is_an_error() {
        let ctx = Context::new();
        assert!(evaluate("${{ env.PERFSCALE_TEST_SURELY_UNSET }}", &ctx).is_err());
    }
}
