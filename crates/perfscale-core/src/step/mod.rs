//! Perfscale native step model and engine.
//!
//! # Overview
//!
//! A **test definition** is a YAML/JSON document with a `steps` array.  Each
//! step identifies a built-in action (`use`), passes parameters (`with`),
//! optionally stores its output (`outputs`), and can assert expectations
//! (`check`).
//!
//! ```yaml
//! steps:
//!   - name: ping
//!     use: std/http@v1
//!     with:
//!       method: GET
//!       url: https://api.example.com/health
//!     check:
//!       status: 200
//!     outputs: resp
//!   - use: std/sleep@v1
//!     with:
//!       ms: 200
//! ```
//!
//! Use [`runner::run_steps`] to execute a test under a given [`RunConfig`].

pub mod actions;
pub mod context;
pub mod runner;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Test definition
// ---------------------------------------------------------------------------

/// Top-level test definition — a list of steps executed per VU iteration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestDef {
    pub steps: Vec<Step>,
}

// ---------------------------------------------------------------------------
// Step
// ---------------------------------------------------------------------------

/// A single step in a test definition.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Step {
    /// Human-readable label shown in log output.
    pub name: Option<String>,

    /// Action identifier, e.g. `"std/http@v1"`. Written as `use:` in YAML;
    /// `uses:` is accepted as an alias (GitHub-Actions muscle memory).
    #[serde(rename = "use", alias = "uses")]
    pub action: String,

    /// Action-specific parameters (interpolation applied at runtime).
    pub with: Option<serde_json::Value>,

    /// Post-execution assertions.  Keys are assertion names; values are
    /// expected values.  Example: `{ "status": 200, "duration_ms_lt": 500 }`.
    pub check: Option<serde_json::Value>,

    /// Variable name to store step output under for `${{ name.field }}` use.
    pub outputs: Option<String>,
}

// ---------------------------------------------------------------------------
// Run configuration
// ---------------------------------------------------------------------------

/// Load configuration — number of virtual users and how long to run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunConfig {
    /// Number of virtual users (concurrent workers).
    #[serde(default = "default_vus")]
    pub vus: u32,

    /// Duration string: `"30s"`, `"1m"`, `"5m30s"`, `"1h"`.
    #[serde(default = "default_duration")]
    pub duration: String,
}

fn default_vus() -> u32 {
    1
}
fn default_duration() -> String {
    "1m".to_string()
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            vus: default_vus(),
            duration: default_duration(),
        }
    }
}

impl RunConfig {
    /// Parse `duration` string into whole seconds.
    pub fn duration_secs(&self) -> u64 {
        parse_duration_secs(&self.duration)
    }
}

/// Parse a human duration string into seconds.
/// Handles: `"30s"`, `"1m"`, `"5m30s"`, `"1h"`, bare numbers (treated as seconds).
pub fn parse_duration_secs(s: &str) -> u64 {
    let mut total = 0u64;
    let mut num = String::new();
    for ch in s.chars() {
        match ch {
            '0'..='9' => num.push(ch),
            'h' => {
                total += num.parse::<u64>().unwrap_or(0) * 3600;
                num.clear();
            }
            'm' => {
                total += num.parse::<u64>().unwrap_or(0) * 60;
                num.clear();
            }
            's' => {
                total += num.parse::<u64>().unwrap_or(0);
                num.clear();
            }
            _ => {}
        }
    }
    if !num.is_empty() {
        total += num.parse::<u64>().unwrap_or(0);
    }
    total.max(1)
}

/// Resolve a well-known preset ID to a [`RunConfig`].
pub fn preset_config(id: &str) -> Option<RunConfig> {
    match id {
        "debug" => Some(RunConfig {
            vus: 1,
            duration: "1m".into(),
        }),
        "smoke" => Some(RunConfig {
            vus: 5,
            duration: "30s".into(),
        }),
        "load" => Some(RunConfig {
            vus: 10,
            duration: "5m".into(),
        }),
        "stress" => Some(RunConfig {
            vus: 50,
            duration: "5m".into(),
        }),
        "spike" => Some(RunConfig {
            vus: 100,
            duration: "1m".into(),
        }),
        "soak" => Some(RunConfig {
            vus: 10,
            duration: "30m".into(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_variants() {
        assert_eq!(parse_duration_secs("30s"), 30);
        assert_eq!(parse_duration_secs("1m"), 60);
        assert_eq!(parse_duration_secs("5m"), 300);
        assert_eq!(parse_duration_secs("1h"), 3600);
        assert_eq!(parse_duration_secs("1m30s"), 90);
        assert_eq!(parse_duration_secs("0s"), 1); // minimum 1
    }

    #[test]
    fn parse_duration_bare_number_is_seconds() {
        assert_eq!(parse_duration_secs("45"), 45);
    }

    #[test]
    fn parse_duration_garbage_is_minimum() {
        assert_eq!(parse_duration_secs("not-a-duration"), 1);
        assert_eq!(parse_duration_secs(""), 1);
    }

    #[test]
    fn run_config_default_is_one_vu_one_minute() {
        let cfg = RunConfig::default();
        assert_eq!(cfg.vus, 1);
        assert_eq!(cfg.duration, "1m");
    }

    #[test]
    fn run_config_duration_secs_delegates_to_parser() {
        let cfg = RunConfig {
            vus: 1,
            duration: "2m".into(),
        };
        assert_eq!(cfg.duration_secs(), 120);
    }

    #[test]
    fn preset_config_known_ids() {
        assert_eq!(preset_config("debug").unwrap().vus, 1);
        assert_eq!(preset_config("smoke").unwrap().vus, 5);
        assert_eq!(preset_config("load").unwrap().vus, 10);
        assert_eq!(preset_config("stress").unwrap().vus, 50);
        assert_eq!(preset_config("spike").unwrap().vus, 100);
        assert_eq!(preset_config("soak").unwrap().duration, "30m");
    }

    #[test]
    fn preset_config_unknown_id_is_none() {
        assert!(preset_config("nonexistent").is_none());
    }

    #[test]
    fn run_config_missing_fields_use_defaults_via_serde() {
        let cfg: RunConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.vus, 1);
        assert_eq!(cfg.duration, "1m");
    }

    #[test]
    fn step_renames_action_field_to_use() {
        let step: Step = serde_json::from_str(r#"{"use": "std/http@v1"}"#).unwrap();
        assert_eq!(step.action, "std/http@v1");
        assert!(step.name.is_none());
        assert!(step.with.is_none());

        let round_tripped = serde_json::to_value(&step).unwrap();
        assert_eq!(round_tripped["use"], "std/http@v1");
        assert!(round_tripped.get("action").is_none());
    }

    #[test]
    fn test_def_deserializes_multiple_steps() {
        let def: TestDef = serde_json::from_str(
            r#"{"steps": [{"use": "std/sleep@v1"}, {"name": "ping", "use": "std/http@v1", "with": {"url": "https://example.com"}}]}"#,
        )
        .unwrap();
        assert_eq!(def.steps.len(), 2);
        assert_eq!(def.steps[1].name.as_deref(), Some("ping"));
    }
}
