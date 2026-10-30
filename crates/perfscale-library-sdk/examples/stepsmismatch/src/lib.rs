//! Fixture: exports the `perfscale-steps` world but its `info()` manifest
//! lists NO `steps` — the engine must reject the load (manifest and ABI may
//! not disagree, RFC 009).

mod bindings {
    wit_bindgen::generate!({
        path: "../../../../wit/step",
        world: "perfscale-steps",
    });
}

use bindings::exports::perfscale::library::library::{Context as LibCtx, Guest as LibraryGuest};
use bindings::exports::perfscale::step::step::{Guest as StepGuest, StepContext};

struct Mismatch;

impl LibraryGuest for Mismatch {
    fn info() -> String {
        // A token function keeps the "no functions and no steps" rule out of
        // the way — the mismatch under test is steps-only.
        serde_json::json!({
            "name": "stepsmismatch",
            "version": "0.1.0",
            "pure": true,
            "functions": [{ "name": "ping", "description": "pong", "secret": false }],
        })
        .to_string()
    }

    fn init(_config_json: String) -> Result<(), String> {
        Ok(())
    }

    fn call(_ctx: LibCtx, func_name: String, _args_json: String) -> Result<String, String> {
        match func_name.as_str() {
            "ping" => Ok("pong".into()),
            other => Err(format!("unknown function '{other}'")),
        }
    }
}

impl StepGuest for Mismatch {
    fn run(_ctx: StepContext, step_id: String, _params_json: String) -> Result<String, String> {
        Err(format!("unknown step '{step_id}'"))
    }
}

bindings::export!(Mismatch with_types_in bindings);
