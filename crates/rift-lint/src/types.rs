//! Core types for the linting library.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// Severity level of a lint issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A critical issue that will cause failures.
    Error,
    /// A potential issue that should be addressed.
    Warning,
    /// Informational message.
    Info,
}

impl Severity {
    /// Get the label for this severity level.
    pub fn label(&self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

/// A single lint issue found during validation.
#[derive(Debug, Clone, Serialize)]
pub struct LintIssue {
    /// Severity of the issue.
    pub severity: Severity,
    /// Error code (e.g., "E001", "W001").
    pub code: String,
    /// Human-readable description of the issue.
    pub message: String,
    /// File where the issue was found.
    #[serde(serialize_with = "serialize_path")]
    pub file: PathBuf,
    /// Location within the file (e.g., "stubs[0].responses[0]").
    pub location: Option<String>,
    /// Suggested fix for the issue.
    pub suggestion: Option<String>,
}

fn serialize_path<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&path.to_string_lossy())
}

impl LintIssue {
    /// Create a new error issue.
    pub fn error(code: impl Into<String>, message: impl Into<String>, file: PathBuf) -> Self {
        Self {
            severity: Severity::Error,
            code: code.into(),
            message: message.into(),
            file,
            location: None,
            suggestion: None,
        }
    }

    /// Create a new warning issue.
    pub fn warning(code: impl Into<String>, message: impl Into<String>, file: PathBuf) -> Self {
        Self {
            severity: Severity::Warning,
            code: code.into(),
            message: message.into(),
            file,
            location: None,
            suggestion: None,
        }
    }

    /// Create a new info issue.
    pub fn info(code: impl Into<String>, message: impl Into<String>, file: PathBuf) -> Self {
        Self {
            severity: Severity::Info,
            code: code.into(),
            message: message.into(),
            file,
            location: None,
            suggestion: None,
        }
    }

    /// Set the location for this issue.
    pub fn with_location(mut self, location: impl Into<String>) -> Self {
        self.location = Some(location.into());
        self
    }

    /// Set the suggestion for this issue.
    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }
}

/// Codes that describe the **build or the run**, not a document, and so are reported once per run
/// however many documents trigger them (issue #1156).
pub const RUN_SCOPED_CODES: &[&str] = &["I004"];

/// Result of linting one or more files.
#[derive(Debug, Default, Serialize)]
pub struct LintResult {
    /// All issues found.
    pub issues: Vec<LintIssue>,
    /// Number of files checked.
    pub files_checked: usize,
    /// Number of errors found.
    pub errors: usize,
    /// Number of warnings found.
    pub warnings: usize,
}

impl LintResult {
    /// Create a new empty lint result.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an issue to the result.
    pub fn add_issue(&mut self, issue: LintIssue) {
        match issue.severity {
            Severity::Error => self.errors += 1,
            Severity::Warning => self.warnings += 1,
            Severity::Info => {}
        }
        self.issues.push(issue);
    }

    /// Check if there are any errors.
    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }

    /// Check if there are any warnings.
    pub fn has_warnings(&self) -> bool {
        self.warnings > 0
    }

    /// Check if validation passed (no errors).
    pub fn is_valid(&self) -> bool {
        self.errors == 0
    }

    /// Merge another result into this one.
    pub fn merge(&mut self, other: LintResult) {
        self.files_checked += other.files_checked;
        self.absorb(other);
    }

    /// [`merge`](Self::merge) without adding `other`'s `files_checked` — for a caller that has
    /// already counted the files itself, as the CLI does.
    ///
    /// A finding in [`RUN_SCOPED_CODES`] describes the build rather than a document, so it is kept
    /// once however many documents report it: two files with JavaScript in an opt-out build are one
    /// `I004`, not two.
    ///
    /// The counters are summed, not recounted from the issues: `errors`/`warnings` are public, so a
    /// caller may have set them directly, and `merge` has always added them as given. Only a dropped
    /// duplicate's own contribution is taken back out.
    pub fn absorb(&mut self, other: LintResult) {
        self.errors += other.errors;
        self.warnings += other.warnings;
        for issue in other.issues {
            let already_reported = RUN_SCOPED_CODES.contains(&issue.code.as_str())
                && self.issues.iter().any(|i| i.code == issue.code);
            if already_reported {
                match issue.severity {
                    Severity::Error => self.errors = self.errors.saturating_sub(1),
                    Severity::Warning => self.warnings = self.warnings.saturating_sub(1),
                    Severity::Info => {}
                }
            } else {
                self.issues.push(issue);
            }
        }
    }
}

/// Options for validation.
#[derive(Debug, Clone, Default)]
pub struct LintOptions {
    /// Lint a document's text verbatim instead of rendering its EJS tags first, matching a file
    /// loaded with `rift --no-parse` (issue #1108).
    pub no_parse: bool,
}
