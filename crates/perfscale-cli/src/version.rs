//! The build-time version string — composed by `build.rs`, see
//! `version_format.rs` for the rules.
//!
//! Release binaries print the clean Cargo version (`0.25.0`); dev builds
//! print a semver pre-release like `0.25.0-next.7+g520b478`, so a local
//! build is never mistaken for the release it follows.

pub const VERSION: &str = env!("PERFSCALE_BUILD_VERSION");
