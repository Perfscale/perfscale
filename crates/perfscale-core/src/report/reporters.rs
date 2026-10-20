//! End-of-run reporter configuration (`report.reporters`, issue #9).
//!
//! Reporters use the same `use:`/`with:` shape as steps: `use` names a
//! built-in renderer (`std/report/json@v1`, `std/report/junit@v1`,
//! `std/report/html@v1`) or a WASM
//! reporter component (`wasm:./reporter.wasm`, see `crate::report::wasm` and
//! `wit/reporter.wit`). For built-ins, `with` picks the destination — a
//! local file (`path`) or an HTTP push (`url`, with optional `headers` whose
//! values may reference environment variables as `${VAR}`; the references
//! are interpolated host-side and never logged or embedded into the report).
//! WASM reporters ignore `path`/`url`: they write through the host's
//! `emit-file` into `with.output` (default `./perfscale-report-<module>`)
//! and receive the free-form `with.config` block as their config.
//!
//! Configuration errors (unknown reporter, missing destination, a `${VAR}`
//! reference to an unset variable, an unloadable WASM module) are fail-fast:
//! lint flags them and the run refuses to start ([`validate_reporters`]).
//! Runtime delivery errors are best-effort — logged and counted, never
//! run-killing.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::metrics::MAX_STORED_STEP_SAMPLES;

/// One configured end-of-run reporter (`report.reporters[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReporterConfig {
    /// Reporter id: `std/report/json@v1`, `std/report/junit@v1`, or
    /// `std/report/html@v1`.
    #[serde(rename = "use")]
    pub use_: String,

    /// Reporter parameters (destination + knobs).
    #[serde(default)]
    pub with: ReporterWith,
}

/// Reporter parameters. Exactly one of `path` / `url` is required.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReporterWith {
    /// Write the report to this local file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// POST the report to this URL (HTTP push).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Extra headers for the HTTP push. Values may embed `${VAR}`
    /// environment references, e.g. `Authorization: "Bearer ${TOKEN}"` —
    /// interpolated host-side at delivery, never logged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    /// Raw per-step samples to include (default
    /// [`MAX_STORED_STEP_SAMPLES`], `0` disables the samples block).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_samples: Option<usize>,

    /// WASM reporters only: output directory for the guest's `emit-file`
    /// writes. Default `./perfscale-report-<module-stem>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,

    /// WASM reporters only: free-form config handed to the guest's
    /// `on-report` verbatim (the `config-json` argument).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

impl ReporterWith {
    /// The raw-sample cap for this reporter.
    pub fn max_samples(&self) -> usize {
        self.max_samples.unwrap_or(MAX_STORED_STEP_SAMPLES)
    }
}

/// The built-in reporters shipped in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReporterKind {
    Json,
    Junit,
    Html,
}

/// Map a reporter id to a built-in. Accepts the full id
/// (`std/report/json@v1`) and the CLI shorthand (`json`).
pub fn reporter_kind(use_: &str) -> Option<ReporterKind> {
    match use_ {
        "std/report/json@v1" | "json" => Some(ReporterKind::Json),
        "std/report/junit@v1" | "junit" => Some(ReporterKind::Junit),
        "std/report/html@v1" | "html" => Some(ReporterKind::Html),
        _ => None,
    }
}

/// The module path of a WASM reporter id (`wasm:./path/to/reporter.wasm`),
/// or `None` when `use_` does not name a WASM reporter.
pub fn wasm_reporter_module(use_: &str) -> Option<&str> {
    use_.strip_prefix("wasm:").filter(|p| !p.is_empty())
}

/// All `${VAR}` references in `s`, in order of appearance.
fn env_refs(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        rest = &rest[start + 2..];
        match rest.find('}') {
            Some(end)
                if end > 0
                    && rest[..end]
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                out.push(&rest[..end]);
                rest = &rest[end + 1..];
            }
            _ => break,
        }
    }
    out
}

/// Interpolate `${VAR}` references in a header value against the process
/// environment. The interpolated value is a secret: callers must never log
/// it.
pub fn resolve_env_refs(s: &str) -> Result<String, String> {
    let mut out = s.to_string();
    for var in env_refs(s) {
        let value = std::env::var(var)
            .map_err(|_| format!("environment variable ${{{var}}} is not set"))?;
        out = out.replace(&format!("${{{var}}}"), &value);
    }
    Ok(out)
}

/// Fail-fast validation of a reporters list — run at lint time and before
/// the run starts. Errors are plain messages; the callers wrap them with
/// location hints.
pub fn validate_reporters(reporters: &[ReporterConfig]) -> Result<(), String> {
    for r in reporters {
        if let Some(module) = wasm_reporter_module(&r.use_) {
            validate_wasm_reporter(r, module)?;
        } else {
            if reporter_kind(&r.use_).is_none() {
                return Err(format!(
                    "unknown reporter '{}' — built-ins: std/report/json@v1, std/report/junit@v1, std/report/html@v1; or a WASM reporter: wasm:./path/to/reporter.wasm",
                    r.use_
                ));
            }
            match (&r.with.path, &r.with.url) {
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "reporter '{}': `with.path` and `with.url` are mutually exclusive",
                        r.use_
                    ))
                }
                (None, None) => {
                    return Err(format!(
                    "reporter '{}' needs a destination: `with.path` (file) or `with.url` (HTTP push)",
                    r.use_
                ))
                }
                _ => {}
            }
        }
        for (k, v) in &r.with.headers {
            for var in env_refs(v) {
                if std::env::var_os(var).is_none() {
                    return Err(format!(
                        "reporter '{r}' header '{k}' references ${{{var}}}, which is not set",
                        r = r.use_
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Fail-fast checks for a `wasm:` reporter: `path`/`url` do not apply (the
/// guest writes via `emit-file`), and the module must exist, compile and
/// export the `perfscale:reporter` world before the run starts.
fn validate_wasm_reporter(r: &ReporterConfig, module: &str) -> Result<(), String> {
    if r.with.path.is_some() || r.with.url.is_some() {
        return Err(format!(
            "reporter '{}': `with.path`/`with.url` do not apply to WASM reporters — the guest writes through emit-file into `with.output`",
            r.use_
        ));
    }
    wasm_module_check(&r.use_, module)
}

/// The module exists, compiles and exports the `perfscale:reporter` world.
/// Without the `wasm-libs` feature a `wasm:` reporter is unsupported.
#[cfg(feature = "wasm-libs")]
fn wasm_module_check(use_: &str, module: &str) -> Result<(), String> {
    crate::report::wasm::WasmReporter::check(std::path::Path::new(module))
        .map_err(|e| format!("reporter '{use_}': {e}"))
}

/// The module exists, compiles and exports the `perfscale:reporter` world.
/// Without the `wasm-libs` feature a `wasm:` reporter is unsupported.
#[cfg(not(feature = "wasm-libs"))]
fn wasm_module_check(use_: &str, _module: &str) -> Result<(), String> {
    Err(format!(
        "reporter '{use_}': WASM reporters need the `wasm-libs` feature, which this build lacks"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_known_reporters_and_destinations() {
        let ok = vec![
            ReporterConfig {
                use_: "std/report/json@v1".into(),
                with: ReporterWith {
                    path: Some("out.json".into()),
                    ..ReporterWith::default()
                },
            },
            ReporterConfig {
                use_: "junit".into(),
                with: ReporterWith {
                    url: Some("http://localhost:9000/reports".into()),
                    ..ReporterWith::default()
                },
            },
            ReporterConfig {
                use_: "std/report/html@v1".into(),
                with: ReporterWith {
                    path: Some("out.html".into()),
                    ..ReporterWith::default()
                },
            },
        ];
        assert!(validate_reporters(&ok).is_ok());

        let bad_use = vec![ReporterConfig {
            use_: "xml".into(),
            with: ReporterWith {
                path: Some("out.xml".into()),
                ..ReporterWith::default()
            },
        }];
        let err = validate_reporters(&bad_use).unwrap_err();
        assert!(err.contains("unknown reporter 'xml'"), "{err}");

        let no_dest = vec![ReporterConfig {
            use_: "std/report/json@v1".into(),
            with: ReporterWith::default(),
        }];
        let err = validate_reporters(&no_dest).unwrap_err();
        assert!(err.contains("needs a destination"), "{err}");

        let both = vec![ReporterConfig {
            use_: "std/report/json@v1".into(),
            with: ReporterWith {
                path: Some("a".into()),
                url: Some("http://x".into()),
                ..ReporterWith::default()
            },
        }];
        let err = validate_reporters(&both).unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn env_refs_resolve_host_side_and_missing_vars_fail() {
        std::env::set_var("PERFSCALE_TEST_TOKEN", "s3cret");
        assert_eq!(
            resolve_env_refs("Bearer ${PERFSCALE_TEST_TOKEN}").unwrap(),
            "Bearer s3cret"
        );
        assert!(resolve_env_refs("Bearer ${PERFSCALE_UNSET_VAR_XYZ}").is_err());

        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            "Bearer ${PERFSCALE_UNSET_VAR_XYZ}".to_string(),
        );
        let r = vec![ReporterConfig {
            use_: "std/report/json@v1".into(),
            with: ReporterWith {
                url: Some("http://x".into()),
                headers,
                ..ReporterWith::default()
            },
        }];
        let err = validate_reporters(&r).unwrap_err();
        assert!(err.contains("PERFSCALE_UNSET_VAR_XYZ"), "{err}");
    }

    #[test]
    fn wasm_reporter_ids_parse_and_validate_the_module() {
        assert_eq!(
            wasm_reporter_module("wasm:./out/r.wasm"),
            Some("./out/r.wasm")
        );
        assert_eq!(wasm_reporter_module("wasm:"), None);
        assert_eq!(wasm_reporter_module("json"), None);

        // The module must exist, compile and export the reporter world
        // (builds without `wasm-libs` reject the reporter outright).
        let r = vec![ReporterConfig {
            use_: "wasm:/definitely/missing.wasm".into(),
            with: ReporterWith::default(),
        }];
        let err = validate_reporters(&r).unwrap_err();
        #[cfg(feature = "wasm-libs")]
        assert!(err.contains("failed to read"), "{err}");
        #[cfg(not(feature = "wasm-libs"))]
        assert!(err.contains("wasm-libs"), "{err}");

        // `path`/`url` are built-in destinations; they do not apply to wasm.
        let r = vec![ReporterConfig {
            use_: "wasm:./r.wasm".into(),
            with: ReporterWith {
                path: Some("out.txt".into()),
                ..ReporterWith::default()
            },
        }];
        let err = validate_reporters(&r).unwrap_err();
        assert!(err.contains("do not apply to WASM reporters"), "{err}");

        // A wasm reporter needs no destination and passes the dest check.
        let r = vec![ReporterConfig {
            use_: "wasm:./r.wasm".into(),
            with: ReporterWith {
                output: Some("out".into()),
                config: Some(serde_json::json!({"style": "short"})),
                ..ReporterWith::default()
            },
        }];
        let err = validate_reporters(&r).unwrap_err();
        assert!(!err.contains("needs a destination"), "{err}");
    }
}
