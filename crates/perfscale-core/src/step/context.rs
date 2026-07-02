//! Per-VU execution context — variable store and `${{ ... }}` interpolation.

use std::collections::HashMap;

use serde_json::Value;

/// Execution context for a single VU iteration.
///
/// Stores step outputs so later steps can reference them via
/// `${{ var_name.field }}` in string parameter values.
#[derive(Debug, Default, Clone)]
pub struct Context {
    pub(crate) vars: HashMap<String, Value>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a step's output under `name`.
    pub fn set(&mut self, name: &str, value: Value) {
        self.vars.insert(name.to_string(), value);
    }

    /// Interpolate `${{ expr }}` placeholders in a string.
    ///
    /// Supported forms:
    /// - `${{ name }}`        → whole stored value as string
    /// - `${{ name.field }}`  → field of a stored JSON object
    pub fn interpolate(&self, s: &str) -> String {
        let mut result = s.to_string();
        let mut offset = 0usize;

        loop {
            let search = &result[offset..];
            let Some(start) = search.find("${{") else {
                break;
            };
            let abs_start = offset + start;
            let after = &result[abs_start + 3..];
            let Some(end_rel) = after.find("}}") else {
                break;
            };

            let expr = after[..end_rel].trim().to_string();
            let value = self.resolve_expr(&expr);
            let full_len = 3 + end_rel + 2; // "${{".len() + content + "}}".len()
            result.replace_range(abs_start..abs_start + full_len, &value);
            offset = abs_start + value.len();
        }
        result
    }

    /// Resolve an expression like `"resp.status"` or `"resp"`.
    fn resolve_expr(&self, expr: &str) -> String {
        let parts: Vec<&str> = expr.splitn(2, '.').collect();
        match (self.vars.get(parts[0]), parts.get(1)) {
            (Some(v), Some(field)) => v.get(*field).map(value_to_string).unwrap_or_default(),
            (Some(v), None) => value_to_string(v),
            _ => String::new(),
        }
    }

    /// Apply interpolation to every string leaf of a JSON `Value`.
    pub fn interpolate_value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.interpolate(s)),
            Value::Object(m) => {
                let mut out = serde_json::Map::new();
                for (k, val) in m {
                    out.insert(k.clone(), self.interpolate_value(val));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| self.interpolate_value(x)).collect()),
            other => other.clone(),
        }
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
}
