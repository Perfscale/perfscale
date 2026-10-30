//! Example perfscale WASM **step** component (RFC 009) — the engine's test
//! fixture for the `perfscale-steps` world. Steps-only: `info()` lists
//! `functions: []` and five steps exercising the result contract (success +
//! output + logs + metrics + http_sample, guest failure, malformed JSON,
//! reserved metric names, secret outputs, ctx echo).

mod bindings {
    wit_bindgen::generate!({
        path: "../../../../wit/step",
        world: "perfscale-steps",
    });
}

use bindings::exports::perfscale::library::library::{Context as LibCtx, Guest as LibraryGuest};
use bindings::exports::perfscale::step::step::{Guest as StepGuest, StepContext};

struct Steps;

impl LibraryGuest for Steps {
    fn info() -> String {
        serde_json::json!({
            "name": "steps",
            "version": "0.1.0",
            "pure": true,
            "functions": [],
            "steps": [
                { "name": "echo", "description": "Echo the params; emit logs, a counter and a histogram" },
                { "name": "work", "description": "Emit an http_sample plus metrics" },
                { "name": "fail", "description": "A step that reports failure" },
                { "name": "err", "description": "A step whose run() returns Err" },
                { "name": "ctx", "description": "Echo the step context the host passed" },
                { "name": "secret", "description": "Mint a session token marked secret" },
                { "name": "badjson", "description": "Return a payload that is not result JSON" },
                { "name": "reservedmetrics", "description": "Try to write an engine-owned metric" },
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

impl StepGuest for Steps {
    fn run(ctx: StepContext, step_id: String, params_json: String) -> Result<String, String> {
        let params: serde_json::Value =
            serde_json::from_str(&params_json).map_err(|e| format!("bad params JSON: {e}"))?;
        match step_id.as_str() {
            "echo" => Ok(serde_json::json!({
                "success": true,
                "output": { "echo": params },
                "logs": [
                    { "tag": "out", "line": "echo ran" },
                    { "tag": "err", "line": "echo warning" },
                    { "tag": "sys", "line": "echo debug" }
                ],
                "metrics": { "steps_echo_total": 1, "steps_echo_duration": [1.5] }
            })
            .to_string()),
            "work" => Ok(serde_json::json!({
                "success": true,
                "output": { "done": true },
                "metrics": { "steps_work_total": 1, "steps_work_duration": [2.0, 3.0] },
                "http_sample": { "duration_ms": 12.5, "status": 200, "failed": false }
            })
            .to_string()),
            "fail" => Ok(serde_json::json!({
                "success": false,
                "output": null,
                "logs": [{ "tag": "err", "line": "boom" }]
            })
            .to_string()),
            "err" => Err("guest-reported error".into()),
            "ctx" => Ok(serde_json::json!({
                "success": true,
                "output": {
                    "vu": ctx.vu_id,
                    "iteration": ctx.iteration_seq,
                    "step_name": ctx.step_name,
                    "has_settings": !ctx.settings_json.is_empty(),
                }
            })
            .to_string()),
            "secret" => Ok(serde_json::json!({
                "success": true,
                "output": { "session": "s3cr3t-t0ken" },
                "secret_output_paths": ["session"]
            })
            .to_string()),
            "badjson" => Ok("this is not result JSON".into()),
            "reservedmetrics" => Ok(serde_json::json!({
                "success": true,
                "metrics": { "http_req_duration": [1.0] }
            })
            .to_string()),
            other => Err(format!("unknown step '{other}'")),
        }
    }
}

bindings::export!(Steps with_types_in bindings);
