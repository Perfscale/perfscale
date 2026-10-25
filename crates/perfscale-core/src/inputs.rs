//! Typed run inputs (issue #11) — the `inputs:` block of a config document.
//!
//! A config declares typed inputs, the test/config references them via
//! `${{ inputs.<name> }}` interpolation (values are resolved once, before the
//! run starts, and fixed for the whole run), and the CLI overrides them at
//! launch time:
//!
//! ```yaml
//! inputs:
//!   users: number            # short form: required, no default
//!   host: { type: string, default: localhost }
//!   verbose: { type: bool, default: false }   # coercion: 0/1/false/true
//!   env: { type: choice, options: [staging, prod], default: staging }
//!   vus_max: { type: number, min: 1, max: 10000 }
//! ```
//!
//! Precedence (highest wins): `-i key=value` CLI override > `-e file.env`
//! env file > `default:` from the declaration. A declared input with no
//! default is *required* — no value from any source is a hard error before
//! the run starts (lint/validate and `run` both refuse).
//!
//! Custom input types registered through the library SDK are issue #10
//! follow-up work and are not part of this contract.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The declared type of one input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    /// A number; `min`/`max` bounds apply.
    Number,
    /// A free-form string.
    String,
    /// A boolean. String values coerce: `0`/`false` → false, `1`/`true` →
    /// true; anything else is an error.
    Bool,
    /// One of a fixed option list (GitHub Actions style `choice`).
    Choice,
}

/// One declared input (`inputs.<name>`). Short form (`users: number`) is
/// just the type; the long form adds `default`, `min`/`max` (number only)
/// and `options` (choice only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum InputDecl {
    /// Bare type name: `users: number`.
    Short(InputType),
    /// Full declaration.
    Full(InputSpec),
}

/// The long form of an input declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputSpec {
    #[serde(rename = "type")]
    pub type_: InputType,
    /// Default value; when absent the input is required (a value must come
    /// from `-i` or the env file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Lower bound (number only), inclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// Upper bound (number only), inclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// The allowed values (choice only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

impl InputDecl {
    fn spec(&self) -> InputSpec {
        match self {
            InputDecl::Short(t) => InputSpec {
                type_: *t,
                default: None,
                min: None,
                max: None,
                options: None,
            },
            InputDecl::Full(s) => s.clone(),
        }
    }
}

/// Fail-fast validation of the declarations themselves: `options` on the
/// right type, `min <= max`, and every `default` coercible and in range.
/// Run at lint and before the run starts.
pub fn validate_inputs(decls: &BTreeMap<String, InputDecl>) -> Result<(), String> {
    for (name, decl) in decls {
        let spec = decl.spec();
        if spec.min.is_some() || spec.max.is_some() {
            if spec.type_ != InputType::Number {
                return Err(format!(
                    "input '{name}': min/max apply to `number` inputs only"
                ));
            }
            if let (Some(min), Some(max)) = (spec.min, spec.max) {
                if min > max {
                    return Err(format!("input '{name}': min ({min}) is above max ({max})"));
                }
            }
        }
        match spec.type_ {
            InputType::Choice => {
                let empty = spec.options.as_deref().unwrap_or(&[]).is_empty();
                if empty {
                    return Err(format!(
                        "input '{name}': a `choice` input needs a non-empty `options:` list"
                    ));
                }
            }
            _ => {
                if spec.options.is_some() {
                    return Err(format!(
                        "input '{name}': options apply to `choice` inputs only"
                    ));
                }
            }
        }
        if let Some(default) = &spec.default {
            let raw = value_to_raw(default);
            coerce(name, &spec, &raw)
                .map_err(|e| format!("input '{name}': invalid default — {e}"))?;
        }
    }
    Ok(())
}

/// Resolve the effective input values for a run: `cli` overrides (`-i`,
/// highest precedence), then env-file values (`-e`, only keys that name a
/// declared input are consumed — an env file may carry unrelated
/// variables), then declaration defaults. Unknown CLI keys and missing
/// required inputs are hard errors.
pub fn resolve_inputs(
    decls: &BTreeMap<String, InputDecl>,
    cli: &[(String, String)],
    env_file: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    for (key, _) in cli {
        if !decls.contains_key(key) {
            return Err(format!(
                "unknown input '{key}' — declared inputs: {}",
                decls.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    let mut out = Map::new();
    for (name, decl) in decls {
        let spec = decl.spec();
        let raw = cli
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| {
                env_file
                    .get(name)
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| spec.default.as_ref().map(value_to_raw));
        let value = match raw {
            Some(raw) => coerce(name, &spec, &raw)?,
            None => {
                return Err(format!(
                    "input '{name}' is required (no default, no -i {name}=… override, not in the env file)"
                ))
            }
        };
        out.insert(name.clone(), value);
    }
    Ok(out)
}

/// A JSON default rendered as the raw string `coerce` expects.
fn value_to_raw(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Coerce a raw (stringly) value into the declared type, with bounds.
fn coerce(name: &str, spec: &InputSpec, raw: &str) -> Result<Value, String> {
    match spec.type_ {
        InputType::String => Ok(Value::String(raw.to_string())),
        InputType::Number => {
            let n: f64 = raw
                .trim()
                .parse()
                .map_err(|_| format!("input '{name}': '{raw}' is not a number (type: number)"))?;
            if let Some(min) = spec.min {
                if n < min {
                    return Err(format!("input '{name}': {n} is below min {min}"));
                }
            }
            if let Some(max) = spec.max {
                if n > max {
                    return Err(format!("input '{name}': {n} exceeds max {max}"));
                }
            }
            // Integral values stay integers — `users=10` interpolates as
            // "10", not "10.0".
            if n.fract() == 0.0 && n.abs() < 9.0e15 {
                return Ok(Value::from(n as i64));
            }
            Ok(serde_json::json!(n))
        }
        InputType::Bool => match raw {
            "0" | "false" => Ok(Value::Bool(false)),
            "1" | "true" => Ok(Value::Bool(true)),
            _ => Err(format!(
                "input '{name}': '{raw}' is not a bool (accepted: 0, 1, false, true)"
            )),
        },
        InputType::Choice => {
            let options = spec.options.as_deref().unwrap_or(&[]);
            if options.iter().any(|o| o == raw) {
                Ok(Value::String(raw.to_string()))
            } else {
                Err(format!(
                    "input '{name}': '{raw}' is not one of [{}]",
                    options.join(", ")
                ))
            }
        }
    }
}

/// The resolved values as a single JSON object for the `${{ inputs.* }}`
/// namespace (null when the run declares no inputs).
pub fn inputs_namespace(resolved: &Map<String, Value>) -> Value {
    if resolved.is_empty() {
        Value::Null
    } else {
        Value::Object(resolved.clone())
    }
}

/// Parse an env file (`-e file.env`): one `KEY=VALUE` per line, blank lines
/// and `#` comments skipped, whitespace around the key trimmed. Values are
/// verbatim (no quote processing) — they are input values, not shell.
pub fn parse_env_file(text: &str) -> Result<Map<String, Value>, String> {
    let mut out = Map::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "env file line {}: expected KEY=VALUE, got '{line}'",
                i + 1
            ));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("env file line {}: empty key", i + 1));
        }
        out.insert(key.to_string(), Value::String(value.to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decls() -> BTreeMap<String, InputDecl> {
        serde_json::from_value(json!({
            "users": "number",
            "host": { "type": "string", "default": "localhost" },
            "verbose": { "type": "bool", "default": false },
            "env": { "type": "choice", "options": ["staging", "prod"], "default": "staging" },
            "pacing_ms": { "type": "number", "min": 1, "max": 10000 },
        }))
        .unwrap()
    }

    #[test]
    fn short_and_long_forms_parse() {
        let d = decls();
        assert!(matches!(d["users"], InputDecl::Short(InputType::Number)));
        assert!(matches!(d["host"], InputDecl::Full(_)));
    }

    #[test]
    fn declaration_validation_catches_bad_specs() {
        let bad_choice: BTreeMap<String, InputDecl> =
            serde_json::from_value(json!({ "env": { "type": "choice" } })).unwrap();
        let err = validate_inputs(&bad_choice).unwrap_err();
        assert!(err.contains("options"), "{err}");

        let bad_bounds: BTreeMap<String, InputDecl> =
            serde_json::from_value(json!({ "n": { "type": "number", "min": 5, "max": 1 } }))
                .unwrap();
        assert!(validate_inputs(&bad_bounds).unwrap_err().contains("min"));

        let misplaced: BTreeMap<String, InputDecl> =
            serde_json::from_value(json!({ "s": { "type": "string", "min": 1 } })).unwrap();
        assert!(validate_inputs(&misplaced).unwrap_err().contains("number"));

        let bad_default: BTreeMap<String, InputDecl> =
            serde_json::from_value(json!({ "n": { "type": "number", "max": 3, "default": 9 } }))
                .unwrap();
        assert!(validate_inputs(&bad_default)
            .unwrap_err()
            .contains("invalid default"));
    }

    #[test]
    fn resolve_applies_precedence_cli_over_env_over_default() {
        let d = decls();
        let env = Map::from_iter([
            ("host".to_string(), json!("env-host")),
            ("users".to_string(), json!("20")),
            ("pacing_ms".to_string(), json!("250")),
        ]);
        let cli = vec![("users".to_string(), "10".to_string())];
        let out = resolve_inputs(&d, &cli, &env).unwrap();
        assert_eq!(out["users"], json!(10), "CLI beats the env file");
        assert_eq!(out["host"], json!("env-host"), "env file beats default");
        assert_eq!(out["verbose"], json!(false), "default fills the rest");
        assert_eq!(out["env"], json!("staging"));
        assert_eq!(out["pacing_ms"], json!(250));
    }

    #[test]
    fn resolve_coerces_bool_and_validates_choice() {
        let d = decls();
        let env = Map::new();
        let cli = vec![
            ("users".to_string(), "3".to_string()),
            ("verbose".to_string(), "1".to_string()),
            ("env".to_string(), "prod".to_string()),
            ("pacing_ms".to_string(), "100".to_string()),
        ];
        let out = resolve_inputs(&d, &cli, &env).unwrap();
        assert_eq!(out["verbose"], json!(true));

        for bad in ["yes", "2", "TRUE", ""] {
            let cli = vec![
                ("users".to_string(), "3".to_string()),
                ("verbose".to_string(), bad.to_string()),
                ("pacing_ms".to_string(), "5".to_string()),
            ];
            let err = resolve_inputs(&d, &cli, &env).unwrap_err();
            assert!(err.contains("not a bool"), "{err}");
        }

        let cli = vec![
            ("users".to_string(), "3".to_string()),
            ("env".to_string(), "qa".to_string()),
            ("pacing_ms".to_string(), "5".to_string()),
        ];
        let err = resolve_inputs(&d, &cli, &env).unwrap_err();
        assert!(err.contains("not one of [staging, prod]"), "{err}");
    }

    #[test]
    fn resolve_enforces_bounds_unknown_keys_and_required() {
        let d = decls();
        let env = Map::new();
        let cli = vec![
            ("users".to_string(), "1".to_string()),
            ("pacing_ms".to_string(), "20000".to_string()),
        ];
        let err = resolve_inputs(&d, &cli, &env).unwrap_err();
        assert!(err.contains("exceeds max"), "{err}");

        let cli = vec![
            ("users".to_string(), "1".to_string()),
            ("pacing_ms".to_string(), "0".to_string()),
        ];
        let err = resolve_inputs(&d, &cli, &env).unwrap_err();
        assert!(err.contains("below min"), "{err}");

        let cli = vec![
            ("users".to_string(), "1".to_string()),
            ("nope".to_string(), "1".to_string()),
            ("pacing_ms".to_string(), "5".to_string()),
        ];
        let err = resolve_inputs(&d, &cli, &env).unwrap_err();
        assert!(err.contains("unknown input 'nope'"), "{err}");

        // Missing required input.
        let cli = vec![("pacing_ms".to_string(), "5".to_string())];
        let err = resolve_inputs(&d, &cli, &env).unwrap_err();
        assert!(err.contains("input 'users' is required"), "{err}");

        // A non-numeric override.
        let cli = vec![
            ("users".to_string(), "ten".to_string()),
            ("pacing_ms".to_string(), "5".to_string()),
        ];
        let err = resolve_inputs(&d, &cli, &env).unwrap_err();
        assert!(err.contains("not a number"), "{err}");
    }

    #[test]
    fn env_file_only_supplies_declared_keys() {
        let d = decls();
        let env = Map::from_iter([
            ("users".to_string(), json!("7")),
            ("pacing_ms".to_string(), json!("50")),
            ("UNRELATED".to_string(), json!("x")),
        ]);
        let out = resolve_inputs(&d, &[], &env).unwrap();
        assert_eq!(out["users"], json!(7));
        assert!(!out.contains_key("UNRELATED"));
    }
}
