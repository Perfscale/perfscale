//! Test fixture library: `spin()` never returns, so the engine's per-call
//! fuel budget must trap it (RFC 005 resource limits).

use perfscale_library_sdk::{export_library, Ctx, Error, FunctionInfo, Library};

#[derive(Default)]
struct Spin;

impl Library for Spin {
    fn functions(&self) -> Vec<FunctionInfo> {
        vec![FunctionInfo {
            name: "spin",
            description: "Infinite loop — trips the fuel limit",
            secret: false,
        }]
    }

    fn call(&mut self, _ctx: &mut Ctx, func: &str, _args: Vec<serde_json::Value>) -> Result<String, Error> {
        match func {
            "spin" => loop {
                std::hint::spin_loop();
            },
            other => Err(Error::new(format!("spin.{other}: unknown function"))),
        }
    }
}

export_library!(Spin);
