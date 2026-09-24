//! Test fixture library importing `wasi:filesystem`: `read(path)` returns the
//! file's content trimmed. Loading it without an `fs` grant must be a hard
//! error; with the grant it reads through the host's read-only preopen.

use perfscale_library_sdk::{args, export_library, Ctx, Error, FunctionInfo, Library};

#[derive(Default)]
struct FsReader;

impl Library for FsReader {
    fn functions(&self) -> Vec<FunctionInfo> {
        vec![FunctionInfo {
            name: "read",
            description: "Read a corpus file (preopened fs_root, read-only)",
            secret: false,
        }]
    }

    fn call(&mut self, _ctx: &mut Ctx, func: &str, args: Vec<serde_json::Value>) -> Result<String, Error> {
        match func {
            "read" => {
                let path = args::string(&args, 0, func)?;
                std::fs::read_to_string(&path)
                    .map(|s| s.trim().to_string())
                    .map_err(|e| Error::new(format!("read({path}): {e}")))
            }
            other => Err(Error::new(format!("fsreader.{other}: unknown function"))),
        }
    }
}

export_library!(FsReader);
