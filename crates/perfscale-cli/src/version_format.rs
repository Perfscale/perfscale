//! Version-string composition shared by `build.rs` (via `#[path]`) and the
//! binary's unit tests. Keep this file dependency-free: both the build
//! script and the bin target compile it.

/// Compose the displayed version from the Cargo package version and the
/// output of `git describe --tags --match 'v*' --long`.
///
/// - Exact tag checkout (`v0.25.0-0-g<sha>`) → the clean release version
///   (`0.25.0`), so a released binary matches the tag it was built from.
/// - Commits after the tag → `<base>-next.<distance>+g<sha>`, a semver
///   pre-release: it sorts *below* the release it follows, so `self-update`
///   treats the stable release as an upgrade from a dev build.
/// - No git / no tags / shallow clone → `<base>-next+unknown`: an
///   unidentifiable build must never pose as a release (release builds run
///   from a tag checkout with full history — see release.yml).
pub fn from_describe(base: &str, describe: Option<&str>) -> String {
    match describe.and_then(parse_describe) {
        Some((0, _)) => base.to_string(),
        Some((distance, sha)) => format!("{base}-next.{distance}+g{sha}"),
        None => format!("{base}-next+unknown"),
    }
}

/// Parse `git describe --long` output (`<tag>-<distance>-g<sha>`) into the
/// commit distance and short sha.
fn parse_describe(describe: &str) -> Option<(u64, &str)> {
    let (head, sha) = describe.rsplit_once("-g")?;
    let (_tag, distance) = head.rsplit_once('-')?;
    if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((distance.parse().ok()?, sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_tag_yields_clean_release_version() {
        assert_eq!(
            from_describe("0.25.0", Some("v0.25.0-0-g520b478")),
            "0.25.0"
        );
    }

    #[test]
    fn commits_after_tag_yield_next_pre_release() {
        assert_eq!(
            from_describe("0.25.0", Some("v0.25.0-28-g520b478")),
            "0.25.0-next.28+g520b478"
        );
    }

    #[test]
    fn missing_git_or_tags_falls_back_to_unknown_dev_marker() {
        assert_eq!(from_describe("0.25.0", None), "0.25.0-next+unknown");
    }

    #[test]
    fn malformed_describe_falls_back_to_unknown_dev_marker() {
        for garbage in [
            "",
            "not-a-version",
            "v0.25.0",
            "v0.25.0-abc-g520b478",
            "v0.25.0-7-gxyz!",
        ] {
            assert_eq!(
                from_describe("0.25.0", Some(garbage)),
                "0.25.0-next+unknown",
                "input: {garbage:?}"
            );
        }
    }
}
