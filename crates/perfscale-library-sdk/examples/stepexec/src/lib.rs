//! Example perfscale WASM step component using the `process` capability
//! (RFC 009, world `perfscale-steps-exec`): the `run` step executes a command
//! through the host-mediated `host-exec` import and returns its outcome.
//!
//! Fixture for the engine's process-capability tests — the component imports
//! `perfscale:step/host-exec`, so it only loads with `capabilities:
//! [process]` AND `allow_process_actions: true`.

mod bindings {
    wit_bindgen::generate!({
        path: "../../../../wit/step",
        world: "perfscale-steps-exec",
    });
}

use bindings::exports::perfscale::library::library::{Context as LibCtx, Guest as LibraryGuest};
use bindings::exports::perfscale::step::step::{Guest as StepGuest, StepContext};
use bindings::perfscale::step::host_exec::{self, ExecRequest};

struct StepExec;

impl LibraryGuest for StepExec {
    fn info() -> String {
        serde_json::json!({
            "name": "stepexec",
            "version": "0.1.0",
            "functions": [],
            "steps": [
                { "name": "run", "description": "Run { executable, argv?, env?, stdin?, timeout_ms? } via host-exec" },
            ],
        })
        .to_string()
    }

    fn init(_config_json: String) -> Result<(), String> {
        Ok(())
    }

    fn call(_ctx: LibCtx, func_name: String, _args_json: String) -> Result<String, String> {
        Err(format!(
            "unknown function '{func_name}' — this component exports no functions"
        ))
    }
}

impl StepGuest for StepExec {
    fn run(_ctx: StepContext, step_id: String, params_json: String) -> Result<String, String> {
        let params: serde_json::Value =
            serde_json::from_str(&params_json).map_err(|e| format!("bad params JSON: {e}"))?;
        if step_id != "run" {
            return Err(format!("unknown step '{step_id}'"));
        }
        let executable = params
            .get("executable")
            .and_then(|v| v.as_str())
            .ok_or("params.executable is required")?
            .to_string();
        let strings = |key: &str| -> Vec<String> {
            params
                .get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let env = params
            .get("env")
            .and_then(|v| v.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let request = ExecRequest {
            executable,
            argv: strings("argv"),
            env,
            cwd: params
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            stdin: params
                .get("stdin")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            timeout_ms: params.get("timeout_ms").and_then(|v| v.as_u64()),
        };
        match host_exec::exec(&request) {
            Ok(r) => Ok(serde_json::json!({
                "success": r.exit_code == 0,
                "output": {
                    "exit_code": r.exit_code,
                    "stdout": r.stdout,
                    "stderr": r.stderr,
                }
            })
            .to_string()),
            Err(e) => Ok(serde_json::json!({
                "success": false,
                "logs": [{ "tag": "err", "line": e }],
            })
            .to_string()),
        }
    }
}

bindings::export!(StepExec with_types_in bindings);
