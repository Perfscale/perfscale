//! YAML parsing for test-definition and config files, validated against a
//! JSON Schema first so errors point at the offending field/path instead of
//! a raw serde error dump.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::step::{RunConfig, Step, TestDef};

/// Where to forward the aggregated run summary after `perfscale run` finishes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReportConfig {
    /// Base URL of a running `perfscale serve` instance, e.g. `http://localhost:7999`.
    pub url: String,
}

/// Top-level `-c config.yaml` document.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ConfigFile {
    /// Base document to inherit from — a relative path, an `http(s)://` URL,
    /// or `{ git, ref, file }`. The base loads first (recursively), then this
    /// document deep-merges on top: objects merge, scalars/arrays here win.
    /// Remote sources require the caller's `--allow-remote-import`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import: Option<crate::import::ImportSpec>,

    #[serde(flatten)]
    pub run: RunConfig,
    pub report: Option<ReportConfig>,

    /// Setup steps run **once** before the load starts (not per VU iteration).
    /// Each step's `outputs` is exposed to test steps under the `config`
    /// namespace, e.g. a `before` step with `outputs: fix_config` is read in a
    /// test as `${{ config.fix_config.<field> }}`. If any setup step fails, the
    /// run aborts before spawning VUs.
    #[serde(default)]
    pub before: Vec<Step>,

    /// Teardown steps run **once** after the load stops — on a normal finish,
    /// on a failed run, on a failed `before`, and on Ctrl-C/SIGTERM alike.
    /// They see `${{ config.* }}` and `${{ vars.* }}` like test steps do.
    /// Unlike `before`, a failing teardown step is logged but does not abort
    /// the remaining steps (best-effort cleanup).
    #[serde(default)]
    pub after: Vec<Step>,

    /// Static variables exposed to `before` and test steps under the `vars`
    /// namespace, e.g. `${{ vars.region }}`. Values may themselves be objects.
    #[serde(default)]
    pub variables: serde_json::Map<String, serde_json::Value>,

    /// Shared mutable variables for `std/set_shared_variable@v1` /
    /// `std/get_shared_variable@v1`: a map of name → initial JSON value,
    /// shared by every VU of the run. Declaration is mandatory — a step
    /// referencing an undeclared name (or an op incompatible with the type
    /// inferred from the initial value) fails validation before the run
    /// starts.
    #[serde(default)]
    pub shared_variables: serde_json::Map<String, serde_json::Value>,
}

/// Parse a test-definition YAML document (`-f test.yaml`).
pub fn parse_test_file(yaml: &str) -> Result<TestDef, String> {
    parse_with_schema(yaml, crate::schema::test_schema())
}

/// Parse a config YAML document (`-c config.yaml`).
pub fn parse_config_file(yaml: &str) -> Result<ConfigFile, String> {
    parse_with_schema(yaml, crate::schema::config_schema())
}

/// Validate an already-parsed (import-merged) JSON value as a test definition.
pub fn test_from_value(value: serde_json::Value) -> Result<TestDef, String> {
    validate_with_schema(value, crate::schema::test_schema())
}

/// Validate an already-parsed (import-merged) JSON value as a config document.
pub fn config_from_value(value: serde_json::Value) -> Result<ConfigFile, String> {
    validate_with_schema(value, crate::schema::config_schema())
}

fn parse_with_schema<T: serde::de::DeserializeOwned>(
    yaml: &str,
    schema: serde_json::Value,
) -> Result<T, String> {
    let value: serde_json::Value =
        serde_yaml::from_str(yaml).map_err(|e| format!("invalid YAML: {e}"))?;
    validate_with_schema(value, schema)
}

fn validate_with_schema<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    schema: serde_json::Value,
) -> Result<T, String> {
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

    /// `${{ ... }}` placeholders (GitHub-Actions-style) must survive YAML
    /// parsing verbatim — quoted or plain — so the runtime interpolator
    /// receives them untouched.
    #[test]
    fn placeholders_survive_yaml_parsing_verbatim() {
        let yaml = r#"
steps:
  - use: std/http@v1
    with:
      url: "https://api.example.com/users/${{ login.body }}"
      headers:
        authorization: Bearer ${{ login.body }}
    check:
      body_contains: "${{ login.body }}"
    outputs: user
"#;
        let test = parse_test_file(yaml).unwrap();
        let with = test.steps[0].with.as_ref().unwrap();
        assert_eq!(
            with["url"],
            "https://api.example.com/users/${{ login.body }}"
        );
        assert_eq!(with["headers"]["authorization"], "Bearer ${{ login.body }}");
        assert_eq!(
            test.steps[0].check.as_ref().unwrap()["body_contains"],
            "${{ login.body }}"
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
            import: None,
            run: RunConfig {
                vus: 7,
                duration: "2m".into(),
                ..Default::default()
            },
            report: Some(ReportConfig {
                url: "http://localhost:7999".into(),
            }),
            before: Vec::new(),
            after: Vec::new(),
            variables: serde_json::Map::new(),
            shared_variables: serde_json::Map::new(),
        };
        let json = serde_json::to_value(&cfg).unwrap();
        let back: ConfigFile = serde_json::from_value(json).unwrap();
        assert_eq!(back.run.vus, 7);
        assert_eq!(back.report.unwrap().url, "http://localhost:7999");
    }

    #[test]
    fn parses_config_with_before_and_variables() {
        let yaml = r#"
vus: 50
variables:
  region: eu
  retries: 3
before:
  - uses: std/http@v1
    with:
      url: https://example.com/token
    outputs: auth
"#;
        let cfg = parse_config_file(yaml).unwrap();
        assert_eq!(cfg.run.vus, 50);
        assert_eq!(cfg.variables["region"], "eu");
        assert_eq!(cfg.variables["retries"], 3);
        assert_eq!(cfg.before.len(), 1);
        // `uses:` alias resolves to the same action field as `use:`.
        assert_eq!(cfg.before[0].action, "std/http@v1");
        assert_eq!(cfg.before[0].outputs.as_deref(), Some("auth"));
    }

    #[test]
    fn parses_config_with_shared_variables() {
        let yaml = r#"
vus: 4
shared_variables:
  pending_orders: []
  approved_count: 0
"#;
        let cfg = parse_config_file(yaml).unwrap();
        assert_eq!(
            cfg.shared_variables["pending_orders"],
            serde_json::json!([])
        );
        assert_eq!(cfg.shared_variables["approved_count"], 0);
        // Absent by default, and wire-compatible with older configs.
        let cfg = parse_config_file("vus: 1\n").unwrap();
        assert!(cfg.shared_variables.is_empty());
    }

    #[test]
    fn config_without_before_or_variables_defaults_empty() {
        let cfg = parse_config_file("vus: 3\n").unwrap();
        assert!(cfg.before.is_empty());
        assert!(cfg.after.is_empty());
        assert!(cfg.variables.is_empty());
    }

    #[test]
    fn parses_config_with_after_section() {
        let yaml = r#"
vus: 10
after:
  - name: stop-keeper
    uses: std/kill_process@v1
    with:
      name: keeper
"#;
        let cfg = parse_config_file(yaml).unwrap();
        assert_eq!(cfg.after.len(), 1);
        assert_eq!(cfg.after[0].action, "std/kill_process@v1");
        assert_eq!(cfg.after[0].name.as_deref(), Some("stop-keeper"));
        assert_eq!(cfg.after[0].with.as_ref().unwrap()["name"], "keeper");
    }

    #[test]
    fn after_step_pro_action_survives_schema_validation() {
        // pro/* actions are registered at runtime; the config schema must not
        // reject an after step that uses one.
        let yaml = r#"
after:
  - uses: pro/fix-close@v1
    with:
      host: example.com
"#;
        let cfg = parse_config_file(yaml).unwrap();
        assert_eq!(cfg.after[0].action, "pro/fix-close@v1");
    }

    #[test]
    fn parses_allow_process_actions_flag() {
        let cfg = parse_config_file("allow_process_actions: true\n").unwrap();
        assert!(cfg.run.allow_process_actions);
        // Fail-closed default.
        let cfg = parse_config_file("vus: 1\n").unwrap();
        assert!(!cfg.run.allow_process_actions);
    }

    #[test]
    fn test_step_accepts_uses_alias() {
        let yaml = r#"
steps:
  - uses: std/log@v1
    with: { message: hi }
"#;
        let test = parse_test_file(yaml).unwrap();
        assert_eq!(test.steps[0].action, "std/log@v1");
    }

    #[test]
    fn step_with_neither_use_nor_uses_is_rejected() {
        let yaml = r#"
steps:
  - with: { message: hi }
"#;
        let err = parse_test_file(yaml).unwrap_err();
        assert!(
            err.contains("schema validation failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn before_step_pro_action_survives_schema_validation() {
        // pro/* actions are registered at runtime; the config schema must not
        // reject a before step that uses one.
        let yaml = r#"
vus: 10
before:
  - uses: pro/fix-config@v1
    with:
      host: example.com
      port: 1111
    outputs: fix_config
"#;
        let cfg = parse_config_file(yaml).unwrap();
        assert_eq!(cfg.before[0].action, "pro/fix-config@v1");
    }

    #[test]
    fn config_file_default_has_no_report() {
        let cfg = ConfigFile::default();
        assert_eq!(cfg.run.vus, 1);
        assert!(cfg.report.is_none());
    }

    #[test]
    fn parses_gpu_section_with_defaults() {
        let yaml = r#"
vus: 4
gpu:
  enabled: true
"#;
        let cfg = parse_config_file(yaml).unwrap();
        let gpu = cfg.run.gpu.expect("gpu section parsed");
        assert!(gpu.enabled);
        assert_eq!(gpu.interval_ms, 1000);
        assert_eq!(gpu.source, "nvidia-smi");
        assert!(gpu.dcgm_url.is_none());
        assert!(gpu.devices.is_none());
    }

    #[test]
    fn parses_gpu_section_full() {
        let yaml = r#"
gpu:
  enabled: true
  interval_ms: 500
  source: dcgm
  dcgm_url: http://10.0.0.5:9400/metrics
  devices: [0, 1]
"#;
        let cfg = parse_config_file(yaml).unwrap();
        let gpu = cfg.run.gpu.expect("gpu section parsed");
        assert_eq!(gpu.interval_ms, 500);
        assert_eq!(gpu.source, "dcgm");
        assert_eq!(
            gpu.dcgm_url.as_deref(),
            Some("http://10.0.0.5:9400/metrics")
        );
        assert_eq!(gpu.devices, Some(vec![0, 1]));
    }

    #[test]
    fn config_without_gpu_has_none_and_stays_off_the_wire() {
        let cfg = parse_config_file("vus: 2\n").unwrap();
        assert!(cfg.run.gpu.is_none());
        // Wire-compatible: absent key deserializes, None never serializes
        // (perfscaled embeds RunConfig).
        let json = serde_json::to_value(RunConfig::default()).unwrap();
        assert!(json.get("gpu").is_none());
    }

    #[test]
    fn rejects_gpu_section_with_wrong_types() {
        for bad in [
            "gpu: yes\n",                                 // not an object
            "gpu:\n  enabled: yes please\n",              // enabled not a bool
            "gpu:\n  enabled: true\n  interval_ms: -5\n", // negative interval
            "gpu:\n  enabled: true\n  devices: zero\n",   // devices not a list
        ] {
            let err = parse_config_file(bad).unwrap_err();
            assert!(
                err.contains("schema validation failed"),
                "'{bad}' → unexpected error: {err}"
            );
        }
    }
}
