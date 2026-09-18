//! Configuration linting library for Rift imposter files.
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
mod number_fidelity;
mod types;
mod validator;

use std::borrow::Cow;
use std::path::Path;

// Re-export public types
pub use number_fidelity::LossyNumber;
pub use types::{LintIssue, LintOptions, LintResult, RUN_SCOPED_CODES, Severity};

// Re-export validation functions for advanced usage
pub use validator::{
    is_javascript_decorate, validate_behavior, validate_headers, validate_imposter,
    validate_is_response, validate_predicate, validate_proxy_response, validate_response,
    validate_stub,
};

/// The two document formats `rift-lint` understands (issue #1071).
///
/// The only place the extension-to-format mapping lives; everything else asks [`format_of`].
#[derive(Debug, Clone, Copy)]
enum Format {
    Json,
    Yaml,
}

/// `path`'s format, by extension: `.yaml`/`.yml` (case-insensitive) is [`Format::Yaml`], and
/// everything else — including no extension at all — is [`Format::Json`], preserving today's
/// behavior for every existing caller.
fn format_of(path: &Path) -> Format {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml") => {
            Format::Yaml
        }
        _ => Format::Json,
    }
}

/// Every explicit port seen in one lint run, with the document and imposter slot that declared it
/// (issue #1156). Feed it each document once with [`record`](Self::record), then ask for the
/// conflicts once with [`conflicts`](Self::conflicts).
///
/// This is the one implementation of `E002`. It used to live only in the CLI, so every library
/// entry point — and `lint_directory` in particular — lost the check the binary performs.
///
/// Only a port the engine would bind counts: an integer that fits in `u16` and is not `0`. An
/// absent, `null` or `0` port is auto-assigned by the engine and can never conflict (#1104); an
/// out-of-range one is `E005`'s to report and is skipped here rather than wrapped onto a real port
/// (#1091). Every imposter slot [`imposters_in`] finds counts, not just a top-level `port` (#1094).
#[derive(Debug, Default)]
pub struct PortUses(std::collections::BTreeMap<u16, Vec<(std::path::PathBuf, String)>>);

impl PortUses {
    /// Record every port `document` declares, attributing each to `source`.
    ///
    /// Call in a stable order across documents: `E002` is reported against the first declaration.
    pub fn record(&mut self, source: &Path, document: &serde_json::Value) {
        for (prefix, slot) in imposters_in(document) {
            if let Some(port) = slot
                .get("port")
                .and_then(serde_json::Value::as_u64)
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| *p != 0)
            {
                self.0
                    .entry(port)
                    .or_default()
                    .push((source.to_path_buf(), prefix));
            }
        }
    }

    /// One `E002` per port declared more than once, in ascending port order, each reported against
    /// the port's first declaration.
    #[must_use]
    pub fn conflicts(&self) -> Vec<LintIssue> {
        let mut issues = Vec::new();
        for (port, uses) in &self.0 {
            let [(first_file, first_prefix), _, ..] = uses.as_slice() else {
                continue;
            };

            // Group consecutive slots of the same file; `uses` is in file order, then slot order.
            let mut by_file: Vec<(&Path, Vec<&str>)> = Vec::new();
            for (file, prefix) in uses {
                let slot = prefix.strip_suffix('.').unwrap_or(prefix);
                match by_file.last_mut() {
                    Some((last, slots)) if *last == file.as_path() => slots.push(slot),
                    _ => by_file.push((file.as_path(), vec![slot])),
                }
            }
            let named: Vec<String> = by_file
                .iter()
                .map(|(file, slots)| {
                    let name = file.file_name().unwrap_or_default().to_string_lossy();
                    let slots: Vec<&str> =
                        slots.iter().copied().filter(|s| !s.is_empty()).collect();
                    if slots.is_empty() {
                        name.to_string()
                    } else {
                        format!("{name} ({})", slots.join(", "))
                    }
                })
                .collect();

            issues.push(
                LintIssue::error(
                    "E002",
                    format!(
                        "Port {port} is used by {} imposters: {}",
                        uses.len(),
                        named.join(", ")
                    ),
                    first_file.clone(),
                )
                .with_location(format!("{first_prefix}port"))
                .with_suggestion(match port.checked_add(1) {
                    Some(next) => {
                        format!(
                            "Assign unique ports to each imposter. Consider using ports {next}+"
                        )
                    }
                    None => "Assign unique ports to each imposter".to_string(),
                }),
            );
        }
        issues
    }
}

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
    lossy_numbers: Vec<LossyNumber>,
    /// Whether `--configfile` will read this text through its YAML branch, where the only shape
    /// that loads is a top-level sequence of imposters (`E046`).
    ///
    /// Keyed off the **content**, not the file extension, because that is what the engine keys off:
    /// `config_loader::parse_document` sniffs the first non-whitespace byte and sends `{` and `[`
    /// to `serde_json`, everything else to `serde_yaml`. A `.yaml` file holding a JSON object is
    /// therefore loaded happily by the engine, and must not be reported.
    yaml_sequence_required: bool,
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

    /// Every number literal the raw text carried that `serde_json` cannot write back
    /// digit-for-digit, in document order (issue #1080).
    ///
    /// [`Document::value`] holds such a literal as the nearest `f64` — `123456789012345678901234567890`
    /// is `1.2345678901234568e29` there — so anything that rewrites the file from it changes the
    /// number. A literal that only changes spelling (`0.10` → `0.1`, `1e2` → `100.0`) is not listed.
    ///
    /// Always empty for a document read by [`parse_yaml_document`] — even a `.yaml` file holding JSON
    /// text, which the engine reads through `serde_json` and rounds the same way. The scan is a JSON
    /// lexer, and `W012` documents the gap.
    pub fn lossy_numbers(&self) -> impl Iterator<Item = &LossyNumber> + '_ {
        self.lossy_numbers.iter()
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
    Ok(Document {
        value,
        duplicates,
        lossy_numbers: number_fidelity::find(text),
        yaml_sequence_required: false,
    })
}

/// Parse `text` as YAML into a [`Document`], recording byte-identical duplicate keys before they
/// collapse. The YAML sibling of [`parse_document`] — kept separate rather than folded into it
/// because the two fail with different error types (`serde_json::Error` is public API on
/// `parse_document` and cannot carry a YAML error).
///
/// # Errors
///
/// Returns the `serde_yaml::Error` for text that is not valid YAML, including a multi-document
/// stream (`serde_yaml` refuses those itself — the same answer `rift --configfile` gives).
pub fn parse_yaml_document(text: &str) -> Result<Document, serde_yaml::Error> {
    let duplicates = duplicate_keys::find_yaml(text)?;
    let value = serde_yaml::from_str::<serde_json::Value>(text)?;
    // The engine sniffs the first non-whitespace byte, not the extension: `{` and `[` go to
    // `serde_json`, everything else to `serde_yaml` (`config_loader::parse_document`). A `.yaml`
    // file holding a JSON document is loaded fine, so `E046` must not fire for it.
    let trimmed = text.trim_start();
    Ok(Document {
        value,
        duplicates,
        lossy_numbers: Vec::new(),
        yaml_sequence_required: !trimmed.starts_with('{') && !trimmed.starts_with('['),
    })
}

/// Lint a [`Document`], reporting everything [`lint_value`] does plus `E044`, `E046` and `W012`.
///
/// Prefer this over [`lint_value`] wherever the raw text is available: it is the only entry point
/// that can see a byte-identical duplicate key.
pub fn lint_document(doc: &Document, source_name: &str, options: &LintOptions) -> LintResult {
    lint_document_at(doc, Path::new(source_name), options, None)
}

/// [`lint_document`] for a caller that lints **several** documents as one run and owns the
/// port-conflict report (issue #1156): no `E002` is emitted here. Record each document with
/// [`PortUses::record`] and report [`PortUses::conflicts`] once, or a port repeated inside one file
/// is reported twice. The `rift-lint` binary does exactly this.
pub fn lint_document_in_run(
    doc: &Document,
    source_name: &str,
    options: &LintOptions,
) -> LintResult {
    lint_document_at(
        doc,
        Path::new(source_name),
        options,
        Some(&mut PortUses::default()),
    )
}

/// The `&Path`-taking core of [`lint_document`].
///
/// [`lint_file`] has a real `&Path` in hand and must not round-trip it through `to_string_lossy`:
/// on a filesystem that allows non-UTF-8 filenames that substitutes U+FFFD, and every finding then
/// names a path that no longer matches the file it came from.
fn lint_document_at(
    doc: &Document,
    path: &Path,
    options: &LintOptions,
    ports: Option<&mut PortUses>,
) -> LintResult {
    let mut result = LintResult::new();
    result.files_checked = 1;

    // E046. Emitted here rather than in `lint_text` because this is the one function every entry
    // point funnels through: the CLI calls `lint_document`, so a rule raised in `lint_text` would
    // be invisible on the path almost every user takes — which is the same shape of gap #1069 was
    // about.
    if doc.yaml_sequence_required && !doc.value.is_array() {
        result.add_issue(
            LintIssue::error(
                "E046",
                "A YAML config must be a sequence of imposters at the document root; \
                 rift --configfile reads a mapping root through its YAML branch, which only \
                 accepts a sequence (the {\"imposters\": [...]} wrapper, intercept and routes are \
                 JSON-only)",
                path.to_path_buf(),
            )
            .with_suggestion(
                "Start the file with \"- port: ...\"; a single imposter is a one-element sequence",
            ),
        );
    }

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

    // W012 (issue #1083). Here for the same reason as E046, and because only the raw text still has
    // the digits: `doc.value` already holds the rounded number.
    for number in &doc.lossy_numbers {
        result.add_issue(
            LintIssue::warning(
                "W012",
                format!(
                    "Number literal '{}' cannot be kept as written; the engine reads it as the \
                     nearest double, {}",
                    number.literal, number.written_as
                ),
                path.to_path_buf(),
            )
            .with_location(format!("line {}, column {}", number.line, number.column))
            .with_suggestion(format!(
                "Write {} if that is the value you mean; if a response body must carry the exact \
                 digits, give the body as a JSON string, which is sent verbatim",
                number.written_as
            )),
        );
    }

    // E050 (issue #1152). The engine refuses this block at the same door: a config file has no
    // response to return a generated CA key in.
    if doc
        .value
        .get("imposters")
        .and(doc.value.get("intercept"))
        .and_then(|block| block.get("returnCaKey"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        result.add_issue(
            LintIssue::error(
                "E050",
                "intercept.returnCaKey cannot be honoured from a config file: there is no response \
                 to return the generated CA key in, so the engine refuses the file",
                path.to_path_buf(),
            )
            .with_location("intercept.returnCaKey")
            .with_suggestion(
                "Start the listener with POST /intercept to receive a generated CA key, or supply \
                 caCertPath/caKeyPath",
            ),
        );
    }

    validate_config(path, &doc.value, &mut result, options);

    // E002 (issue #1156). Who reports it depends on who owns the run: a caller linting several
    // documents together passes its own collector and reports the conflicts once, at the end, so
    // a within-file conflict is not reported again here.
    match ports {
        Some(run) => run.record(path, &doc.value),
        None => {
            let mut own = PortUses::default();
            own.record(path, &doc.value);
            for issue in own.conflicts() {
                result.add_issue(issue);
            }
        }
    }
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

    let mut out = std::collections::HashSet::new();
    for (prefix, imposter) in imposters_in(value) {
        collect(imposter, &prefix, &mut out);
    }
    out
}

/// Every imposter `value` holds, each with the location prefix of its slot: `""` for a single
/// imposter, `imposters[i].` for the `{"imposters": [...]}` wrapper and `[i].` for a bare array.
///
/// These are the three document shapes `rift --configfile` loads, dispatched the way
/// `validate_config` dispatches them, so a prefix joined with a field name (`imposters[1].port`)
/// names the same slot the duplicate-key scan reports.
#[must_use]
pub fn imposters_in(value: &serde_json::Value) -> Vec<(String, &serde_json::Value)> {
    if let Some(arr) = value.get("imposters").and_then(serde_json::Value::as_array) {
        arr.iter()
            .enumerate()
            .map(|(i, imposter)| (format!("imposters[{i}]."), imposter))
            .collect()
    } else if let Some(arr) = value.as_array() {
        arr.iter()
            .enumerate()
            .map(|(i, imposter)| (format!("[{i}]."), imposter))
            .collect()
    } else {
        vec![(String::new(), value)]
    }
}

/// Lint a single imposter configuration file.
///
/// Returns a `LintResult` containing all issues found.
pub fn lint_file(path: &Path, options: &LintOptions) -> LintResult {
    lint_file_in(path, options, None)
}

/// [`lint_file`] with an optional run-wide port collector — see [`lint_document_at`].
fn lint_file_in(path: &Path, options: &LintOptions, ports: Option<&mut PortUses>) -> LintResult {
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
    lint_text(&content, path, format_of(path), options, ports)
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

    // Sorted: `read_dir` order is unspecified, and `E002` is reported against the first declaration
    // of a port, so an unsorted walk would name a different file from run to run (issue #1156).
    let mut files: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|file_path| {
            file_path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("json")
                        || ext.eq_ignore_ascii_case("yaml")
                        || ext.eq_ignore_ascii_case("yml")
                })
        })
        .collect();
    files.sort();

    // One collector for the whole directory, reported once: each file records into it rather than
    // reporting its own within-file conflicts, which would then be reported a second time here.
    let mut ports = PortUses::default();
    for file_path in &files {
        result.merge(lint_file_in(file_path, options, Some(&mut ports)));
    }
    for issue in ports.conflicts() {
        result.add_issue(issue);
    }

    result
}

/// Lint a JSON string directly (useful for in-memory validation).
///
/// Returns a `LintResult` containing all issues found.
pub fn lint_json(json: &str, source_name: &str, options: &LintOptions) -> LintResult {
    lint_text(json, Path::new(source_name), Format::Json, options, None)
}

/// Lint a YAML string directly (useful for in-memory validation) (issue #1071).
///
/// Returns a `LintResult` containing all issues found, including `E046` when the document's root
/// is not the sequence-of-imposters shape the engine's YAML path requires.
pub fn lint_yaml(yaml: &str, source_name: &str, options: &LintOptions) -> LintResult {
    lint_text(yaml, Path::new(source_name), Format::Yaml, options, None)
}

/// A document's text as the engine parses it, and what rendering it found.
#[derive(Debug)]
pub struct RenderedText<'a> {
    /// Borrowed when there was nothing to render.
    pub text: Cow<'a, str>,
    /// `W013` for each variable a tag could not substitute (unset with no default, or not valid
    /// Unicode).
    pub issues: Vec<LintIssue>,
}

impl RenderedText<'_> {
    /// True when the text had EJS tags that were rendered, so it is not what the file holds.
    #[must_use]
    pub fn was_rendered(&self) -> bool {
        matches!(self.text, Cow::Owned(_))
    }
}

/// Render `text`'s EJS tags the way the engine does before parsing a `--configfile` or `file:`
/// source (issue #1108), with the engine's own code, so the lint judges the document the engine
/// loads. With [`LintOptions::no_parse`], or with no tags, the text is returned as it is.
///
/// Includes resolve against `path`'s directory, and `process.env` is this process's environment.
///
/// # Errors
///
/// `E049` with the engine's message when the engine would refuse to load the file.
pub fn render_template<'a>(
    text: &'a str,
    path: &Path,
    options: &LintOptions,
) -> Result<RenderedText<'a>, Box<LintIssue>> {
    if options.no_parse || !rift_ejs::has_tags(text) {
        return Ok(RenderedText {
            text: Cow::Borrowed(text),
            issues: Vec::new(),
        });
    }
    let rendered = rift_ejs::render(text, path, rift_ejs::FileAccess::Allowed)
        .map_err(|e| LintIssue::error("E049", e.to_string(), path.to_path_buf()))?;
    let issues = rendered
        .unset_env
        .iter()
        .map(|unset| {
            let suggestion = match unset.reason {
                rift_ejs::EnvProblem::Unset => format!(
                    "Set {} where rift runs, or give the tag a default: \
                     <%= process.env.{} || 'value' %>",
                    unset.name, unset.name
                ),
                rift_ejs::EnvProblem::NotUnicode => format!(
                    "Set {} to a value that is valid Unicode where rift runs",
                    unset.name
                ),
            };
            LintIssue::warning(
                "W013",
                format!(
                    "{}; the engine does the same, and the document was linted that way",
                    unset.describe()
                ),
                path.to_path_buf(),
            )
            .with_suggestion(suggestion)
        })
        .collect();
    Ok(RenderedText {
        text: Cow::Owned(rendered.text),
        issues,
    })
}

/// The `&Path`-taking core of [`lint_json`] and [`lint_yaml`]; see [`lint_document_at`] for why
/// the path stays a path.
fn lint_text(
    text: &str,
    path: &Path,
    format: Format,
    options: &LintOptions,
    ports: Option<&mut PortUses>,
) -> LintResult {
    let rendered = match render_template(text, path, options) {
        Ok(rendered) => rendered,
        Err(issue) => {
            let mut result = LintResult::new();
            result.files_checked = 1;
            result.add_issue(*issue);
            return result;
        }
    };
    let mut result = lint_parsed_text(&rendered.text, path, format, options, ports);
    if rendered.was_rendered() {
        mark_rendered_positions(&mut result);
    }
    for issue in rendered.issues {
        result.add_issue(issue);
    }
    result
}

/// Say where a finding's line and column count from when the document was rendered from a template
/// (issue #1108): an `include` or a substitution moves them, so they locate the rendered text, not
/// the file on disk. Only `E001` and `W012` carry a line and column.
pub fn mark_rendered_positions(result: &mut LintResult) {
    for issue in &mut result.issues {
        match issue.code.as_str() {
            "E001" => issue
                .message
                .push_str(" (line and column are in the rendered document)"),
            "W012" => {
                if let Some(location) = issue.location.as_mut() {
                    location.push_str(" of the rendered document");
                }
            }
            _ => {}
        }
    }
}

fn lint_parsed_text(
    text: &str,
    path: &Path,
    format: Format,
    options: &LintOptions,
    ports: Option<&mut PortUses>,
) -> LintResult {
    match format {
        Format::Json => match parse_document(text) {
            Ok(doc) => lint_document_at(&doc, path, options, ports),
            Err(e) => {
                let mut result = LintResult::new();
                result.files_checked = 1;
                result.add_issue(LintIssue::error(
                    // E001, not E002 (issue #1008). E002 is the port conflict, which is what the
                    // CLI and the published table have always meant by it; this entry point had
                    // assigned the two codes the other way round, so a library caller who looked
                    // up E002 read "port conflict" for a JSON syntax error.
                    "E001",
                    format!("Invalid JSON: {e}"),
                    path.to_path_buf(),
                ));
                result
            }
        },
        Format::Yaml => match parse_yaml_document(text) {
            Ok(doc) => lint_document_at(&doc, path, options, ports),
            Err(e) => {
                let mut result = LintResult::new();
                result.files_checked = 1;
                result.add_issue(LintIssue::error(
                    // Same E001 as the JSON branch (issue #1008's reasoning applies equally
                    // here): a syntax error is not the port-conflict code.
                    "E001",
                    format!("Invalid YAML: {e}"),
                    path.to_path_buf(),
                ));
                result
            }
        },
    }
}

/// Lint a parsed JSON value directly (useful when you already have parsed JSON).
///
/// Returns a `LintResult` containing all issues found.
///
/// **Cannot report `E044` or `W012`.** A byte-identical duplicate key is already collapsed, and a
/// number too wide or too precise for a double already rounded, by the time a `Value` exists, so
/// this entry point is structurally blind to both. Use [`parse_document`] +
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
    let mut ports = PortUses::default();
    ports.record(path, value);
    for issue in ports.conflicts() {
        result.add_issue(issue);
    }
    result
}
