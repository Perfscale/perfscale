//! Compiles `proto/echo.proto` for the in-crate gRPC test server.
//!
//! The generated code is referenced only by `#[cfg(test)]` modules and the
//! `grpc_echo_server` example — but a build script cannot be dev-only, so
//! this runs on every build of perfscale-core. It is deliberately **pure
//! Rust**: `protox` compiles the .proto into a `FileDescriptorSet` and
//! tonic's codegen consumes that set directly, so no `protoc` binary is
//! needed on PATH (perfscale-core is consumed as a git dependency by other
//! repos whose Docker/CI images do not ship protoc).

use prost::Message as _;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/echo.proto");
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);

    // Burn artifacts (library/burn.rs) are tied to the exact wasmtime version
    // and target triple that produced them; pin both into the binary so the
    // loader can reject foreign artifacts before `Component::deserialize`.
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    let lock = std::fs::read_to_string("../../Cargo.lock").unwrap_or_default();
    println!(
        "cargo:rustc-env=PERFSCALE_WASMTIME_VERSION={}",
        wasmtime_version(&lock).unwrap_or("unknown")
    );
    println!(
        "cargo:rustc-env=PERFSCALE_TARGET_TRIPLE={}",
        std::env::var("TARGET")?
    );

    // protoc replacement: pure-Rust proto compilation → FileDescriptorSet
    // (source info included, so doc comments survive into the codegen).
    let fds = protox::compile(["proto/echo.proto"], ["proto"])?;

    // Persisted for the reflection service and the `descriptor_set` tests.
    std::fs::write(out_dir.join("echo_descriptor.bin"), fds.encode_to_vec())?;

    // Service/message code from the same descriptor set — no protoc involved.
    tonic_prost_build::configure().compile_fds(fds)?;
    Ok(())
}

/// Extract the wasmtime version from the workspace Cargo.lock (the `wasmtime`
/// package entry). `None` when the lock is missing or unparseable — burn
/// artifacts then carry "unknown" and only match other "unknown" builds.
fn wasmtime_version(lock: &str) -> Option<&str> {
    let entry = lock.split("[[package]]").find(|p| {
        p.lines()
            .any(|l| l.trim() == "name = \"wasmtime\"")
    })?;
    entry.lines().find_map(|l| {
        l.trim()
            .strip_prefix("version = \"")
            .and_then(|rest| rest.strip_suffix('"'))
    })
}
