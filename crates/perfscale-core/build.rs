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

    // protoc replacement: pure-Rust proto compilation → FileDescriptorSet
    // (source info included, so doc comments survive into the codegen).
    let fds = protox::compile(["proto/echo.proto"], ["proto"])?;

    // Persisted for the reflection service and the `descriptor_set` tests.
    std::fs::write(out_dir.join("echo_descriptor.bin"), fds.encode_to_vec())?;

    // Service/message code from the same descriptor set — no protoc involved.
    tonic_prost_build::configure().compile_fds(fds)?;
    Ok(())
}
