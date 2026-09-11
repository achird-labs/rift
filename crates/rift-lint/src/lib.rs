//! Configuration linting library for Rift HTTP Proxy.
//!
//! This library provides validation capabilities for Mountebank-compatible
//! imposter configurations. It can be used as a standalone library or through
//! the `rift-lint` CLI binary.
//!
//! # Example
//!
//! ```
//! use rift_lint::{lint_json, LintOptions};
//!
//! let config = r#"{
//!     "port": 3000,
//!     "protocol": "http",
//!     "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }]
//! }"#;
//!
//! let result = lint_json(config, "imposter.json", &LintOptions::default());
//! assert!(!result.has_errors());
//! ```

mod duplicate_keys;
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

/// A document's parsed value together with what only its raw text could reveal.
///
/// `value` is the collapsed [`serde_json::Value`] every rule validates. The duplicate-key list is
/// private and [`parse_document`] is its only constructor, so the two are consistent **as
/// constructed** — they come from two reads of one text, and pairing them is the whole point of the
/// type. `value` is public for callers that need the parse; replacing it does not update the
/// duplicate list.
#[derive(Debug)]
pub struct Document {
    /// The parsed document. Byte-identical duplicate keys have already been collapsed here.
    pub value: serde_json::Value,
    duplicates: Vec<duplicate_keys::Duplicate>,
}

impl Document {
    /// Every byte-identical repeated key the raw text carried, as `(containing object, key)`.
    ///
    /// Document-wide, and deliberately broader than `E044`'s two header fields: anything that
    /// rewrites the file from [`Document::value`] loses **all** of them, not only the ones the
    /// engine rejects. The repeat that matters most here is the one `E044` pointedly does *not*
    /// report — a repeated name in `is.headers`, which the engine merges into two header lines on
    /// purpose, so nothing else would report its loss.
    ///
    /// The location is `None` for the document's root object, which has no path to name.
    pub fn duplicate_keys(&self) -> impl Iterator<Item = (Option<&str>, &str)> + '_ {
        self.duplicates
            .iter()
            .map(|d| (d.location.as_deref(), d.key.as_str()))
    }
}

/// Parse `text` into a [`Document`], recording byte-identical duplicate keys before they collapse.
///
/// # Errors
///
/// Returns the `serde_json::Error` for text that is not valid JSON, carrying the same line and
/// column `serde_json::from_str` would report.
pub fn parse_document(text: &str) -> Result<Document, serde_json::Error> {
    let duplicates = duplicate_keys::find(text)?;
    let value = serde_json::from_str(text)?;
    Ok(Document { value, duplicates })
}

/// Lint a [`Document`], reporting everything [`lint_value`] does plus `E044`.
///
/// Prefer this over [`lint_value`] wherever the raw text is available: it is the only entry point
/// that can see a byte-identical duplicate key.
pub fn lint_document(doc: &Document, source_name: &str, options: &LintOptions) -> LintResult {
    lint_document_at(doc, Path::new(source_name), options)
}

/// The `&Path`-taking core of [`lint_document`].
///
/// [`lint_file`] has a real `&Path` in hand and must not round-trip it through `to_string_lossy`:
/// on a filesystem that allows non-UTF-8 filenames that substitutes U+FFFD, and every finding then
/// names a path that no longer matches the file it came from.
fn lint_document_at(doc: &Document, path: &Path, options: &LintOptions) -> LintResult {
    let mut result = LintResult::new();
    result.files_checked = 1;

    let single_valued = single_valued_header_locations(&doc.value);
    for duplicate in &doc.duplicates {
        let Some(location) = duplicate.location.as_deref() else {
            continue;
        };
        if !single_valued.contains(location) {
            continue;
        }
        result.add_issue(
            LintIssue::error(
                "E044",
                format!(
                    "Header '{}' is given twice in {location}; a single-valued header object \
                     names each header once (the engine rejects this document on every text path, \
                     and silently keeps the last value through --configfile's object forms)",
                    duplicate.key
                ),
                path.to_path_buf(),
            )
            .with_location(location)
            .with_suggestion("Remove the duplicate header"),
        );
    }

    validate_config(path, &doc.value, &mut result, options);
    result
}

/// Locations of every single-valued header object in `value`, in the dotted/indexed form the
/// duplicate scan produces (wrapper included, since the scan sees the document from its root).
///
/// E044 is confined to these. They are the only objects where a byte-identical repeated key is
/// actually rejected: both deserialize through `single_value_headers`. Elsewhere a repeat is either
/// **deliberate** — `is.headers` is multi-valued and *merges* a repeated key into two header lines,
/// which is how two `Set-Cookie`s are sent (`rift_types::wire`'s
/// `headers_merge_byte_identical_duplicate_keys` pins it) — or accepted last-wins, as in a free-form
/// `is.body`, which is a `serde_json::Value`. Reporting those would flag working documents.
///
/// A repeated *known struct field* (`{"port": 3000, "port": 3001}`) is also rejected by the engine,
/// but recognising one needs the schema; it is deliberately not covered here.
fn single_valued_header_locations(value: &serde_json::Value) -> std::collections::HashSet<String> {
    fn collect(
        imposter: &serde_json::Value,
        prefix: &str,
        out: &mut std::collections::HashSet<String>,
    ) {
        let Some(stubs) = imposter.get("stubs").and_then(serde_json::Value::as_array) else {
            return;
        };
        for (i, stub) in stubs.iter().enumerate() {
            let Some(responses) = stub.get("responses").and_then(serde_json::Value::as_array)
            else {
                continue;
            };
            for (j, response) in responses.iter().enumerate() {
                let base = format!("{prefix}stubs[{i}].responses[{j}]");
                if response
                    .get("proxy")
                    .and_then(|p| p.get("injectHeaders"))
                    .is_some()
                {
                    out.insert(format!("{base}.proxy.injectHeaders"));
                }
                if response
                    .get("_rift")
                    .and_then(|r| r.get("fault"))
                    .and_then(|f| f.get("error"))
                    .and_then(|e| e.get("headers"))
                    .is_some()
                {
                    out.insert(format!("{base}._rift.fault.error.headers"));
                }
            }
        }
    }

    // The same three document shapes `validate_config` accepts.
    let mut out = std::collections::HashSet::new();
    if let Some(arr) = value.get("imposters").and_then(serde_json::Value::as_array) {
        for (i, imposter) in arr.iter().enumerate() {
            collect(imposter, &format!("imposters[{i}]."), &mut out);
        }
    } else if let Some(arr) = value.as_array() {
        for (i, imposter) in arr.iter().enumerate() {
            collect(imposter, &format!("[{i}]."), &mut out);
        }
    } else {
        collect(value, "", &mut out);
    }
    out
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

    // Through the text path rather than parsing here, so a file gets E044 too — and with the real
    // `&Path`, so a non-UTF-8 filename still names itself exactly in every finding.
    lint_text(&content, path, options)
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
    lint_text(json, Path::new(source_name), options)
}

/// The `&Path`-taking core of [`lint_json`]; see [`lint_document_at`] for why the path stays a path.
fn lint_text(text: &str, path: &Path, options: &LintOptions) -> LintResult {
    match parse_document(text) {
        Ok(doc) => lint_document_at(&doc, path, options),
        Err(e) => {
            let mut result = LintResult::new();
            result.files_checked = 1;
            result.add_issue(LintIssue::error(
                // E001, not E002 (issue #1008). E002 is the port conflict, which is what the CLI
                // and the published table have always meant by it; this entry point had assigned
                // the two codes the other way round, so a library caller who looked up E002 read
                // "port conflict" for a JSON syntax error.
                "E001",
                format!("Invalid JSON: {e}"),
                path.to_path_buf(),
            ));
            result
        }
    }
}

/// Lint a parsed JSON value directly (useful when you already have parsed JSON).
///
/// Returns a `LintResult` containing all issues found.
///
/// **Cannot report `E044`.** A byte-identical duplicate key is already collapsed by the time a
/// `Value` exists, so this entry point is structurally blind to it. Use [`parse_document`] +
/// [`lint_document`] (or [`lint_json`] / [`lint_file`], which do) when the raw text is available.
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
