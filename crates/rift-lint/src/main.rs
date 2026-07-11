//! Rift Imposter Configuration Linter CLI
//!
//! This tool validates imposter configuration files for compatibility with Rift,
//! detecting common issues before loading them into the server.
//!
//! Usage:
//!   rift-lint <directory_or_file> [OPTIONS]

use clap::Parser;
use rift_lint::{LintIssue, LintOptions, LintResult, Severity, lint_file};
use serde_json::Value;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Runtime ANSI color codes. Resolved once in `main` from `NO_COLOR`/TTY/json-mode via
/// `Palette::detect`, then read anywhere via `palette()`. Fields are empty strings when color is
/// disabled, so `{green}`-style interpolation becomes a no-op instead of requiring call-site branching.
#[derive(Debug, Clone, Copy)]
struct Palette {
    green: &'static str,
    red: &'static str,
    yellow: &'static str,
    cyan: &'static str,
    bold: &'static str,
    dim: &'static str,
    reset: &'static str,
}

impl Palette {
    const PLAIN: Palette = Palette {
        green: "",
        red: "",
        yellow: "",
        cyan: "",
        bold: "",
        dim: "",
        reset: "",
    };

    /// Color is on only for an interactive text-mode session: never in `-o json` (stdout must be
    /// pure JSON), never with `NO_COLOR` set, and never when stdout is piped/redirected.
    fn detect(json_mode: bool) -> Self {
        let color =
            !json_mode && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
        if color {
            Palette {
                green: "\x1b[32m",
                red: "\x1b[31m",
                yellow: "\x1b[33m",
                cyan: "\x1b[36m",
                bold: "\x1b[1m",
                dim: "\x1b[2m",
                reset: "\x1b[0m",
            }
        } else {
            Palette::PLAIN
        }
    }
}

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// The process-wide palette, set once in `main` before anything is printed. Falls back to plain
/// (no color) rather than panicking if read before `main` initializes it.
fn palette() -> Palette {
    PALETTE.get().copied().unwrap_or(Palette::PLAIN)
}

/// Rift Imposter Configuration Linter
#[derive(Parser, Debug)]
#[command(name = "rift-lint")]
#[command(
    author,
    version,
    about = "Validate imposter configuration files for Rift compatibility"
)]
struct Args {
    /// Path to imposter file or directory containing imposter files
    #[arg(required = true)]
    path: PathBuf,

    /// Fix issues automatically where possible
    #[arg(short, long)]
    fix: bool,

    /// Output format: text (default), json
    #[arg(short, long, default_value = "text")]
    output: String,

    /// Only show errors (hide warnings)
    #[arg(short = 'e', long)]
    errors_only: bool,

    /// Strict mode - treat warnings as errors
    #[arg(short, long)]
    strict: bool,
}

/// Print to stdout in text mode, or stderr in json mode. In `-o json`, stdout is reserved
/// exclusively for the final `print_results_json` payload — every other message is decoration.
fn emit(json_mode: bool, msg: &str) {
    if json_mode {
        eprintln!("{msg}");
    } else {
        println!("{msg}");
    }
}

fn main() {
    let args = Args::parse();
    let json_mode = args.output == "json";
    let _ = PALETTE.set(Palette::detect(json_mode));
    let Palette {
        yellow,
        cyan,
        bold,
        dim,
        reset,
        ..
    } = palette();

    // The banner and scan progress are decoration, not data: always on stderr so stdout stays
    // clean in both json mode (pure JSON) and piped text mode (no banner noise).
    eprintln!("{bold}{cyan}Rift Imposter Linter{reset}");
    eprintln!("{dim}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━{reset}");

    let mut result = LintResult::default();
    let options = LintOptions::default();

    // Collect all imposter files
    let files = collect_imposter_files(&args.path);

    if files.is_empty() {
        emit(
            json_mode,
            &format!(
                "{yellow}Warning:{reset} No JSON files found in {:?}",
                args.path
            ),
        );
        // In json mode still emit a (zero) result so stdout is always valid JSON — a consumer
        // piping to `jq` shouldn't get empty input for the no-files case (issue #347).
        if json_mode {
            print_results_json(&result);
        }
        std::process::exit(0);
    }

    eprintln!("{dim}Scanning:{reset} {cyan}{}{reset}", args.path.display());
    eprintln!(
        "{dim}Found:{reset}    {bold}{}{reset} imposter file(s)\n",
        files.len()
    );
    result.files_checked = files.len();

    // First pass: Load all files and check for port conflicts
    let mut port_map: HashMap<u16, Vec<PathBuf>> = HashMap::new();
    let mut imposters: Vec<(PathBuf, Value)> = Vec::new();

    for file in &files {
        match load_imposter_file(file) {
            Ok(imposter) => {
                if let Some(port) = imposter.get("port").and_then(|v| v.as_u64()) {
                    port_map.entry(port as u16).or_default().push(file.clone());
                }
                imposters.push((file.clone(), imposter));
            }
            Err(e) => {
                result.add_issue(
                    LintIssue::error("E001", format!("Failed to parse JSON: {e}"), file.clone())
                        .with_suggestion("Check for JSON syntax errors"),
                );
            }
        }
    }

    // Check for port conflicts
    check_port_conflicts(&port_map, &mut result);

    // Second pass: Validate each imposter using the library
    for (file, _) in &imposters {
        let file_result = lint_file(file, &options);
        // Merge without double-counting files_checked (we already counted)
        result.issues.extend(file_result.issues);
        result.errors += file_result.errors;
        result.warnings += file_result.warnings;
    }

    // Print results
    if json_mode {
        print_results_json(&result);
    } else {
        print_results(&result, &args);
    }

    // Apply fixes if requested
    if args.fix && result.errors > 0 {
        emit(json_mode, &format!("\n{bold}Applying fixes...{reset}"));
        apply_fixes(&imposters, json_mode);
    }

    // Exit with error code if there were errors (or warnings in strict mode)
    let has_errors = result.errors > 0 || (args.strict && result.warnings > 0);
    std::process::exit(if has_errors { 1 } else { 0 });
}

fn collect_imposter_files(path: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    if path.is_file() {
        if path.extension().is_some_and(|ext| ext == "json") {
            files.push(path.to_path_buf());
        }
    } else if path.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_file() && entry_path.extension().is_some_and(|ext| ext == "json") {
                files.push(entry_path);
            }
        }
    }

    files.sort();
    files
}

/// Error loading and parsing an imposter file for linting.
#[derive(Debug, thiserror::Error)]
enum LoadError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

fn load_imposter_file(path: &Path) -> Result<Value, LoadError> {
    let content = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

fn check_port_conflicts(port_map: &HashMap<u16, Vec<PathBuf>>, result: &mut LintResult) {
    for (port, files) in port_map {
        if files.len() > 1 {
            let file_names: Vec<String> = files
                .iter()
                .map(|f| {
                    f.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .collect();

            result.add_issue(
                LintIssue::error(
                    "E002",
                    format!(
                        "Port {port} is used by {} files: {}",
                        files.len(),
                        file_names.join(", ")
                    ),
                    files[0].clone(),
                )
                .with_location("port")
                .with_suggestion(format!(
                    "Assign unique ports to each imposter. Consider using ports {}+",
                    port + 1
                )),
            );
        }
    }
}

fn print_results_json(result: &LintResult) {
    let output = serde_json::to_string_pretty(&result).unwrap();
    println!("{output}");
}

fn print_results(result: &LintResult, args: &Args) {
    let Palette {
        green,
        red,
        yellow,
        cyan,
        bold,
        dim,
        reset,
    } = palette();

    println!();

    if result.issues.is_empty() {
        println!("{green}{bold}No issues found!{reset}");
    } else {
        // Group issues by file
        let mut issues_by_file: HashMap<&PathBuf, Vec<&LintIssue>> = HashMap::new();
        for issue in &result.issues {
            issues_by_file.entry(&issue.file).or_default().push(issue);
        }

        // Sort files for consistent output
        let mut files: Vec<_> = issues_by_file.keys().collect();
        files.sort();

        for file in files {
            let issues = &issues_by_file[file];

            // Filter issues based on errors_only flag
            let filtered_issues: Vec<_> = if args.errors_only {
                issues
                    .iter()
                    .filter(|i| i.severity == Severity::Error)
                    .collect()
            } else {
                issues.iter().collect()
            };

            // Skip files with no relevant issues
            if filtered_issues.is_empty() {
                continue;
            }

            // Count errors and warnings for this file
            let file_errors = filtered_issues
                .iter()
                .filter(|i| i.severity == Severity::Error)
                .count();
            let file_warnings = filtered_issues
                .iter()
                .filter(|i| i.severity == Severity::Warning)
                .count();

            let file_name = file.file_name().unwrap_or_default().to_string_lossy();

            // File header with issue count
            let status_indicator = if file_errors > 0 {
                format!("{red}FAIL{reset}")
            } else {
                format!("{yellow}WARN{reset}")
            };

            let counts = if file_errors > 0 && file_warnings > 0 {
                format!(
                    " {dim}({red}{file_errors} error(s){reset}{dim}, {yellow}{file_warnings} warning(s){reset}{dim}){reset}"
                )
            } else if file_errors > 0 {
                format!(" {dim}({red}{file_errors} error(s){reset}{dim}){reset}")
            } else if file_warnings > 0 {
                format!(" {dim}({yellow}{file_warnings} warning(s){reset}{dim}){reset}")
            } else {
                String::new()
            };

            println!("{status_indicator} {bold}{cyan}{file_name}{reset}{counts}");

            for issue in filtered_issues {
                let severity_marker = match issue.severity {
                    Severity::Error => format!("{red}|{reset}"),
                    Severity::Warning => format!("{yellow}|{reset}"),
                    Severity::Info => format!("{cyan}|{reset}"),
                };

                let severity_str = format!(
                    "{bold}{}{}{reset}",
                    severity_color(&issue.severity),
                    issue.severity.label()
                );

                let location_str = issue
                    .location
                    .as_ref()
                    .map(|l| format!("{dim}[{reset}{cyan}{l}{reset}{dim}]{reset}"))
                    .unwrap_or_default();

                let code_str = format!(
                    "{dim}({}{}{dim}){reset}",
                    severity_color(&issue.severity),
                    issue.code
                );

                println!(
                    "  {severity_marker} {location_str} {severity_str}: {} {code_str}",
                    issue.message
                );

                if let Some(suggestion) = &issue.suggestion {
                    println!("  {severity_marker}   {green}-> {suggestion}{reset}");
                }
            }
            println!();
        }
    }

    // Summary
    println!("{dim}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━{reset}");
    println!("{bold}{cyan}Summary{reset}");
    println!("{dim}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━{reset}");
    println!(
        "  {dim}Files checked:{reset} {bold}{}{reset}",
        result.files_checked
    );

    // Errors count
    if result.errors > 0 {
        println!(
            "  {red}Errors:{reset}    {bold}{red}{}{reset}",
            result.errors
        );
    } else {
        println!("  {green}Errors:{reset}    {bold}{green}0{reset}");
    }

    // Warnings count
    if result.warnings > 0 {
        println!(
            "  {yellow}Warnings:{reset}  {bold}{yellow}{}{reset}",
            result.warnings
        );
    } else {
        println!("  {dim}Warnings:{reset}  {bold}0{reset}");
    }

    println!();

    if result.errors == 0 && result.warnings == 0 {
        println!("{green}{bold}All checks passed!{reset}");
    } else if result.errors == 0 {
        println!("{yellow}{bold}Passed with warnings{reset}");
    } else {
        println!("{red}{bold}Linting failed with errors{reset}");
    }
}

fn severity_color(severity: &Severity) -> &'static str {
    let p = palette();
    match severity {
        Severity::Error => p.red,
        Severity::Warning => p.yellow,
        Severity::Info => p.cyan,
    }
}

/// Rewrite a single header *value* only when it is actually invalid, returning a short
/// description of the fix (for reporting) or `None` when the value is already valid and left
/// untouched. A string-only array is a valid multi-value header (engine support since #238) and
/// is never rewritten; an array carrying a non-string element is stringified element-wise so it
/// stays a multi-value header (rather than being joined into one comma-string, which changes HTTP
/// semantics and dropped non-string elements — issue #547).
fn fix_header_value(value: &mut Value) -> Option<&'static str> {
    if let Some(arr) = value.as_array_mut() {
        if arr.iter().all(Value::is_string) {
            return None;
        }
        for element in arr.iter_mut() {
            if !element.is_string() {
                *element = Value::String(element.to_string());
            }
        }
        Some("array elements -> strings")
    } else if value.is_number() {
        *value = Value::String(value.to_string());
        Some("number -> string")
    } else if value.is_boolean() {
        let bool_str = if value.as_bool().unwrap_or(false) {
            "true"
        } else {
            "false"
        };
        *value = Value::String(bool_str.to_string());
        Some("boolean -> string")
    } else {
        None
    }
}

fn apply_fixes(imposters: &[(PathBuf, Value)], json_mode: bool) {
    let Palette {
        green, red, reset, ..
    } = palette();
    let mut fixes_applied = 0;

    for (file, imposter) in imposters {
        let mut modified = imposter.clone();
        let mut file_fixed = false;

        // Fix header values
        if let Some(stubs) = modified.get_mut("stubs").and_then(|v| v.as_array_mut()) {
            for stub in stubs {
                if let Some(responses) = stub.get_mut("responses").and_then(|v| v.as_array_mut()) {
                    for response in responses {
                        if let Some(is_response) = response.get_mut("is")
                            && let Some(headers) = is_response
                                .get_mut("headers")
                                .and_then(|v| v.as_object_mut())
                        {
                            for (name, value) in headers.iter_mut() {
                                if let Some(kind) = fix_header_value(value) {
                                    file_fixed = true;
                                    fixes_applied += 1;
                                    emit(json_mode, &format!("  Fixed header '{name}' {kind}"));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Write fixed file
        if file_fixed {
            match serde_json::to_string_pretty(&modified) {
                Ok(content) => {
                    if let Err(e) = std::fs::write(file, content) {
                        emit(
                            json_mode,
                            &format!("{red}Error writing {}: {e}{reset}", file.display()),
                        );
                    } else {
                        emit(
                            json_mode,
                            &format!("{green}Fixed: {}{reset}", file.display()),
                        );
                    }
                }
                Err(e) => {
                    emit(
                        json_mode,
                        &format!("{red}Error serializing {}: {e}{reset}", file.display()),
                    );
                }
            }
        }
    }

    emit(
        json_mode,
        &format!("\n{green}Applied {fixes_applied} fixes{reset}"),
    );
}

#[cfg(test)]
mod tests {
    use super::fix_header_value;
    use serde_json::{Value, json};

    #[test]
    fn fix_header_value_leaves_valid_string_array() {
        // #547 regression: a string-only array is a valid multi-value header and MUST NOT be
        // rewritten — joining it into "a=1, b=2" silently changes runtime HTTP semantics.
        let mut v = json!(["a=1", "b=2"]);
        assert_eq!(fix_header_value(&mut v), None);
        assert_eq!(v, json!(["a=1", "b=2"]));
    }

    #[test]
    fn fix_header_value_stringifies_invalid_array() {
        // A non-string element makes the array invalid (E018); fix by stringifying elements in
        // place, preserving the array (multi-value) and dropping nothing.
        let mut v = json!(["a=1", 2, true]);
        assert!(fix_header_value(&mut v).is_some());
        assert_eq!(v, json!(["a=1", "2", "true"]));
    }

    #[test]
    fn fix_header_value_number_to_string() {
        let mut v = json!(200);
        assert!(fix_header_value(&mut v).is_some());
        assert_eq!(v, Value::String("200".to_string()));
    }

    #[test]
    fn fix_header_value_boolean_to_string() {
        let mut v = json!(true);
        assert!(fix_header_value(&mut v).is_some());
        assert_eq!(v, Value::String("true".to_string()));
    }

    #[test]
    fn fix_header_value_plain_string_untouched() {
        let mut v = json!("text/html");
        assert_eq!(fix_header_value(&mut v), None);
        assert_eq!(v, json!("text/html"));
    }

    #[test]
    fn fix_header_value_empty_array_untouched() {
        // Vacuously all-string, matches the validator (no E018) — left alone, not "fixed".
        let mut v = json!([]);
        assert_eq!(fix_header_value(&mut v), None);
        assert_eq!(v, json!([]));
    }

    #[test]
    fn fix_header_value_nested_non_scalar_element_stringified_not_dropped() {
        // A non-scalar element is stringified to its JSON form rather than dropped — no data loss,
        // and the array length is preserved.
        let mut v = json!(["a=1", {"x": 1}, null]);
        assert!(fix_header_value(&mut v).is_some());
        assert_eq!(v, json!(["a=1", "{\"x\":1}", "null"]));
    }
}
