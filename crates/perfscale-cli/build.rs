//! Stamps the binary with a version string that distinguishes release builds
//! (exact tag checkout) from dev builds — see `src/version_format.rs` for the
//! composition rules. `PERFSCALE_VERSION_OVERRIDE` pins the version for
//! builds outside a git checkout (dist tarballs, packaging).

#[path = "src/version_format.rs"]
mod version_format;

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PERFSCALE_VERSION_OVERRIDE");

    // Re-stamp when the checkout moves: new commit, branch switch, new tag.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string("../../.git/HEAD") {
        if let Some(reference) = head.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed=../../.git/{reference}");
        }
    }

    let version = match std::env::var("PERFSCALE_VERSION_OVERRIDE") {
        Ok(version) => version,
        Err(_) => {
            version_format::from_describe(env!("CARGO_PKG_VERSION"), git_describe().as_deref())
        }
    };
    println!("cargo:rustc-env=PERFSCALE_BUILD_VERSION={version}");
}

fn git_describe() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--match", "v*", "--long"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
