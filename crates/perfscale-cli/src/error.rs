//! Self-descriptive CLI errors: every failure says what happened, why, what
//! to do about it, and where the docs are.
//!
//! ```text
//! error: invalid test file 'bad.yaml'
//!   cause: schema validation failed:
//!     /steps/0 — "use" is a required property
//!   hint: every step needs a `use:` field naming an action, e.g. `use: std/http@v1`
//!   docs: https://github.com/Perfscale/perfscale/blob/main/docs/yaml-reference.md
//! ```

use std::fmt;

pub const DOCS_BASE: &str = "https://github.com/Perfscale/perfscale/blob/main/docs";

#[derive(Debug)]
pub struct CliError {
    message: String,
    cause: Option<String>,
    hint: Option<String>,
    docs: Option<String>,
}

impl CliError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cause: None,
            hint: None,
            docs: None,
        }
    }

    pub fn cause(mut self, cause: impl Into<String>) -> Self {
        self.cause = Some(cause.into());
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Doc page path relative to `docs/`, e.g. `"yaml-reference.md"` or
    /// `"cli/commands.md#exit-code-semantics"`.
    pub fn docs(mut self, page: &str) -> Self {
        self.docs = Some(format!("{DOCS_BASE}/{page}"));
        self
    }

    /// Wrap an error string coming out of `perfscale-core` (engine spawn,
    /// YAML parsing, ...) and attach the most useful hint we can infer.
    pub fn from_engine(message: String) -> Self {
        if message.contains("k6 not found in PATH") {
            Self::new(message)
                .hint("install k6, or use the built-in engine instead: `perfscale run -f test.yaml -c config.yaml`")
                .docs("getting-started.md#install")
        } else if message.contains("locust not found in PATH") {
            Self::new(message)
                .hint("install locust (`pip install locust`), or use the built-in engine instead: `perfscale run -f test.yaml -c config.yaml`")
                .docs("getting-started.md#install")
        } else {
            Self::new(message).docs("cli/commands.md")
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(cause) = &self.cause {
            // Indent multi-line causes so they read as one block.
            let indented = cause.replace('\n', "\n    ");
            write!(f, "\n  cause: {indented}")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, "\n  hint: {hint}")?;
        }
        if let Some(docs) = &self.docs {
            write!(f, "\n  docs: {docs}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_error_is_single_line() {
        let e = CliError::new("boom");
        assert_eq!(e.to_string(), "boom");
    }

    #[test]
    fn full_error_renders_all_sections_in_order() {
        let e = CliError::new("invalid test file 'x.yaml'")
            .cause("schema validation failed")
            .hint("add a `use:` field")
            .docs("yaml-reference.md");
        let s = e.to_string();
        let cause_pos = s.find("cause:").unwrap();
        let hint_pos = s.find("hint:").unwrap();
        let docs_pos = s.find("docs:").unwrap();
        assert!(
            cause_pos < hint_pos && hint_pos < docs_pos,
            "section order: {s}"
        );
        assert!(
            s.contains(&format!("{DOCS_BASE}/yaml-reference.md")),
            "docs url: {s}"
        );
    }

    #[test]
    fn multiline_cause_is_indented() {
        let e = CliError::new("x").cause("line one\nline two");
        assert!(e.to_string().contains("line one\n    line two"));
    }

    #[test]
    fn from_engine_k6_not_found_points_at_install_docs() {
        let e =
            CliError::from_engine("k6 not found in PATH — install from https://k6.io/...".into());
        let s = e.to_string();
        assert!(s.contains("hint:"), "{s}");
        assert!(s.contains("built-in engine"), "{s}");
        assert!(s.contains("getting-started.md#install"), "{s}");
    }

    #[test]
    fn from_engine_locust_not_found_points_at_install_docs() {
        let e = CliError::from_engine(
            "locust not found in PATH — install with `pip install locust`".into(),
        );
        let s = e.to_string();
        assert!(s.contains("pip install locust"), "{s}");
        assert!(s.contains("getting-started.md#install"), "{s}");
    }

    #[test]
    fn from_engine_other_errors_link_command_docs() {
        let e = CliError::from_engine("Failed to spawn k6: permission denied".into());
        assert!(e.to_string().contains("cli/commands.md"));
    }
}
