//! Validation integration for imposter and stub configurations.
//!
//! This module provides validation capabilities using the `rift-lint` library
//! to validate configurations before importing or saving them.

use rift_lint::{LintOptions, LintResult, Severity, lint_json, lint_value, validate_stub};
use std::path::PathBuf;

/// Validate an imposter configuration from a JSON string.
///
/// Returns a `ValidationReport` containing all issues found.
pub fn validate_imposter_json(json: &str, source_name: &str) -> ValidationReport {
    let options = LintOptions::default();
    let result = lint_json(json, source_name, &options);
    ValidationReport::from_lint_result(result)
}

/// Validate an imposter configuration from a parsed JSON value.
///
/// Returns a `ValidationReport` containing all issues found.
pub fn validate_imposter_value(value: &serde_json::Value, source_name: &str) -> ValidationReport {
    let options = LintOptions::default();
    let result = lint_value(value, source_name, &options);
    ValidationReport::from_lint_result(result)
}

/// Validate a stub configuration from a JSON string.
///
/// Returns a `ValidationReport` containing all issues found.
pub fn validate_stub_json(json: &str) -> ValidationReport {
    let options = LintOptions::default();

    // First check if it's valid JSON
    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            return ValidationReport {
                issues: vec![ValidationIssue {
                    severity: IssueSeverity::Error,
                    code: "E002".to_string(),
                    message: format!("Invalid JSON: {e}"),
                    location: None,
                    suggestion: Some("Fix the JSON syntax errors".to_string()),
                }],
                errors: 1,
                warnings: 0,
            };
        }
    };

    // Use the validator to check the stub
    let mut lint_result = LintResult {
        files_checked: 1,
        ..Default::default()
    };
    let path = PathBuf::from("<editor>");
    // A standalone stub has no imposter-level `_rift.scripts` registry to resolve `ref:` against.
    validate_stub(
        &path,
        &value,
        0,
        &mut lint_result,
        &options,
        &serde_json::Value::Null,
    );

    ValidationReport::from_lint_result(lint_result)
}

/// Severity level for validation issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    Error,
    Warning,
    Info,
}

impl IssueSeverity {
    /// Get the label for this severity level.
    pub fn label(&self) -> &'static str {
        match self {
            IssueSeverity::Error => "error",
            IssueSeverity::Warning => "warning",
            IssueSeverity::Info => "info",
        }
    }
}

impl From<Severity> for IssueSeverity {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Error => IssueSeverity::Error,
            Severity::Warning => IssueSeverity::Warning,
            Severity::Info => IssueSeverity::Info,
        }
    }
}

/// A single validation issue.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationIssue {
    pub severity: IssueSeverity,
    pub code: String,
    pub message: String,
    pub location: Option<String>,
    pub suggestion: Option<String>,
}

/// Report containing all validation issues.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidationReport {
    pub issues: Vec<ValidationIssue>,
    pub errors: usize,
    pub warnings: usize,
}

impl ValidationReport {
    /// Create a report from a lint result.
    fn from_lint_result(result: LintResult) -> Self {
        let issues = result
            .issues
            .into_iter()
            .map(|issue| ValidationIssue {
                severity: issue.severity.into(),
                code: issue.code,
                message: issue.message,
                location: issue.location,
                suggestion: issue.suggestion,
            })
            .collect();

        Self {
            issues,
            errors: result.errors,
            warnings: result.warnings,
        }
    }

    /// Check if validation passed (no errors).
    pub fn is_valid(&self) -> bool {
        self.errors == 0
    }

    /// Check if there are any errors.
    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }

    /// Check if there are any warnings.
    pub fn has_warnings(&self) -> bool {
        self.warnings > 0
    }

    /// Check if there are any issues at all.
    pub fn has_issues(&self) -> bool {
        !self.issues.is_empty()
    }

    /// Get a summary string for the status bar.
    pub fn summary(&self) -> String {
        if self.errors == 0 && self.warnings == 0 {
            "Valid".to_string()
        } else if self.errors > 0 && self.warnings > 0 {
            format!("{} errors, {} warnings", self.errors, self.warnings)
        } else if self.errors > 0 {
            format!(
                "{} error{}",
                self.errors,
                if self.errors == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "{} warning{}",
                self.warnings,
                if self.warnings == 1 { "" } else { "s" }
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_valid_stub() {
        let json = r#"{
            "predicates": [{"equals": {"path": "/test"}}],
            "responses": [{"is": {"statusCode": 200}}]
        }"#;
        let report = validate_stub_json(json);
        assert!(report.is_valid());
    }

    #[test]
    fn test_validate_invalid_json() {
        let json = r#"{ invalid json }"#;
        let report = validate_stub_json(json);
        assert!(report.has_errors());
        assert_eq!(report.errors, 1);
    }

    #[test]
    fn test_validate_missing_responses() {
        let json = r#"{
            "predicates": [{"equals": {"path": "/test"}}]
        }"#;
        let report = validate_stub_json(json);
        assert!(report.has_errors());
    }

    #[test]
    fn test_validate_valid_imposter() {
        let json = r#"{
            "port": 4545,
            "protocol": "http",
            "stubs": [
                {
                    "predicates": [{"equals": {"path": "/test"}}],
                    "responses": [{"is": {"statusCode": 200}}]
                }
            ]
        }"#;
        let report = validate_imposter_json(json, "test.json");
        assert!(report.is_valid());
    }

    #[test]
    fn test_validate_invalid_port() {
        let json = r#"{
            "port": 99999,
            "protocol": "http",
            "stubs": []
        }"#;
        let report = validate_imposter_json(json, "test.json");
        assert!(report.has_errors());
    }
}
