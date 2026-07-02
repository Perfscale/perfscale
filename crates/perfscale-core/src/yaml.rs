//! YAML parsing for test-definition and config files, validated against a
//! JSON Schema first so errors point at the offending field/path instead of
//! a raw serde error dump.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::step::{RunConfig, TestDef};

/// Where to forward the aggregated run summary after `perfscale run` finishes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReportConfig {
    /// Base URL of a running `perfscale serve` instance, e.g. `http://localhost:7999`.
    pub url: String,
}

/// Top-level `-c config.yaml` document.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ConfigFile {
    #[serde(flatten)]
    pub run: RunConfig,
    pub report: Option<ReportConfig>,
}

/// Parse a test-definition YAML document (`-f test.yaml`).
pub fn parse_test_file(yaml: &str) -> Result<TestDef, String> {
    parse_with_schema(yaml, crate::schema::test_schema())
}

/// Parse a config YAML document (`-c config.yaml`).
pub fn parse_config_file(yaml: &str) -> Result<ConfigFile, String> {
    parse_with_schema(yaml, crate::schema::config_schema())
}

fn parse_with_schema<T: serde::de::DeserializeOwned>(
    yaml: &str,
    schema: serde_json::Value,
) -> Result<T, String> {
    let value: serde_json::Value =
        serde_yaml::from_str(yaml).map_err(|e| format!("invalid YAML: {e}"))?;

    let compiled = jsonschema::JSONSchema::compile(&schema)
        .map_err(|e| format!("internal schema error: {e}"))?;
    if let Err(errors) = compiled.validate(&value) {
        let messages: Vec<String> = errors
            .map(|e| format!("{} — {e}", e.instance_path))
            .collect();
        return Err(format!(
            "schema validation failed:\n  {}",
            messages.join("\n  ")
        ));
    }

    serde_json::from_value(value).map_err(|e| format!("invalid document: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_test_file() {
        let yaml = r#"
steps:
  - name: ping
    use: std/http@v1
    with:
      method: GET
      url: https://example.com
    check:
      status: 200
"#;
        let test = parse_test_file(yaml).unwrap();
        assert_eq!(test.steps.len(), 1);
        assert_eq!(test.steps[0].action, "std/http@v1");
    }

    #[test]
    fn rejects_test_file_missing_use() {
        let yaml = r#"
steps:
  - name: ping
    with:
      url: https://example.com
"#;
        let err = parse_test_file(yaml).unwrap_err();
        assert!(
            err.contains("schema validation failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_malformed_yaml() {
        let yaml = "steps: [this is not valid: yaml: at all";
        let err = parse_test_file(yaml).unwrap_err();
        assert!(err.contains("invalid YAML"), "unexpected error: {err}");
    }

    #[test]
    fn parses_config_file_with_defaults() {
        let cfg = parse_config_file("vus: 10\nduration: 30s\n").unwrap();
        assert_eq!(cfg.run.vus, 10);
        assert_eq!(cfg.run.duration, "30s");
        assert!(cfg.report.is_none());
    }

    #[test]
    fn parses_config_file_with_report() {
        let yaml = "vus: 5\nduration: 1m\nreport:\n  url: http://localhost:7999\n";
        let cfg = parse_config_file(yaml).unwrap();
        assert_eq!(cfg.report.unwrap().url, "http://localhost:7999");
    }

    #[test]
    fn empty_config_file_uses_run_config_defaults() {
        let cfg = parse_config_file("{}").unwrap();
        assert_eq!(cfg.run.vus, 1);
        assert_eq!(cfg.run.duration, "1m");
    }

    #[test]
    fn rejects_config_with_wrong_field_type() {
        let err = parse_config_file("vus: not-a-number\n").unwrap_err();
        assert!(
            err.contains("schema validation failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_config_with_report_missing_url() {
        let err = parse_config_file("report: {}\n").unwrap_err();
        assert!(
            err.contains("schema validation failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_test_file_with_wrong_steps_type() {
        let err = parse_test_file("steps: not-a-list\n").unwrap_err();
        assert!(
            err.contains("schema validation failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_test_file_with_no_steps_key() {
        let err = parse_test_file("name: whoops\n").unwrap_err();
        assert!(
            err.contains("schema validation failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parses_test_file_with_every_builtin_action() {
        let yaml = r#"
steps:
  - use: std/http@v1
    with: { url: https://example.com }
    outputs: resp
  - use: std/check@v1
    with: { on: resp, status: 200 }
  - use: std/sleep@v1
    with: { ms: 5 }
  - use: std/log@v1
    with: { message: done }
"#;
        let test = parse_test_file(yaml).unwrap();
        assert_eq!(test.steps.len(), 4);
        assert_eq!(test.steps[0].outputs.as_deref(), Some("resp"));
    }

    #[test]
    fn config_file_round_trips_through_serde() {
        let cfg = ConfigFile {
            run: RunConfig {
                vus: 7,
                duration: "2m".into(),
            },
            report: Some(ReportConfig {
                url: "http://localhost:7999".into(),
            }),
        };
        let json = serde_json::to_value(&cfg).unwrap();
        let back: ConfigFile = serde_json::from_value(json).unwrap();
        assert_eq!(back.run.vus, 7);
        assert_eq!(back.report.unwrap().url, "http://localhost:7999");
    }

    #[test]
    fn config_file_default_has_no_report() {
        let cfg = ConfigFile::default();
        assert_eq!(cfg.run.vus, 1);
        assert!(cfg.report.is_none());
    }
}
