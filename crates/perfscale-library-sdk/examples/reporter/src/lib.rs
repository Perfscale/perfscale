//! Example perfscale WASM reporter (issue #9) — also the engine's test
//! fixture (`crates/perfscale-core/src/report/wasm.rs`).
//!
//! Implements `perfscale:reporter@0.1.0` (`wit/reporter.wit`): on `on-report`
//! it validates the `report@v1` document, emits the raw report plus a
//! human-readable `summary.txt` through the host's `emit-file`, and returns
//! the summary. Config knobs (the `with.config` block):
//!
//! - `{"fail": true}` — return an error after emitting (best-effort path);
//! - `{"emit_evil": true}` — attempt `emit-file` with a `../` name (the host
//!   must refuse the traversal; the guest propagates the refusal).

mod bindings {
    wit_bindgen::generate!({
        path: "../../../../wit/reporter",
        world: "perfscale-reporter",
    });
}

use bindings::exports::perfscale::reporter::reporter::Guest;
use bindings::perfscale::reporter::host;

struct ExampleReporter;

impl Guest for ExampleReporter {
    fn on_report(report_json: String, config_json: String) -> Result<String, String> {
        let report: serde_json::Value = serde_json::from_str(&report_json)
            .map_err(|e| format!("invalid report JSON: {e}"))?;
        if report["schemaVersion"] != "report@v1" {
            return Err(format!(
                "expected schemaVersion report@v1, got {}",
                report["schemaVersion"]
            ));
        }
        let config: serde_json::Value = serde_json::from_str(&config_json)
            .map_err(|e| format!("invalid config JSON: {e}"))?;

        host::emit_file("report.json", &report_json)?;
        let steps = report["steps"].as_array().map_or(0, |s| s.len());
        let run_id = report["run"]["run_id"].as_str().unwrap_or("?");
        let summary = format!("run {run_id}: {steps} step(s)");
        host::emit_file("summary.txt", &summary)?;

        if config["emit_evil"] == true {
            // Must be refused host-side; the guest surfaces the refusal.
            host::emit_file("../evil.txt", "x").map_err(|e| format!("emit-file refused: {e}"))?;
            return Err("emit-file accepted a traversal path".into());
        }
        if config["fail"] == true {
            return Err("requested failure".into());
        }
        Ok(summary)
    }
}

bindings::export!(ExampleReporter with_types_in bindings);
