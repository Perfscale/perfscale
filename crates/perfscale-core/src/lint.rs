//! YAML linting for test-definition and config files.
//!
//! Goes beyond schema validation: every issue carries *where* it is, *what*
//! is wrong, and — where we can tell — *what to use instead* (including
//! did-you-mean suggestions for typo'd field names, which plain schema
//! validation cannot express because unknown fields are legal for forward
//! compatibility at run time).

use serde_json::Value;

/// One problem found in a document.
#[derive(Debug, Clone, PartialEq)]
pub struct LintIssue {
    /// JSON-pointer-ish location, e.g. `/steps/0` or `(file)`.
    pub location: String,
    /// What is wrong.
    pub problem: String,
    /// What to use instead, when we can tell.
    pub suggestion: Option<String>,
}

/// Which schema a document should be linted against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Test,
    Config,
}

/// Guess the document kind: a mapping with a `steps` key is a test
/// definition, anything else is treated as a config.
pub fn detect_kind(yaml: &str) -> DocKind {
    match serde_yaml::from_str::<Value>(yaml) {
        Ok(v) if v.get("steps").is_some() => DocKind::Test,
        _ => DocKind::Config,
    }
}

/// Lint a document. Empty result = valid.
pub fn lint(yaml: &str, kind: DocKind) -> Vec<LintIssue> {
    let value: Value = match serde_yaml::from_str(yaml) {
        Ok(v) => v,
        Err(e) => {
            return vec![LintIssue {
                location: "(file)".into(),
                problem: format!("invalid YAML: {e}"),
                suggestion: Some(
                    "check indentation and quoting — every step list item starts with `- `".into(),
                ),
            }];
        }
    };

    let mut issues = Vec::new();
    schema_issues(&value, kind, &mut issues);
    match kind {
        DocKind::Test => lint_test_fields(&value, &mut issues),
        DocKind::Config => lint_config_fields(&value, &mut issues),
    }
    issues
}

// ---------------------------------------------------------------------------
// Schema validation → issues with suggestions
// ---------------------------------------------------------------------------

fn schema_issues(value: &Value, kind: DocKind, issues: &mut Vec<LintIssue>) {
    let schema = match kind {
        DocKind::Test => crate::schema::test_schema(),
        DocKind::Config => crate::schema::config_schema(),
    };
    let compiled =
        jsonschema::JSONSchema::compile(&schema).expect("generated schemas always compile");

    let collected: Vec<(String, String)> = match compiled.validate(value) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|e| (e.instance_path.to_string(), e.to_string()))
            .collect(),
    };

    for (path, problem) in collected {
        let location = if path.is_empty() {
            "(root)".to_string()
        } else {
            path
        };
        let suggestion = schema_error_suggestion(&problem);
        issues.push(LintIssue {
            location,
            problem,
            suggestion,
        });
    }
}

fn schema_error_suggestion(problem: &str) -> Option<String> {
    if problem.contains("\"use\" is a required property") {
        Some("every step must name an action: `use: std/http@v1`, `std/check@v1`, `std/sleep@v1`, or `std/log@v1`".into())
    } else if problem.contains("\"steps\" is a required property") {
        Some("a test definition is a mapping with a `steps:` list at the top level".into())
    } else if problem.contains("\"url\" is a required property") {
        Some("`report:` needs a `url:` pointing at a running `perfscale serve`, e.g. `url: http://localhost:7999`".into())
    } else if problem.contains("is not of type \"integer\"") {
        Some("use a plain number, e.g. `vus: 10`".into())
    } else if problem.contains("is not of type \"string\"") {
        Some("quote the value if it contains special characters, e.g. `duration: \"30s\"`".into())
    } else if problem.contains("is not of type \"array\"") {
        Some("`steps:` is a list — each entry starts with `- `".into())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Unknown / typo'd fields (beyond what the schema rejects)
// ---------------------------------------------------------------------------

const TEST_TOP_FIELDS: [&str; 1] = ["steps"];
const STEP_FIELDS: [&str; 5] = ["name", "use", "with", "check", "outputs"];
const CONFIG_TOP_FIELDS: [&str; 3] = ["vus", "duration", "report"];
const REPORT_FIELDS: [&str; 1] = ["url"];
const CHECK_FIELDS: [&str; 4] = ["on", "status", "duration_ms_lt", "body_contains"];
const HTTP_WITH_FIELDS: [&str; 7] = [
    "method",
    "url",
    "headers",
    "body",
    "timeout",
    "insecure",
    "multipart",
];
const SLEEP_WITH_FIELDS: [&str; 2] = ["ms", "seconds"];
const LOG_WITH_FIELDS: [&str; 1] = ["message"];

fn lint_test_fields(value: &Value, issues: &mut Vec<LintIssue>) {
    if let Some(map) = value.as_object() {
        unknown_field_issues(map, &TEST_TOP_FIELDS, "(root)", issues);
    }

    let Some(steps) = value.get("steps").and_then(|s| s.as_array()) else {
        return;
    };

    for (i, step) in steps.iter().enumerate() {
        let loc = format!("/steps/{i}");
        let Some(map) = step.as_object() else {
            continue;
        };

        unknown_field_issues(map, &STEP_FIELDS, &loc, issues);

        let action = map.get("use").and_then(|v| v.as_str()).unwrap_or("");
        if !action.is_empty() && !is_known_action(action) {
            issues.push(LintIssue {
                location: format!("{loc}/use"),
                problem: format!("unknown action '{action}'"),
                suggestion: did_you_mean(
                    action,
                    &["std/http@v1", "std/check@v1", "std/sleep@v1", "std/log@v1"],
                )
                .or_else(|| {
                    Some(
                        "available actions: std/http@v1, std/check@v1, std/sleep@v1, std/log@v1"
                            .into(),
                    )
                }),
            });
        }

        if let Some(with) = map.get("with").and_then(|v| v.as_object()) {
            let with_fields: Option<&[&str]> = match action {
                "std/http@v1" | "http" => Some(&HTTP_WITH_FIELDS),
                "std/check@v1" | "check" => Some(&CHECK_FIELDS),
                "std/sleep@v1" | "sleep" => Some(&SLEEP_WITH_FIELDS),
                "std/log@v1" | "log" => Some(&LOG_WITH_FIELDS),
                _ => None,
            };
            if let Some(fields) = with_fields {
                unknown_field_issues(with, fields, &format!("{loc}/with"), issues);
            }
        }

        if let Some(check) = map.get("check").and_then(|v| v.as_object()) {
            unknown_field_issues(check, &CHECK_FIELDS, &format!("{loc}/check"), issues);
        }
    }
}

fn lint_config_fields(value: &Value, issues: &mut Vec<LintIssue>) {
    let Some(map) = value.as_object() else { return };
    unknown_field_issues(map, &CONFIG_TOP_FIELDS, "(root)", issues);

    if let Some(report) = map.get("report").and_then(|v| v.as_object()) {
        unknown_field_issues(report, &REPORT_FIELDS, "/report", issues);
    }
}

fn is_known_action(action: &str) -> bool {
    matches!(
        action,
        "std/http@v1"
            | "http"
            | "std/check@v1"
            | "check"
            | "std/sleep@v1"
            | "sleep"
            | "std/log@v1"
            | "log"
    )
}

fn unknown_field_issues(
    map: &serde_json::Map<String, Value>,
    known: &[&str],
    location: &str,
    issues: &mut Vec<LintIssue>,
) {
    for key in map.keys() {
        if !known.contains(&key.as_str()) {
            issues.push(LintIssue {
                location: location.to_string(),
                problem: format!("unknown field '{key}'"),
                suggestion: did_you_mean(key, known)
                    .or_else(|| Some(format!("valid fields here: {}", known.join(", ")))),
            });
        }
    }
}

/// `Some("did you mean 'check'?")` when a known name is within edit
/// distance 2 of the typo.
fn did_you_mean(input: &str, candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .map(|c| (c, edit_distance(input, c)))
        .filter(|(_, d)| *d <= 2)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| format!("did you mean '{c}'?"))
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];

    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_test_file_has_no_issues() {
        let yaml = r#"
steps:
  - name: ping
    use: std/http@v1
    with:
      method: GET
      url: https://example.com
    check:
      status: 200
    outputs: resp
"#;
        assert_eq!(lint(yaml, DocKind::Test), vec![]);
    }

    #[test]
    fn valid_config_has_no_issues() {
        let yaml = "vus: 10\nduration: 30s\nreport:\n  url: http://localhost:7999\n";
        assert_eq!(lint(yaml, DocKind::Config), vec![]);
    }

    #[test]
    fn malformed_yaml_is_one_issue_with_suggestion() {
        let issues = lint("steps: [oops: {", DocKind::Test);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].problem.contains("invalid YAML"));
        assert!(issues[0].suggestion.is_some());
    }

    #[test]
    fn missing_use_reports_location_and_fix() {
        let yaml = "steps:\n  - name: ping\n    with:\n      url: https://x\n";
        let issues = lint(yaml, DocKind::Test);
        let missing = issues
            .iter()
            .find(|i| i.problem.contains("\"use\" is a required property"))
            .unwrap();
        assert_eq!(missing.location, "/steps/0");
        assert!(missing
            .suggestion
            .as_deref()
            .unwrap()
            .contains("std/http@v1"));
    }

    #[test]
    fn typo_in_step_field_gets_did_you_mean() {
        let yaml = "steps:\n  - use: std/http@v1\n    with:\n      url: https://x\n    chek:\n      status: 200\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'chek'"))
            .unwrap();
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'check'?"));
    }

    #[test]
    fn typo_in_check_key_gets_did_you_mean() {
        let yaml =
            "steps:\n  - use: std/http@v1\n    with:\n      url: https://x\n    check:\n      body_containz: ok\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'body_containz'"))
            .unwrap();
        assert_eq!(typo.location, "/steps/0/check");
        assert_eq!(
            typo.suggestion.as_deref(),
            Some("did you mean 'body_contains'?")
        );
    }

    #[test]
    fn typo_in_http_with_key_gets_did_you_mean() {
        let yaml =
            "steps:\n  - use: std/http@v1\n    with:\n      url: https://x\n      methd: GET\n";
        let issues = lint(yaml, DocKind::Test);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'methd'"))
            .unwrap();
        assert_eq!(typo.location, "/steps/0/with");
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'method'?"));
    }

    #[test]
    fn unknown_action_lists_alternatives() {
        let yaml = "steps:\n  - use: std/htp@v1\n    with:\n      url: https://x\n";
        let issues = lint(yaml, DocKind::Test);
        let bad = issues
            .iter()
            .find(|i| i.problem.contains("unknown action"))
            .unwrap();
        assert_eq!(bad.location, "/steps/0/use");
        assert_eq!(
            bad.suggestion.as_deref(),
            Some("did you mean 'std/http@v1'?")
        );
    }

    #[test]
    fn config_typo_gets_did_you_mean() {
        let yaml = "vsu: 10\nduration: 30s\n";
        let issues = lint(yaml, DocKind::Config);
        let typo = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'vsu'"))
            .unwrap();
        assert_eq!(typo.suggestion.as_deref(), Some("did you mean 'vus'?"));
    }

    #[test]
    fn config_wrong_type_gets_type_suggestion() {
        let issues = lint("vus: ten\n", DocKind::Config);
        let wrong = issues
            .iter()
            .find(|i| i.problem.contains("is not of type \"integer\""))
            .unwrap();
        assert!(wrong.suggestion.as_deref().unwrap().contains("vus: 10"));
    }

    #[test]
    fn detect_kind_by_steps_key() {
        assert_eq!(detect_kind("steps: []\n"), DocKind::Test);
        assert_eq!(detect_kind("vus: 5\n"), DocKind::Config);
        assert_eq!(detect_kind("not: [valid"), DocKind::Config);
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("check", "check"), 0);
        assert_eq!(edit_distance("chek", "check"), 1);
        assert_eq!(edit_distance("vsu", "vus"), 2);
        assert!(edit_distance("completely", "different") > 2);
    }

    #[test]
    fn unrelated_unknown_field_lists_valid_fields() {
        let yaml = "steps:\n  - use: std/log@v1\n    frobnicate: yes\n";
        let issues = lint(yaml, DocKind::Test);
        let unknown = issues
            .iter()
            .find(|i| i.problem.contains("unknown field 'frobnicate'"))
            .unwrap();
        assert!(unknown
            .suggestion
            .as_deref()
            .unwrap()
            .contains("valid fields here"));
    }
}
