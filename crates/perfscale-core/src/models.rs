//! Shared result types.

use serde::{Deserialize, Serialize};

/// Result of a completed script-based run (k6 or locust, oneshot mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub exit_code: i32,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub script: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_result_serde_round_trip() {
        let result = RunResult {
            exit_code: 0,
            success: true,
            stdout: "ok".into(),
            stderr: String::new(),
            script: "export default function(){}".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: RunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.exit_code, 0);
        assert!(back.success);
        assert_eq!(back.stdout, "ok");
        assert_eq!(back.script, result.script);
    }
}
