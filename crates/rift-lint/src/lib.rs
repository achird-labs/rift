//! Configuration linting library for Rift HTTP Proxy.
//!
//! This library provides validation capabilities for Mountebank-compatible
//! imposter configurations. It can be used as a standalone library or through
//! the `rift-lint` CLI binary.
//!
//! # Example
//!
//! ```no_run
//! use rift_lint::{lint_file, lint_directory, LintOptions, LintResult};
//! use std::path::Path;
//!
//! // Lint a single file
//! let result = lint_file(Path::new("imposter.json"), &LintOptions::default());
//!
//! // Lint a directory
//! let result = lint_directory(Path::new("./imposters"), &LintOptions::default());
//!
//! if result.has_errors() {
//!     eprintln!("Found {} errors", result.errors);
//! }
//! ```

mod types;
mod validator;

use std::path::Path;

// Re-export public types
pub use types::{LintIssue, LintOptions, LintResult, Severity};

// Re-export validation functions for advanced usage
pub use validator::{
    validate_behavior, validate_headers, validate_imposter, validate_is_response,
    validate_predicate, validate_proxy_response, validate_response, validate_stub,
};

/// Validate a parsed config value, accepting the same shapes `rift --configfile` accepts:
/// a single imposter object, a `{"imposters": [...]}` wrapper, or a bare `[...]` array.
/// Each imposter is validated individually so the wrapper itself isn't mistaken for one.
fn validate_config(
    path: &Path,
    value: &serde_json::Value,
    result: &mut LintResult,
    options: &LintOptions,
) {
    let imposters = value
        .get("imposters")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array());
    match imposters {
        Some(arr) => {
            for imposter in arr {
                validate_imposter(path, imposter, result, options);
            }
        }
        None => validate_imposter(path, value, result, options),
    }
}

/// Lint a single imposter configuration file.
///
/// Returns a `LintResult` containing all issues found.
pub fn lint_file(path: &Path, options: &LintOptions) -> LintResult {
    let mut result = LintResult::new();
    result.files_checked = 1;

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            result.add_issue(LintIssue::error(
                "E001",
                format!("Failed to read file: {e}"),
                path.to_path_buf(),
            ));
            return result;
        }
    };

    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            result.add_issue(LintIssue::error(
                "E002",
                format!("Invalid JSON: {e}"),
                path.to_path_buf(),
            ));
            return result;
        }
    };

    validate_config(path, &value, &mut result, options);
    result
}

/// Lint all JSON files in a directory (non-recursive).
///
/// Returns a `LintResult` containing all issues found across all files.
pub fn lint_directory(path: &Path, options: &LintOptions) -> LintResult {
    let mut result = LintResult::new();

    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => {
            result.add_issue(LintIssue::error(
                "E001",
                format!("Failed to read directory: {e}"),
                path.to_path_buf(),
            ));
            return result;
        }
    };

    for entry in entries.flatten() {
        let file_path = entry.path();
        if file_path.extension().map(|e| e == "json").unwrap_or(false) {
            let file_result = lint_file(&file_path, options);
            result.merge(file_result);
        }
    }

    result
}

/// Lint a JSON string directly (useful for in-memory validation).
///
/// Returns a `LintResult` containing all issues found.
pub fn lint_json(json: &str, source_name: &str, options: &LintOptions) -> LintResult {
    let mut result = LintResult::new();
    result.files_checked = 1;

    let path = Path::new(source_name);

    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            result.add_issue(LintIssue::error(
                "E002",
                format!("Invalid JSON: {e}"),
                path.to_path_buf(),
            ));
            return result;
        }
    };

    validate_config(path, &value, &mut result, options);
    result
}

/// Lint a parsed JSON value directly (useful when you already have parsed JSON).
///
/// Returns a `LintResult` containing all issues found.
pub fn lint_value(
    value: &serde_json::Value,
    source_name: &str,
    options: &LintOptions,
) -> LintResult {
    let mut result = LintResult::new();
    result.files_checked = 1;

    let path = Path::new(source_name);
    validate_config(path, value, &mut result, options);
    result
}
