//! Test fixture (RFC 005 phase 3.5): a library built against the **legacy**
//! WIT 0.1 ABI (`wit/v0.1/library.wit` — the call context has no
//! `settings-json`). The host must keep loading 0.1.x components; this crate
//! deliberately does NOT use `perfscale-library-sdk` (which targets the
//! current ABI) and generates its guest bindings straight from the frozen
//! 0.1 contract.
//!
//! Build: `cargo build --release --target wasm32-wasip2`.

mod bindings {
    wit_bindgen::generate!({
        path: "../../../../wit/v0.1",
        world: "perfscale-library",
        pub_export_macro: true,
        default_bindings_module: "bindings",
    });
}

use bindings::exports::perfscale::library::library::{Context, Guest};

struct Hello01;

impl Guest for Hello01 {
    fn info() -> String {
        serde_json::json!({
            "name": "hello01",
            "version": env!("CARGO_PKG_VERSION"),
            "functions": [
                { "name": "greet", "description": "Greeting for a name", "secret": false },
                { "name": "token", "description": "Fixed test token", "secret": true },
            ],
        })
        .to_string()
    }

    fn init(config_json: String) -> Result<(), String> {
        // Accept any config, like the SDK default.
        if !config_json.trim().is_empty() {
            let _: serde_json::Value = serde_json::from_str(&config_json)
                .map_err(|e| format!("init: invalid config JSON: {e}"))?;
        }
        Ok(())
    }

    fn call(ctx: Context, func: String, args_json: String) -> Result<String, String> {
        let args: serde_json::Value = serde_json::from_str(&args_json)
            .map_err(|e| format!("{func}: invalid args JSON: {e}"))?;
        match func.as_str() {
            "greet" => {
                let name = args
                    .get(0)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "greet: argument 1 must be a string".to_string())?;
                Ok(format!("hello, {name}! (vu {}, seq {})", ctx.vu_id, ctx.message_seq))
            }
            "token" => Ok("fixed-0.1-token".to_string()),
            other => Err(format!("hello01.{other}: unknown function — exports: greet, token")),
        }
    }
}

bindings::export!(Hello01 with_types_in bindings);
