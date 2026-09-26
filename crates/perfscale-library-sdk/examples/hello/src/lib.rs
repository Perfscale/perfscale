//! Example perfscale library (RFC 005): `greet(name)` and `token([key])`.
//!
//! Build: `cargo build --release --target wasm32-wasip2` — the artifact at
//! `target/wasm32-wasip2/release/perfscale_hello_library.wasm` is referenced
//! from YAML as `libraries: - use: <path>.wasm`.

use perfscale_library_sdk::{args, export_library, Ctx, Error, FunctionInfo, Library};

#[derive(Default)]
struct Hello {
    greeting: Option<String>,
}

impl Library for Hello {
    fn functions(&self) -> Vec<FunctionInfo> {
        vec![
            FunctionInfo {
                name: "greet",
                description: "Greeting for a name",
                secret: false,
            },
            FunctionInfo {
                name: "token",
                description: "Random 16-char token; optional memo key reuses it within one message",
                secret: true,
            },
            FunctionInfo {
                name: "settings",
                description: "The frozen run settings JSON the host handed over (WIT 0.2 context)",
                secret: false,
            },
        ]
    }

    fn init(&mut self, config: serde_json::Value) -> Result<(), Error> {
        self.greeting = config
            .get("greeting")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(())
    }

    fn call(&mut self, ctx: &mut Ctx, func: &str, args: Vec<serde_json::Value>) -> Result<String, Error> {
        match func {
            "greet" => {
                let name = args::string(&args, 0, func)?;
                Ok(format!("{}, {name}!", self.greeting.as_deref().unwrap_or("hello")))
            }
            "token" => {
                let mint = |c: &mut Ctx| {
                    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
                    Ok((0..16)
                        .map(|_| ALPHABET[c.prng().below(36) as usize] as char)
                        .collect::<String>())
                };
                // `${hello.token(order)}` appearing twice in one message
                // yields one token; `${hello.token()}` is always fresh.
                match args::optional_string(&args, 0) {
                    Some(key) => ctx.memo(&key, mint),
                    None => mint(ctx),
                }
            }
            "settings" => Ok(ctx.settings_json.clone()),
            other => Err(Error::new(format!(
                "hello.{other}: unknown function — exports: greet, token, settings"
            ))),
        }
    }
}

export_library!(Hello);

#[cfg(test)]
mod tests {
    use super::*;
    use perfscale_library_sdk::test_call;

    // The SDK harness: no wasm runtime, no engine — call the trait directly.
    #[test]
    fn greet_and_memoized_token() {
        let mut lib = Hello::default();
        lib.init(serde_json::json!({ "greeting": "hi" })).unwrap();
        let mut ctx = Ctx::new(42);
        ctx.message_seq = 1;
        assert_eq!(
            test_call(&mut lib, &mut ctx, "greet", vec!["world".into()]).unwrap(),
            "hi, world!"
        );
        let a = test_call(&mut lib, &mut ctx, "token", vec!["order".into()]).unwrap();
        let b = test_call(&mut lib, &mut ctx, "token", vec!["order".into()]).unwrap();
        assert_eq!(a, b, "same message + key → same token");
        ctx.message_seq = 2;
        let c = test_call(&mut lib, &mut ctx, "token", vec!["order".into()]).unwrap();
        assert_ne!(a, c, "next message → fresh token");
        assert!(test_call(&mut lib, &mut ctx, "nope", vec![]).is_err());
    }
}
