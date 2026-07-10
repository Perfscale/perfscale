//! JSON Schema generation for test/config YAML files — used both for
//! pre-execution validation ([`crate::yaml`]) and for IDE autocomplete via a
//! `# yaml-language-server: $schema=...` modeline in example files.

use schemars::schema_for;
use serde_json::json;

use crate::step::TestDef;
use crate::yaml::ConfigFile;

/// Schema for `-f test.yaml` documents.
pub fn test_schema() -> serde_json::Value {
    let mut schema =
        serde_json::to_value(schema_for!(TestDef)).expect("TestDef schema is always valid JSON");
    relax_use_alias(&mut schema);
    schema
}

/// Schema for `-c config.yaml` documents.
pub fn config_schema() -> serde_json::Value {
    let mut schema = serde_json::to_value(schema_for!(ConfigFile))
        .expect("ConfigFile schema is always valid JSON");
    relax_use_alias(&mut schema);
    schema
}

/// schemars emits `use` as the sole required property of a step, but the
/// runtime also accepts the `uses` alias (`#[serde(alias = "uses")]`). Patch
/// the generated `Step` definition so validation accepts either: add a `uses`
/// property mirroring `use`, and require exactly one of the two. Both the test
/// and config schemas reference the same `Step` definition.
fn relax_use_alias(schema: &mut serde_json::Value) {
    let Some(step) = schema.pointer_mut("/definitions/Step") else {
        return;
    };
    if let Some(use_prop) = step.get("properties").and_then(|p| p.get("use")).cloned() {
        if let Some(props) = step.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.entry("uses").or_insert(use_prop);
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
    fn both_schemas_compile_as_valid_json_schema() {
        jsonschema::JSONSchema::compile(&test_schema()).expect("test schema must compile");
        jsonschema::JSONSchema::compile(&config_schema()).expect("config schema must compile");
    }
}
