//! Rift Imposter Configuration Linter CLI
//!
//! This tool validates imposter configuration files for compatibility with Rift,
//! detecting common issues before loading them into the server.
//!
//! Usage:
//!   rift-lint <directory_or_file> [OPTIONS]

use clap::Parser;
use rift_lint::{lint_file, LintIssue, LintOptions, LintResult, Severity};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ANSI color codes
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

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

fn main() {
    let args = Args::parse();

    println!("{BOLD}{CYAN}Rift Imposter Linter{RESET}");
    println!("{DIM}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━{RESET}");

    let mut result = LintResult::default();
    let options = LintOptions::default();

    // Collect all imposter files
    let files = collect_imposter_files(&args.path);

    if files.is_empty() {
        println!(
            "{YELLOW}Warning:{RESET} No JSON files found in {:?}",
            args.path
        );
        std::process::exit(0);
    }

    println!("{DIM}Scanning:{RESET} {CYAN}{}{RESET}", args.path.display());
    println!(
        "{DIM}Found:{RESET}    {BOLD}{}{RESET} imposter file(s)\n",
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
    if args.output == "json" {
        print_results_json(&result);
    } else {
        print_results(&result, &args);
    }

    // Apply fixes if requested
    if args.fix && result.errors > 0 {
        println!("\n{BOLD}Applying fixes...{RESET}");
        apply_fixes(&imposters);
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
    } else if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let entry_path = entry.path();
                if entry_path.is_file() && entry_path.extension().is_some_and(|ext| ext == "json") {
                    files.push(entry_path);
                }
            }
        }
    }

    files.sort();
    files
}

fn load_imposter_file(path: &Path) -> Result<Value, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
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
    println!();

    if result.issues.is_empty() {
        println!("{GREEN}{BOLD}No issues found!{RESET}");
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
                format!("{RED}FAIL{RESET}")
            } else {
                format!("{YELLOW}WARN{RESET}")
            };

            let counts = if file_errors > 0 && file_warnings > 0 {
                format!(
                    " {DIM}({RED}{file_errors} error(s){RESET}{DIM}, {YELLOW}{file_warnings} warning(s){RESET}{DIM}){RESET}"
                )
            } else if file_errors > 0 {
                format!(" {DIM}({RED}{file_errors} error(s){RESET}{DIM}){RESET}")
            } else if file_warnings > 0 {
                format!(" {DIM}({YELLOW}{file_warnings} warning(s){RESET}{DIM}){RESET}")
            } else {
                String::new()
            };

            println!("{status_indicator} {BOLD}{CYAN}{file_name}{RESET}{counts}");

            for issue in filtered_issues {
                let severity_marker = match issue.severity {
                    Severity::Error => format!("{RED}|{RESET}"),
                    Severity::Warning => format!("{YELLOW}|{RESET}"),
                    Severity::Info => format!("{CYAN}|{RESET}"),
                };

                let severity_str = format!(
                    "{BOLD}{}{}{RESET}",
                    severity_color(&issue.severity),
                    issue.severity.label()
                );

                let location_str = issue
                    .location
                    .as_ref()
                    .map(|l| format!("{DIM}[{RESET}{CYAN}{l}{RESET}{DIM}]{RESET}"))
                    .unwrap_or_default();

                let code_str = format!(
                    "{DIM}({}{}{DIM}){RESET}",
                    severity_color(&issue.severity),
                    issue.code
                );

                println!(
                    "  {severity_marker} {location_str} {severity_str}: {} {code_str}",
                    issue.message
                );

                if let Some(suggestion) = &issue.suggestion {
                    println!("  {severity_marker}   {GREEN}-> {suggestion}{RESET}");
                }
            }
            println!();
        }
    }

    // Summary
    println!("{DIM}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━{RESET}");
    println!("{BOLD}{CYAN}Summary{RESET}");
    println!("{DIM}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━{RESET}");
    println!(
        "  {DIM}Files checked:{RESET} {BOLD}{}{RESET}",
        result.files_checked
    );

    // Errors count
    if result.errors > 0 {
        println!(
            "  {RED}Errors:{RESET}    {BOLD}{RED}{}{RESET}",
            result.errors
        );
    } else {
        println!("  {GREEN}Errors:{RESET}    {BOLD}{GREEN}0{RESET}");
    }

    // Warnings count
    if result.warnings > 0 {
        println!(
            "  {YELLOW}Warnings:{RESET}  {BOLD}{YELLOW}{}{RESET}",
            result.warnings
        );
    } else {
        println!("  {DIM}Warnings:{RESET}  {BOLD}0{RESET}");
    }

    println!();

    if result.errors == 0 && result.warnings == 0 {
        println!("{GREEN}{BOLD}All checks passed!{RESET}");
    } else if result.errors == 0 {
        println!("{YELLOW}{BOLD}Passed with warnings{RESET}");
    } else {
        println!("{RED}{BOLD}Linting failed with errors{RESET}");
    }
}

fn severity_color(severity: &Severity) -> &'static str {
    match severity {
        Severity::Error => RED,
        Severity::Warning => YELLOW,
        Severity::Info => CYAN,
    }
}

fn apply_fixes(imposters: &[(PathBuf, Value)]) {
    let mut fixes_applied = 0;

    for (file, imposter) in imposters {
        let mut modified = imposter.clone();
        let mut file_fixed = false;

        // Fix header values
        if let Some(stubs) = modified.get_mut("stubs").and_then(|v| v.as_array_mut()) {
            for stub in stubs {
                if let Some(responses) = stub.get_mut("responses").and_then(|v| v.as_array_mut()) {
                    for response in responses {
                        if let Some(is_response) = response.get_mut("is") {
                            if let Some(headers) = is_response
                                .get_mut("headers")
                                .and_then(|v| v.as_object_mut())
                            {
                                for (name, value) in headers.iter_mut() {
                                    if value.is_array() {
                                        // Convert array to comma-separated string
                                        if let Some(arr) = value.as_array() {
                                            let joined: Vec<String> = arr
                                                .iter()
                                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                                .collect();
                                            *value = Value::String(joined.join(", "));
                                            file_fixed = true;
                                            fixes_applied += 1;
                                            println!("  Fixed header '{name}' array -> string");
                                        }
                                    } else if value.is_number() {
                                        *value = Value::String(value.to_string());
                                        file_fixed = true;
                                        fixes_applied += 1;
                                        println!("  Fixed header '{name}' number -> string");
                                    } else if value.is_boolean() {
                                        let bool_str = if value.as_bool().unwrap_or(false) {
                                            "true"
                                        } else {
                                            "false"
                                        };
                                        *value = Value::String(bool_str.to_string());
                                        file_fixed = true;
                                        fixes_applied += 1;
                                        println!("  Fixed header '{name}' boolean -> string");
                                    }
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
                        println!("{RED}Error writing {}: {e}{RESET}", file.display());
                    } else {
                        println!("{GREEN}Fixed: {}{RESET}", file.display());
                    }
                }
                Err(e) => {
                    println!("{RED}Error serializing {}: {e}{RESET}", file.display());
                }
            }
        }
    }

    println!("\n{GREEN}Applied {fixes_applied} fixes{RESET}");
}
