//! Example perfscale WASM library with a config extension (issue #10) —
//! also the engine's test fixture for config-schema probing and validation.
//!
//! Implements `perfscale:library@0.3.0` (`wit/library.wit`, world
//! `perfscale-library-config`): the library claims the top-level config key
//! `configlib` with a small JSON Schema, and its `endpoint()` function reads
//! the validated block from the run settings' `extensions` object.

mod bindings {
    wit_bindgen::generate!({
        path: "../../../../wit",
        world: "perfscale-library-config",
    });
}

use bindings::exports::perfscale::library::config_schema::Guest as ConfigSchemaGuest;
use bindings::exports::perfscale::library::library::{Context, Guest as LibraryGuest};

struct ConfigLib;

impl LibraryGuest for ConfigLib {
    fn info() -> String {
        serde_json::json!({
            "name": "configlib",
            "version": "0.1.0",
            "pure": true,
            "functions": [{ "name": "endpoint", "description": "The configured endpoint", "secret": false }],
        })
        .to_string()
    }

    fn init(_config_json: String) -> Result<(), String> {
        Ok(())
    }

    fn call(ctx: Context, func_name: String, _args_json: String) -> Result<String, String> {
        match func_name.as_str() {
            "endpoint" => {
                let settings: serde_json::Value = serde_json::from_str(&ctx.settings_json)
                    .map_err(|e| format!("invalid settings JSON: {e}"))?;
                Ok(settings
                    .pointer("/extensions/configlib/endpoint")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("https://localhost")
                    .to_string())
            }
            other => Err(format!("unknown function '{other}'")),
        }
    }
}

impl ConfigSchemaGuest for ConfigLib {
    fn config_schema() -> String {
        serde_json::json!({
            "key": "configlib",
            "schema": {
                "type": "object",
                "properties": {
                    "endpoint": { "type": "string" },
                    "pool_size": { "type": "integer", "minimum": 1 }
                },
                "required": ["endpoint"],
                "additionalProperties": false
            }
        })
        .to_string()
    }
}

bindings::export!(ConfigLib with_types_in bindings);
