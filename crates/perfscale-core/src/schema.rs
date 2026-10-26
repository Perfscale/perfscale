//! JSON Schema generation for test/config YAML files — used both for
//! pre-execution validation ([`crate::yaml`]) and for IDE autocomplete via a
//! `# yaml-language-server: $schema=...` modeline in example files.

use std::sync::OnceLock;

use schemars::schema_for;
use serde_json::json;

use crate::step::TestDef;
use crate::yaml::ConfigFile;

/// Schema for `-f test.yaml` documents.
pub fn test_schema() -> serde_json::Value {
    let mut schema =
        serde_json::to_value(schema_for!(TestDef)).expect("TestDef schema is always valid JSON");
    relax_step_aliases(&mut schema);
    schema
}

/// Schema for `-c config.yaml` documents.
pub fn config_schema() -> serde_json::Value {
    let mut schema = serde_json::to_value(schema_for!(ConfigFile))
        .expect("ConfigFile schema is always valid JSON");
    relax_step_aliases(&mut schema);
    close_config_top_level(&mut schema);
    schema
}

/// Compiled [`test_schema`], built once and cached. Schema compilation
/// dominates YAML parse cost (~99%), and the schema is static — derived from
/// the types — so a process-wide cache is correct.
pub fn compiled_test_schema() -> &'static jsonschema::JSONSchema {
    static CACHE: OnceLock<jsonschema::JSONSchema> = OnceLock::new();
    CACHE.get_or_init(|| {
        jsonschema::JSONSchema::compile(&test_schema()).expect("test schema always compiles")
    })
}

/// Compiled [`config_schema`], built once and cached; see
/// [`compiled_test_schema`].
pub fn compiled_config_schema() -> &'static jsonschema::JSONSchema {
    static CACHE: OnceLock<jsonschema::JSONSchema> = OnceLock::new();
    CACHE.get_or_init(|| {
        jsonschema::JSONSchema::compile(&config_schema()).expect("config schema always compiles")
    })
}

/// schemars does not emit serde aliases. Patch the generated `Step`
/// definition so validation accepts `use`/`uses` and `if`/`when`, mirroring
/// each canonical property under its alias. Both test and config schemas
/// reference the same `Step` definition.
fn relax_step_aliases(schema: &mut serde_json::Value) {
    let Some(step) = schema.pointer_mut("/definitions/Step") else {
        return;
    };
    if let Some(use_prop) = step.get("properties").and_then(|p| p.get("use")).cloned() {
        if let Some(props) = step.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.entry("uses").or_insert(use_prop);
        }
    }
    if let Some(condition) = step.get("properties").and_then(|p| p.get("if")).cloned() {
        if let Some(props) = step.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.entry("when").or_insert(condition);
        }
    }
    if let Some(obj) = step.as_object_mut() {
        obj.remove("required");
        obj.insert(
            "anyOf".to_string(),
            json!([{ "required": ["use"] }, { "required": ["uses"] }]),
        );
    }
}

/// `ConfigFile` flattens `RunConfig`, and serde's `deny_unknown_fields` does
/// not work through `flatten` (neither does schemars' emission of it) — so a
/// typo'd top-level key (`iterations: 1`) would still parse silently (#8).
/// Close the top level here instead: schemars inlines the flattened
/// `RunConfig` properties into `ConfigFile`'s `properties`, so
/// `additionalProperties: false` at the root rejects exactly the unknown
/// keys.
fn close_config_top_level(schema: &mut serde_json::Value) {
    if let Some(obj) = schema.as_object_mut() {
        obj.insert("additionalProperties".to_string(), json!(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_describes_steps_array() {
        let schema = test_schema();
        assert!(schema["properties"]["steps"].is_object());
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("steps")));
    }

    #[test]
    fn config_schema_describes_vus_and_duration_with_defaults() {
        let schema = config_schema();
        assert_eq!(schema["properties"]["vus"]["default"], 1);
        assert_eq!(schema["properties"]["duration"]["default"], "1m");
        // report is optional: not in the required list.
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        assert!(!required.contains(&serde_json::json!("report")));
    }

    #[test]
    fn config_schema_rejects_unknown_top_level_keys() {
        // The issue-#8 guard: `deny_unknown_fields` can't cross the
        // `RunConfig` flatten, so the schema patch is what rejects a typo'd
        // top-level key.
        let schema = config_schema();
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        let compiled = jsonschema::JSONSchema::compile(&schema).unwrap();
        assert!(compiled
            .validate(&serde_json::json!({"vus": 2, "iterations": 1}))
            .is_err());
        assert!(compiled
            .validate(&serde_json::json!({"vus": 2, "duration": "30s"}))
            .is_ok());
        assert!(compiled.validate(&serde_json::json!({})).is_ok());
    }

    #[test]
    fn both_schemas_compile_as_valid_json_schema() {
        jsonschema::JSONSchema::compile(&test_schema()).expect("test schema must compile");
        jsonschema::JSONSchema::compile(&config_schema()).expect("config schema must compile");
    }
}
