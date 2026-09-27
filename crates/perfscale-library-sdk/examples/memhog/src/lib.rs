//! Test fixture library for the host's per-instance memory cap:
//! `grow(mb)` allocates and *touches* `mb` MiB of linear memory. The host
//! caps each instance at 64 MiB, so `grow(128)` must trap (memory growth
//! refused by the store limiter) while `grow(1)` succeeds.

use perfscale_library_sdk::{export_library, Ctx, Error, FunctionInfo, Library};

#[derive(Default)]
struct MemHog;

impl Library for MemHog {
    fn functions(&self) -> Vec<FunctionInfo> {
        vec![FunctionInfo {
            name: "grow",
            description: "Allocate and touch N MiB of linear memory",
            secret: false,
        }]
    }

    fn call(&mut self, _ctx: &mut Ctx, func: &str, args: Vec<serde_json::Value>) -> Result<String, Error> {
        match func {
            "grow" => {
                let mb = args[0].as_u64().unwrap_or(1) as usize;
                // touch_every touches each page: allocation alone may stay
                // virtual — the cap must trip on real growth.
                let mut buf = vec![0u8; mb << 20];
                for (i, b) in buf.iter_mut().step_by(4096).enumerate() {
                    *b = (i & 0xFF) as u8;
                }
                Ok(format!("grew {} MiB (checksum {})", mb, buf.iter().map(|b| *b as u64).sum::<u64>() % 1000))
            }
            other => Err(Error::new(format!("memhog.{other}: unknown function"))),
        }
    }
}

export_library!(MemHog);
