//! Issue #1156: `E002` (port conflict) was computed only by the CLI, in `main.rs`. No library entry
//! point had an equivalent — so every `lint_directory` caller (in-repo, the sdk-conformance corpus
//! gate) and every caller handing a multi-imposter document to `lint_json` / `lint_yaml` /
//! `lint_value` / `lint_file` silently lost the check the CLI performs.
//!
//! One collector, `PortUses`, now serves every entry point and the CLI. These pin the library side;
//! `tests/cli_output.rs` pins the CLI side and must pass unchanged.

use rift_lint::{LintOptions, LintResult, lint_directory, lint_file, lint_json, lint_value};
use serde_json::json;

fn e002s(result: &LintResult) -> Vec<&rift_lint::LintIssue> {
    result.issues.iter().filter(|i| i.code == "E002").collect()
}

fn opts() -> LintOptions {
    LintOptions::default()
}

// A multi-imposter document that repeats a port, linted as one document.
#[test]
fn lint_json_reports_a_port_repeated_inside_one_document() {
    let doc = json!({ "imposters": [
        { "port": 4545, "protocol": "http", "stubs": [] },
        { "port": 4545, "protocol": "http", "stubs": [] }
    ] })
    .to_string();
    let result = lint_json(&doc, "one.json", &opts());
    let hits = e002s(&result);
    assert_eq!(
        hits.len(),
        1,
        "exactly one E002 per port, got {:?}",
        result.issues
    );
    assert_eq!(
        hits[0].location.as_deref(),
        Some("imposters[0].port"),
        "reported against the first declaration"
    );
}

// The same through the value entry point, which takes a different path (`validate_config`).
#[test]
fn lint_value_reports_a_repeated_port_too() {
    let value = json!({ "imposters": [
        { "port": 4545, "protocol": "http", "stubs": [] },
        { "port": 4545, "protocol": "http", "stubs": [] }
    ] });
    assert_eq!(e002s(&lint_value(&value, "one.json", &opts())).len(), 1);
}

// Only ports E005 accepts count: an absent, null or 0 port is auto-assigned by the engine and never
// conflicts (#1104); an out-of-range port is skipped, not wrapped onto a real one (#1091).
#[test]
fn auto_assigned_and_out_of_range_ports_never_conflict() {
    let doc = json!({ "imposters": [
        { "protocol": "http", "stubs": [] },
        { "protocol": "http", "stubs": [] },
        { "port": null, "protocol": "http", "stubs": [] },
        { "port": null, "protocol": "http", "stubs": [] },
        { "port": 0, "protocol": "http", "stubs": [] },
        { "port": 0, "protocol": "http", "stubs": [] },
        { "port": 70000, "protocol": "http", "stubs": [] },
        { "port": 70000, "protocol": "http", "stubs": [] }
    ] })
    .to_string();
    assert!(
        e002s(&lint_json(&doc, "one.json", &opts())).is_empty(),
        "none of these may conflict"
    );
}

// A single-imposter document cannot conflict with itself.
#[test]
fn a_single_imposter_file_has_no_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a.json");
    std::fs::write(
        &path,
        json!({ "port": 4545, "protocol": "http", "stubs": [] }).to_string(),
    )
    .expect("write");
    assert!(e002s(&lint_file(&path, &opts())).is_empty());
}

fn write(dir: &std::path::Path, name: &str, value: &serde_json::Value) {
    std::fs::write(dir.join(name), value.to_string()).expect("write");
}

// Across files, and deterministically: `read_dir` order is unspecified, so without sorting the
// "first declaration" an E002 is reported against would change from run to run.
#[test]
fn lint_directory_reports_a_cross_file_conflict_against_the_first_file_every_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Written in reverse name order, so an unsorted walk is likely to see `b` first.
    write(
        dir.path(),
        "b.json",
        &json!({ "port": 4545, "protocol": "http", "stubs": [] }),
    );
    write(
        dir.path(),
        "a.json",
        &json!({ "port": 4545, "protocol": "http", "stubs": [] }),
    );

    for _ in 0..5 {
        let result = lint_directory(dir.path(), &opts());
        let hits = e002s(&result);
        assert_eq!(hits.len(), 1, "one E002 per port, got {:?}", result.issues);
        assert!(
            hits[0].file.ends_with("a.json"),
            "reported against the lexicographically first file, got {:?}",
            hits[0].file
        );
    }
}

// A within-file conflict inside a directory must be reported once — not once by the per-document
// check and again by the directory-wide one.
#[test]
fn lint_directory_reports_a_within_file_conflict_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        "a.json",
        &json!({ "imposters": [
            { "port": 4545, "protocol": "http", "stubs": [] },
            { "port": 4545, "protocol": "http", "stubs": [] }
        ] }),
    );
    let result = lint_directory(dir.path(), &opts());
    assert_eq!(
        e002s(&result).len(),
        1,
        "a within-file conflict is one finding, got {:?}",
        result.issues
    );
}
