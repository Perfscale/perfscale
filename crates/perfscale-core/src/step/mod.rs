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
pub(crate) mod db;
pub mod graphql;
pub(crate) mod grpc;
pub mod http;
pub mod llm;
pub mod process;
pub mod pubsub;
pub(crate) mod resources;
pub mod runner;
pub mod schedule;
pub mod shared_variable;
pub mod thresholds;
pub(crate) mod ws;

pub use crate::report::{DuringRunShipper, MetricSnapshot};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Test definition
// ---------------------------------------------------------------------------

/// Top-level test definition — a list of steps executed per VU iteration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestDef {
    /// Base document to inherit from — a relative path, an `http(s)://` URL,
    /// or `{ git, ref, file }`. The base loads first (recursively), then this
    /// document deep-merges on top; a local `steps:` list replaces the base's.
    /// Remote sources require the caller's `--allow-remote-import`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import: Option<crate::import::ImportSpec>,

    /// Value-generator libraries for `${alias.fn(...)}` tokens (RFC 005).
    /// Concatenates with the config file's and with imported documents'
    /// declarations; a duplicate alias is a validation error. Every entry
    /// must declare `capabilities:` (`[]` for no grants).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub libraries: Option<Vec<crate::library::LibraryRef>>,

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

    /// Gate severity for `std/thresholds@v1` steps: `fail` (default),
    /// `warn`, or `info`. Meaningless for other actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,

    /// Custom message for `std/thresholds@v1` steps (interpolated, appended
    /// to the auto-generated violation summary). Meaningless for other
    /// actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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

    /// Duration string: `"30s"`, `"500ms"`, `"1.5s"`, `"1m"`, `"5m30s"`,
    /// `"1h"`. Sub-second values are honored by the native engine.
    #[serde(default = "default_duration")]
    pub duration: String,

    /// Allow filesystem-touching steps: `std/file-read@v1`,
    /// `std/file-write@v1`, and `file` parts in `std/http@v1` multipart
    /// uploads. Fail-closed: defaults to `false` so a step list from an
    /// untrusted source cannot read or write arbitrary paths.
    #[serde(default)]
    pub allow_file_actions: bool,

    /// Allow process-touching steps: `std/child_process@v1` and
    /// `std/kill_process@v1`. Fail-closed: defaults to `false` so a step list
    /// from an untrusted source cannot spawn or signal OS processes.
    #[serde(default)]
    pub allow_process_actions: bool,

    /// Allow `libraries:` entries to carry a non-empty `capabilities:` grant
    /// (fs/net/clock for WASM libraries, RFC 005). Fail-closed: defaults to
    /// `false`, and any grant without it is a validation error.
    #[serde(default)]
    pub allow_library_capabilities: bool,

    /// Run-level determinism seed (RFC 005): when set, every `${…}`
    /// generator and library instance derives its seed as
    /// `hash(seed, vu_id, conn_seq)`, making the run's generated values
    /// reproducible. Wall-clock time stays non-deterministic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,

    /// Ramping-VU load profile (k6-style): ramp the number of virtual users
    /// linearly to each stage's `target` over its `duration`. Mutually
    /// exclusive with `arrival`; when present it overrides `vus`/`duration`
    /// (total run length is the sum of stage durations). Native engine only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<VuStage>,

    /// Arrival-rate load profile (open model): hold an iterations-per-second
    /// rate profile, growing a worker pool up to `arrival.max_vus`. Mutually
    /// exclusive with `stages`. Native engine only. Boxed to keep `RunConfig`
    /// (and the `ExecutionPlan` enum embedding it) compact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrival: Option<Box<ArrivalConfig>>,

    /// GPU metrics collection for the run (`gpu:`) — sample utilization,
    /// VRAM, temperature, and power every `interval_ms` while the VUs run,
    /// and land a `gpu` section in the run summary. Off by default;
    /// collection failures never fail the run. Native engine only. Boxed to
    /// keep `RunConfig` (and the `ExecutionPlan` enum embedding it) compact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<Box<crate::gpu::GpuConfig>>,

    /// Metrics reporting (`report:`) — forward the run summary, and with
    /// `during_run: true` stream cumulative metric snapshots while the VUs
    /// run (see [`crate::report`]). Off by default. Native engine only.
    ///
    /// Skipped in the JSON schema: a `-c config.yaml` document carries its
    /// own typed `report:` block ([`crate::yaml::ReportConfig`], `url`
    /// required) next to the flattened `RunConfig` — without the skip the
    /// two `report` properties would collide in the generated schema, and
    /// the flattened one (all-optional) would shadow the typed one. The
    /// embedding process (CLI, agent) maps the config-file block into this
    /// field programmatically; serde stays live so a standalone `RunConfig`
    /// on the wire (perfscaled) can carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub report: Option<crate::report::ReportRunConfig>,

    /// Confinement root for file actions. When set, every file path a step
    /// touches is canonicalized and must stay under this directory (`../`
    /// escapes and symlink hops out of it are rejected). Never parsed from
    /// the wire/YAML: the embedding process (agent, CLI) sets it from its
    /// own trusted configuration.
    #[serde(skip)]
    #[schemars(skip)]
    pub fs_root: Option<std::path::PathBuf>,

    /// WebRTC media-plane configuration (RFC 007) handed to the pro
    /// `pro/webrtc-*` actions through [`context::Context::webrtc_config`]. A
    /// `-c config.yaml` document carries its own typed `webrtc:` block
    /// ([`crate::yaml::WebRtcConfig`] next to the flattened `RunConfig`);
    /// the embedding process (CLI, agent) maps that block into this field,
    /// exactly like `report` — serde stays live so a standalone `RunConfig`
    /// on the wire (perfscaled) can carry it. Requires a build with the pro
    /// webrtc module registered (see `yaml::require_webrtc_module`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub webrtc: Option<crate::yaml::WebRtcConfig>,
}

/// One `stages:` entry — ramp the VU count linearly to `target` over
/// `duration`. The first stage ramps from 0 VUs, each later stage from the
/// previous stage's `target` (like k6's `ramping-vus` with `startVUs: 0`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VuStage {
    /// Stage length: `"30s"`, `"500ms"`, `"1.5s"`, `"1m"`, `"5m30s"`, `"1h"`
    /// — minimum 1ms.
    pub duration: String,

    /// Virtual users to reach by the end of the stage. `0` is a full
    /// ramp-down: remaining VUs exit gracefully at their next step boundary.
    pub target: u32,
}

/// Arrival-rate configuration (`arrival:`) — an open load model: the engine
/// holds an iterations-per-second profile and scales a worker pool to keep
/// up, instead of looping a fixed set of VUs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArrivalConfig {
    /// Hard cap on the worker pool — must be ≥ 1. A permit that arrives while
    /// all `max_vus` workers are busy is dropped and counted in the
    /// `dropped_iterations` metric. Effectively required: the default (0) is
    /// rejected at validation time.
    #[serde(default)]
    pub max_vus: u32,

    /// Workers spawned at run start (default 1). The pool grows lazily up to
    /// `max_vus` while permits arrive faster than the pool can serve them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_allocated_vus: Option<u32>,

    /// Rate profile: ramp the arrival rate linearly to each stage's `rate`
    /// over its `duration`. The first stage ramps from 0 iterations/sec.
    pub stages: Vec<RateStage>,
}

/// One `arrival.stages:` entry — ramp the arrival rate linearly to `rate`
/// over `duration`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RateStage {
    /// Stage length: `"30s"`, `"500ms"`, `"1.5s"`, `"1m"`, `"5m30s"`, `"1h"`
    /// — minimum 1ms.
    pub duration: String,

    /// Iterations per second to reach by the end of the stage. Fractions are
    /// allowed (`0.5` = one iteration every 2s); must be ≥ 0.
    pub rate: f64,
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
            allow_file_actions: false,
            allow_process_actions: false,
            allow_library_capabilities: false,
            seed: None,
            stages: Vec::new(),
            arrival: None,
            gpu: None,
            report: None,
            fs_root: None,
            webrtc: None,
        }
    }
}

impl RunConfig {
    /// Parse `duration` string into whole seconds (truncated, minimum 1).
    pub fn duration_secs(&self) -> u64 {
        parse_duration_secs(&self.duration)
    }

    /// Parse `duration` string into milliseconds — `"500ms"`, `"1.5s"`,
    /// `"30s"` all work; the legacy lenient fallbacks apply (garbage → 1s).
    pub fn duration_ms(&self) -> u64 {
        parse_duration_ms(&self.duration)
    }
}

/// Scan a human duration string into milliseconds, or `None` when the input
/// is malformed. Grammar: a sequence of `<number><unit>` where the unit is
/// `h`, `m`, `s` or `ms` and the number may be fractional (`"1.5s"`); a
/// trailing bare number counts as seconds. In `strict` mode every byte must
/// fit the grammar; in lenient mode unknown characters are skipped and a unit
/// without a number counts as zero (the historical `parse_duration_secs`
/// behaviour).
fn scan_duration_ms(s: &str, strict: bool) -> Option<u64> {
    let chars: Vec<char> = s.trim().chars().collect();
    let mut i = 0;
    let mut total = 0.0f64;
    let mut any_number = false;
    while i < chars.len() {
        let start = i;
        let mut dot = false;
        while i < chars.len() && (chars[i].is_ascii_digit() || (chars[i] == '.' && !dot)) {
            dot |= chars[i] == '.';
            i += 1;
        }
        let num: f64 = if start == i {
            if strict {
                // A unit (or the end) with no number before it.
                return None;
            }
            // Lenient: not a number — skip one byte of garbage and move on.
            i += 1;
            continue;
        } else {
            any_number = true;
            chars[start..i].iter().collect::<String>().parse().ok()?
        };
        // Unit: `ms` (two chars) wins over `m`; no unit = bare seconds.
        let unit_ms = if i + 1 < chars.len() && chars[i] == 'm' && chars[i + 1] == 's' {
            i += 2;
            1.0
        } else if i < chars.len() && matches!(chars[i], 'h' | 'm' | 's') {
            let u = match chars[i] {
                'h' => 3_600_000.0,
                'm' => 60_000.0,
                _ => 1_000.0,
            };
            i += 1;
            u
        } else {
            // Bare number = seconds, but only as the final token — a stray
            // letter after it ("1h30x") is a typo, not a unit.
            if strict && i < chars.len() {
                return None;
            }
            1_000.0
        };
        total += num * unit_ms;
    }
    if !any_number {
        return None;
    }
    Some(total.round() as u64)
}

/// Parse a human duration string into milliseconds.
/// Handles: `"500ms"`, `"30s"`, `"1.5s"`, `"1m"`, `"5m30s"`, `"1h"`, bare
/// numbers (treated as seconds). Lenient: unparseable or zero input falls
/// back to 1000ms (the historical 1s floor).
pub fn parse_duration_ms(s: &str) -> u64 {
    match scan_duration_ms(s, false) {
        Some(ms) if ms > 0 => ms,
        _ => 1000,
    }
}

/// Parse a human duration string into seconds.
/// Handles: `"30s"`, `"1m"`, `"5m30s"`, `"1h"`, bare numbers (treated as
/// seconds). Sub-second input truncates to 0 and clamps to the 1s minimum —
/// use [`parse_duration_ms`] when sub-second precision matters.
pub fn parse_duration_secs(s: &str) -> u64 {
    (parse_duration_ms(s) / 1000).max(1)
}

/// Strict variant of [`parse_duration_ms`] for load-profile stages: instead
/// of clamping garbage to the floor it returns an error, and zero-length
/// durations are rejected — a stage with no (or unparseable) length is a
/// config bug the user should fix, not silently run.
pub fn parse_duration_ms_strict(s: &str) -> Result<u64, String> {
    if s.trim().is_empty() {
        return Err("empty duration — use e.g. \"30s\", \"500ms\", \"1m30s\", \"1h\"".into());
    }
    let ms = scan_duration_ms(s, true).ok_or_else(|| {
        format!("invalid duration '{s}' — use e.g. \"30s\", \"500ms\", \"1m30s\", \"1h\"")
    })?;
    if ms == 0 {
        return Err(format!("invalid duration '{s}': must be at least 1ms"));
    }
    Ok(ms)
}

/// Strict variant of [`parse_duration_secs`] for load-profile stages: instead
/// of clamping garbage to 1s it returns an error, and zero-length durations
/// are rejected — a stage with no (or unparseable) length is a config bug the
/// user should fix, not silently run. Sub-second durations round up to 1s;
/// use [`parse_duration_ms_strict`] when sub-second precision matters.
pub fn parse_duration_secs_strict(s: &str) -> Result<u64, String> {
    Ok(parse_duration_ms_strict(s)?.div_ceil(1000))
}

/// Resolve a well-known preset ID to a [`RunConfig`].
pub fn preset_config(id: &str) -> Option<RunConfig> {
    let run = |vus, duration: &str| RunConfig {
        vus,
        duration: duration.into(),
        ..Default::default()
    };
    match id {
        "debug" => Some(run(1, "1m")),
        "smoke" => Some(run(5, "30s")),
        "load" => Some(run(10, "5m")),
        "stress" => Some(run(50, "5m")),
        "spike" => Some(run(100, "1m")),
        "soak" => Some(run(10, "30m")),
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
    fn parse_duration_ms_subsecond_and_fractional() {
        assert_eq!(parse_duration_ms("500ms"), 500);
        assert_eq!(parse_duration_ms("250ms"), 250);
        assert_eq!(parse_duration_ms("1.5s"), 1500);
        assert_eq!(parse_duration_ms("0.5s"), 500);
        assert_eq!(parse_duration_ms("1m500ms"), 60_500);
        assert_eq!(parse_duration_ms("30s"), 30_000);
        assert_eq!(parse_duration_ms("1m30s"), 90_000);
        assert_eq!(parse_duration_ms("45"), 45_000);
    }

    #[test]
    fn parse_duration_ms_keeps_the_lenient_1s_floor() {
        assert_eq!(parse_duration_ms(""), 1000);
        assert_eq!(parse_duration_ms("not-a-duration"), 1000);
        assert_eq!(parse_duration_ms("0s"), 1000);
        assert_eq!(parse_duration_ms("0ms"), 1000);
    }

    #[test]
    fn parse_duration_secs_truncates_subsecond_to_the_1s_minimum() {
        assert_eq!(parse_duration_secs("500ms"), 1);
        assert_eq!(parse_duration_secs("1.5s"), 1);
        assert_eq!(parse_duration_secs("2500ms"), 2);
    }

    #[test]
    fn strict_ms_parser_accepts_subsecond_forms() {
        assert_eq!(parse_duration_ms_strict("500ms").unwrap(), 500);
        assert_eq!(parse_duration_ms_strict("1.5s").unwrap(), 1500);
        assert_eq!(parse_duration_ms_strict("30s").unwrap(), 30_000);
        assert_eq!(parse_duration_ms_strict("1m30s").unwrap(), 90_000);
        assert_eq!(parse_duration_ms_strict("45").unwrap(), 45_000);
    }

    #[test]
    fn strict_ms_parser_rejects_garbage_and_zero() {
        for bad in ["", "0s", "0ms", "0", "s", "not-a-duration", "1h30x", "10 s"] {
            assert!(
                parse_duration_ms_strict(bad).is_err(),
                "'{bad}' must be rejected"
            );
        }
    }

    #[test]
    fn strict_secs_parser_rounds_subsecond_up() {
        assert_eq!(parse_duration_secs_strict("500ms").unwrap(), 1);
        assert_eq!(parse_duration_secs_strict("1500ms").unwrap(), 2);
    }

    #[test]
    fn strict_duration_parser_accepts_valid_forms() {
        assert_eq!(parse_duration_secs_strict("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs_strict("1m").unwrap(), 60);
        assert_eq!(parse_duration_secs_strict("1m30s").unwrap(), 90);
        assert_eq!(parse_duration_secs_strict("1h").unwrap(), 3600);
        assert_eq!(parse_duration_secs_strict("45").unwrap(), 45);
    }

    #[test]
    fn strict_duration_parser_rejects_garbage_and_zero() {
        for bad in ["", "0s", "0", "s", "not-a-duration", "1h30x", "10 s"] {
            assert!(
                parse_duration_secs_strict(bad).is_err(),
                "'{bad}' must be rejected"
            );
        }
    }

    #[test]
    fn run_config_default_has_no_load_profile() {
        let cfg = RunConfig::default();
        assert!(cfg.stages.is_empty());
        assert!(cfg.arrival.is_none());
        // …and the fields are wire-compatible: absent keys deserialize, and
        // empty profiles never serialize (perfscaled embeds RunConfig).
        let cfg: RunConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.stages.is_empty() && cfg.arrival.is_none());
        let json = serde_json::to_value(RunConfig::default()).unwrap();
        assert!(json.get("stages").is_none() && json.get("arrival").is_none());
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
            ..Default::default()
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
        // File and process actions are fail-closed, and fs_root is never
        // wire-settable.
        assert!(!cfg.allow_file_actions);
        assert!(!cfg.allow_process_actions);
        assert!(cfg.fs_root.is_none());
    }

    #[test]
    fn run_config_fs_root_is_not_deserializable() {
        // A client must not pick its own sandbox root — the field is
        // `#[serde(skip)]` and stays `None` even when present in the input.
        let cfg: RunConfig =
            serde_json::from_str(r#"{ "fs_root": "/", "allow_file_actions": true }"#).unwrap();
        assert!(cfg.allow_file_actions);
        assert!(cfg.fs_root.is_none());
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
