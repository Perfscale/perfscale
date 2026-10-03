//! Test fixture with the jco/StarlingMonkey shape: the component imports
//! `wasi:filesystem/*` (toolchain noise) but `info()` reports `"pure": true`,
//! so it loads without an `fs` grant. `read(path)` still attempts a file
//! read at runtime — without a grant that fails (no preopens), with the
//! grant it works. Used to prove the pure marker waives the grant without
//! weakening the runtime sandbox.

use perfscale_library_sdk::{args, export_library, Ctx, Error, FunctionInfo, Library};

#[derive(Default)]
struct PureFs;

impl Library for PureFs {
    fn functions(&self) -> Vec<FunctionInfo> {
        vec![
            FunctionInfo {
                name: "echo",
                description: "Echo the argument back (no I/O)",
                secret: false,
            },
            FunctionInfo {
                name: "read",
                description: "Attempt a file read (fails without a preopen)",
                secret: false,
            },
        ]
    }

    fn pure(&self) -> bool {
        true
    }

    fn call(&mut self, _ctx: &mut Ctx, func: &str, args: Vec<serde_json::Value>) -> Result<String, Error> {
        match func {
            "echo" => args::string(&args, 0, func),
            "read" => {
                let path = args::string(&args, 0, func)?;
                std::fs::read_to_string(&path)
                    .map(|s| s.trim().to_string())
                    .map_err(|e| Error::new(format!("read({path}): {e}")))
            }
            other => Err(Error::new(format!("purefs.{other}: unknown function"))),
        }
    }
}

export_library!(PureFs);
