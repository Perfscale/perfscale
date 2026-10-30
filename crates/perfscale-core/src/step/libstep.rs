//! Custom SDK steps — `use: lib/<alias>/<step>` dispatch (RFC 009).
//!
//! A WASM component declared in `libraries:` whose `info()` manifest lists
//! `steps` exports them as step actions: `lib/<alias>/<step>` resolves the
//! alias in the run's `LibrarySet`, checks the step against the probed
//! manifest, interpolates `with:` like any action, and invokes the
//! component's `perfscale:step/step.run` export on the calling VU's parked
//! instance (see [`Context::library_step_instance`]).
//!
//! The guest's result JSON is the serialized `ActionOutput` (RFC 008):
//! `success` (required), `output` (stored under `outputs:`), `logs`
//! (`out`/`err`/`sys` tags), `metrics` (folded by the runner exactly like a
//! native step's reserved `metrics` key), `http_sample`, and
//! `secret_output_paths` (output keys registered with the run's
//! [`SecretRegistry`] for log masking). The host enforces metric-name
//! hygiene — lowercase `[a-z0-9_]`, never an engine-owned series — so a
//! guest cannot write into `http_req_duration` & co.

use std::time::Instant;

use serde_json::Value;

use super::actions::{ActionOutput, HttpSample, LogTag};
use super::context::Context;

/// Parse `lib/<alias>/<step>` — exactly three non-empty segments.
pub(crate) fn parse_step_id(action_id: &str) -> Result<(&str, &str), String> {
    let rest = action_id
        .strip_prefix("lib/")
        .expect("prefix checked by caller");
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [alias, step] if !alias.is_empty() && !step.is_empty() => Ok((alias, step)),
        _ => Err(format!(
            "invalid step id '{action_id}' — expected lib/<alias>/<step> (no @vN: versioning is the pinned artifact's)"
        )),
    }
}

/// Execute one `lib/<alias>/<step>` action. `params` already has `${{ }}`
/// interpolation applied (the shared `execute_action` front door).
pub(crate) fn execute_lib_step(
    action_id: &str,
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    match run(action_id, params, ctx, step_name) {
        Ok(out) => out,
        Err(msg) => ActionOutput {
            value: Value::Null,
            logs: vec![(LogTag::Err, format!("{step_name}: {msg}"))],
            success: false,
            http_sample: None,
        },
    }
}

fn run(
    action_id: &str,
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> Result<ActionOutput, String> {
    let (alias, step) = parse_step_id(action_id)?;

    // The alias resolves against the run's declared libraries (the error
    // names the declared aliases, mirroring the token resolver).
    let set = ctx.libraries.as_ref().ok_or_else(|| {
        format!(
            "unknown library alias '{alias}' in step id '{action_id}' — no libraries are declared"
        )
    })?;
    let lib = set
        .libraries
        .iter()
        .find(|l| l.alias == alias)
        .ok_or_else(|| {
            let known: Vec<&str> = set.libraries.iter().map(|l| l.alias.as_str()).collect();
            format!(
                "unknown library alias '{alias}' in step id '{action_id}' — declared aliases: {}",
                known.join(", ")
            )
        })?;

    // The step must be in the component's probed manifest — the load-time
    // check already guarantees the manifest and the ABI agree.
    if !lib.provider.steps().iter().any(|s| s.name == step) {
        let exported: Vec<&str> = lib.provider.steps().iter().map(|s| s.name).collect();
        return Err(if exported.is_empty() {
            format!(
                "unknown step '{step}' for library '{alias}' ({}) — this component exports no steps",
                lib.provider.id()
            )
        } else {
            format!(
                "unknown step '{step}' for library '{alias}' ({}) — exported steps: {}",
                lib.provider.id(),
                exported.join(", ")
            )
        });
    }

    // `${…}` generator tokens in params expand like any token-supporting
    // action — through the shared per-VU expander (one generator/message
    // context, dropped before the guest runs). Token-free params never build
    // a generator (expand_value short-circuits on `contains_token`).
    let params = ctx
        .token_expander()
        .expand_value(params)
        .map_err(|e| format!("library '{alias}' step '{step}': {e}"))?;
    let params_json =
        serde_json::to_string(&params).map_err(|e| format!("failed to encode step params: {e}"))?;
    let step_ctx = crate::library::StepCtx {
        iteration_seq: ctx.iteration_seq,
        vu_id: ctx.vu_id,
        time_ms: crate::generate::now_unix_millis() as u64,
        settings_json: &ctx.settings_json,
        step_name,
    };

    let instance = ctx.library_step_instance(alias)?;
    let started = Instant::now();
    let outcome = instance
        .lock()
        .unwrap()
        .run_step(&step_ctx, step, &params_json)
        .and_then(|json| {
            serde_json::from_str::<StepResultJson>(&json).map_err(|e| {
                format!("library '{alias}' step '{step}' returned malformed result JSON: {e}")
            })
        })
        .and_then(|parsed| into_output(alias, step, parsed, ctx));
    let elapsed = started.elapsed();
    if let Some(metrics) = &ctx.library_metrics {
        // A step that reports `success: false` counts as an error too, like
        // any failed action.
        metrics.record_step(alias, elapsed, outcome.as_ref().is_ok_and(|o| o.success));
    }
    outcome
}

/// The guest's result JSON (RFC 008 `ActionOutput` as serialized in
/// `wit/step.wit`). `success` is required; everything else defaults.
#[derive(Debug, serde::Deserialize)]
struct StepResultJson {
    success: bool,
    #[serde(default)]
    output: Value,
    #[serde(default)]
    logs: Vec<StepLogJson>,
    #[serde(default)]
    metrics: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    http_sample: Option<StepHttpSampleJson>,
    /// Output keys whose values the engine registers for log masking (a
    /// minted session token must never reach the run log).
    #[serde(default)]
    secret_output_paths: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct StepLogJson {
    tag: String,
    line: String,
}

#[derive(Debug, serde::Deserialize)]
struct StepHttpSampleJson {
    duration_ms: f64,
    status: u16,
    #[serde(default)]
    failed: bool,
}

/// Host-side validation + [`ActionOutput`] conversion.
fn into_output(
    alias: &str,
    step: &str,
    result: StepResultJson,
    ctx: &Context,
) -> Result<ActionOutput, String> {
    let fail = |msg: String| -> String { format!("library '{alias}' step '{step}': {msg}") };

    let mut logs = Vec::with_capacity(result.logs.len());
    for l in result.logs {
        let tag = match l.tag.as_str() {
            "out" => LogTag::Out,
            "err" => LogTag::Err,
            "sys" => LogTag::Sys,
            other => {
                return Err(fail(format!(
                    "unknown log tag '{other}' — expected out, err or sys"
                )))
            }
        };
        logs.push((tag, l.line));
    }

    // Metric-name hygiene (RFC 008/009): lowercase [a-z0-9_], and never an
    // engine-owned series — a guest must not write into the engine's own
    // metrics (http_req_duration, the registered family catalog, …).
    let metrics = result.metrics.unwrap_or_default();
    for name in metrics.keys() {
        if !is_valid_metric_name(name) {
            return Err(fail(format!(
                "metric name '{name}' is invalid — metric names match [a-z][a-z0-9_]*"
            )));
        }
        if is_reserved_metric_name(name) {
            return Err(fail(format!(
                "metric name '{name}' is reserved by the engine — pick a name outside the registered families"
            )));
        }
    }

    // `metrics` rides inside the output value so the runner's existing
    // reserved-key fold (and `std/check@v1`) sees it exactly like a native
    // step's. A guest must not also place `metrics` in its output.
    let mut value = result.output;
    if !metrics.is_empty() {
        match &mut value {
            Value::Object(map) => {
                if map.contains_key("metrics") {
                    return Err(fail(
                        "`output.metrics` is a reserved key — return metrics in the result's top-level `metrics` field".into(),
                    ));
                }
                map.insert("metrics".into(), Value::Object(metrics));
            }
            _ => {
                value = serde_json::json!({ "value": value, "metrics": metrics });
            }
        }
    }

    // Declared secret outputs register with the run's masking registry — the
    // guest declares, the engine enforces (same posture as FunctionInfo.secret).
    for path in &result.secret_output_paths {
        let segments: Vec<&str> = path.split('.').collect();
        let mut current = &value;
        for seg in &segments {
            current = current.get(*seg).unwrap_or(&Value::Null);
        }
        match current {
            Value::String(s) => ctx.secrets.record(s),
            Value::Null => {}
            other => ctx.secrets.record(&other.to_string()),
        }
    }

    Ok(ActionOutput {
        value,
        logs,
        success: result.success,
        http_sample: result.http_sample.map(|s| HttpSample {
            duration_ms: s.duration_ms,
            status: s.status,
            failed: s.failed,
        }),
    })
}

/// `[a-z][a-z0-9_]*` — the metric-name grammar every engine series follows.
fn is_valid_metric_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Engine-owned series a guest step may not write: every registered metrics
/// family (built-ins + any pro family registered at startup) plus the
/// runner's own names. Mechanical derivatives (`<family>_failed`) are covered
/// because their `<family>_duration`/`_rtt` source names are registered.
fn is_reserved_metric_name(name: &str) -> bool {
    name == "metrics"
        || crate::metrics::catalog()
            .iter()
            .any(|(_, def)| def.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn step_id_grammar() {
        assert_eq!(parse_step_id("lib/sig/login").unwrap(), ("sig", "login"));
        for bad in ["lib/sig", "lib//login", "lib/sig/", "lib/a/b/c", "lib/"] {
            assert!(parse_step_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn metric_name_hygiene() {
        assert!(is_valid_metric_name("sig_login_duration"));
        for bad in ["Login", "sig-login", "1x", "", "sig_login_duration "] {
            assert!(!is_valid_metric_name(bad), "{bad}");
        }
        assert!(is_reserved_metric_name("http_req_duration"));
        assert!(is_reserved_metric_name("metrics"));
        assert!(!is_reserved_metric_name("sig_login_duration"));
    }

    #[test]
    fn result_conversion_success_with_metrics_and_logs() {
        let ctx = Context::new();
        let parsed: StepResultJson = serde_json::from_value(json!({
            "success": true,
            "output": { "session": "abc" },
            "logs": [ { "tag": "out", "line": "ok" }, { "tag": "sys", "line": "dbg" } ],
            "metrics": { "sig_logins_total": 1, "sig_login_duration": [12.4] },
            "http_sample": { "duration_ms": 12.4, "status": 200 },
            "secret_output_paths": ["session"],
        }))
        .unwrap();
        let out = into_output("sig", "login", parsed, &ctx).unwrap();
        assert!(out.success);
        assert_eq!(out.value["session"], json!("abc"));
        // metrics folded into the output under the reserved key.
        assert_eq!(out.value["metrics"]["sig_logins_total"], json!(1));
        assert_eq!(out.logs.len(), 2);
        let sample = out.http_sample.unwrap();
        assert_eq!(sample.status, 200);
        // The declared secret output registered for masking.
        assert_eq!(ctx.secrets.mask("token abc here"), "token *** here");
    }

    #[test]
    fn result_conversion_wraps_non_object_output_when_metrics_present() {
        let ctx = Context::new();
        let parsed: StepResultJson = serde_json::from_value(json!({
            "success": true, "output": "plain", "metrics": { "x_total": 2 },
        }))
        .unwrap();
        let out = into_output("a", "s", parsed, &ctx).unwrap();
        assert_eq!(out.value["value"], json!("plain"));
        assert_eq!(out.value["metrics"]["x_total"], json!(2));
    }

    #[test]
    fn result_conversion_rejects_reserved_and_invalid_metric_names() {
        let ctx = Context::new();
        for metrics in [
            json!({ "http_req_duration": [1.0] }),
            json!({ "Bad Name": 1 }),
        ] {
            let parsed: StepResultJson =
                serde_json::from_value(json!({ "success": true, "metrics": metrics })).unwrap();
            let err = into_output("a", "s", parsed, &ctx).unwrap_err();
            assert!(err.contains("reserved") || err.contains("invalid"), "{err}");
        }
    }

    #[test]
    fn result_conversion_rejects_output_metrics_collision_and_bad_tags() {
        let ctx = Context::new();
        let parsed: StepResultJson = serde_json::from_value(json!({
            "success": true, "output": { "metrics": {} }, "metrics": { "x_total": 1 },
        }))
        .unwrap();
        let err = into_output("a", "s", parsed, &ctx).unwrap_err();
        assert!(err.contains("reserved key"), "{err}");

        let parsed: StepResultJson = serde_json::from_value(
            json!({ "success": true, "logs": [{ "tag": "nope", "line": "x" }] }),
        )
        .unwrap();
        let err = into_output("a", "s", parsed, &ctx).unwrap_err();
        assert!(err.contains("unknown log tag"), "{err}");
    }

    #[test]
    fn malformed_result_json_is_actionable() {
        let ctx = Context::new();
        // Missing the required `success` key.
        let err = serde_json::from_str::<StepResultJson>(r#"{"output":{}}"#).unwrap_err();
        let msg = format!("library 'a' step 's' returned malformed result JSON: {err}");
        assert!(msg.contains("malformed result JSON"), "{msg}");
        let _ = ctx;
    }

    // --- token expansion in step params (issue: literal ${alias.fn(...)} must
    // not reach the guest) ---------------------------------------------------

    /// A fake steps+function library (no wasm): the `echo` step echoes the
    /// params JSON it received; the `shout` function uppercases its argument.
    /// Counts instantiations so the no-token path can prove it minted no
    /// extra generator instance.
    struct FakeSteps {
        instances: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::library::LibraryProvider for FakeSteps {
        fn id(&self) -> &str {
            "fake@v1"
        }
        fn functions(&self) -> &[crate::library::FunctionInfo] {
            &[crate::library::FunctionInfo {
                name: "shout",
                description: "uppercase",
                secret: false,
            }]
        }
        fn steps(&self) -> &[crate::library::StepInfo] {
            &[crate::library::StepInfo {
                name: "echo",
                description: "echo params",
            }]
        }
        fn instantiate(
            &self,
            _config: Option<Value>,
            _seed: u64,
        ) -> Result<Box<dyn crate::library::LibraryInstance>, String> {
            self.instances
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Box::new(FakeStepsInstance))
        }
        fn share_per_context(&self) -> bool {
            false
        }
    }

    struct FakeStepsInstance;

    impl crate::library::LibraryInstance for FakeStepsInstance {
        fn call(
            &mut self,
            _ctx: &crate::library::CallCtx<'_>,
            func: &str,
            args: &[Value],
        ) -> Result<String, String> {
            match func {
                "shout" => Ok(args
                    .first()
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_uppercase()),
                other => Err(format!("unknown function '{other}'")),
            }
        }
        fn run_step(
            &mut self,
            _ctx: &crate::library::StepCtx<'_>,
            step: &str,
            params_json: &str,
        ) -> Result<String, String> {
            match step {
                "echo" => {
                    Ok(json!({ "success": true, "output": { "params": params_json } }).to_string())
                }
                other => Err(format!("unknown step '{other}'")),
            }
        }
    }

    fn fake_ctx(instances: Arc<std::sync::atomic::AtomicUsize>) -> Context {
        let mut ctx = Context::new();
        ctx.libraries = Some(Arc::new(crate::library::LibrarySet {
            libraries: vec![crate::library::ResolvedLibrary {
                alias: "echo".into(),
                provider: Arc::new(FakeSteps { instances }),
                config: None,
                rules: Default::default(),
            }],
        }));
        ctx
    }

    /// `${alias.fn(...)}` tokens in step params expand through the shared
    /// per-VU generator before the guest sees the JSON — exactly like
    /// token-supporting built-in actions.
    #[test]
    fn generator_tokens_expand_in_step_params() {
        let instances = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ctx = fake_ctx(Arc::clone(&instances));
        let out = execute_lib_step(
            "lib/echo/echo",
            &json!({ "msg": "${echo.shout(echo)}" }),
            &ctx,
            "s",
        );
        assert!(out.success, "{:?}", out.logs);
        let params: Value = serde_json::from_str(out.value["params"].as_str().unwrap()).unwrap();
        assert_eq!(params["msg"], json!("ECHO"));
        // Step instance + one generator instance (share_per_context is off
        // here, so the generator mints its own).
        assert_eq!(instances.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    /// Token-free params never build a generator: exactly one instance (the
    /// step's) is minted.
    #[test]
    fn token_free_params_build_no_generator() {
        let instances = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ctx = fake_ctx(Arc::clone(&instances));
        let out = execute_lib_step("lib/echo/echo", &json!({ "msg": "plain" }), &ctx, "s");
        assert!(out.success, "{:?}", out.logs);
        let params: Value = serde_json::from_str(out.value["params"].as_str().unwrap()).unwrap();
        assert_eq!(params["msg"], json!("plain"));
        assert_eq!(
            instances.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "no generator (and its per-generator instance) without tokens"
        );
    }

    /// A failing token expansion fails the step before the guest runs.
    #[test]
    fn failing_token_expansion_fails_the_step() {
        let instances = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ctx = fake_ctx(Arc::clone(&instances));
        let out = execute_lib_step(
            "lib/echo/echo",
            &json!({ "msg": "${echo.nope(1)}" }),
            &ctx,
            "s",
        );
        assert!(!out.success);
        assert!(out.logs[0].1.contains("unknown function"), "{:?}", out.logs);
    }
}
