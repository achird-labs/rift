//! Every feature file in the specification parses with the runner's parser (issue #1466), and the
//! runner runs every one (#1350). gherkin-rs is stricter than official Gherkin (a table cell accepts
//! only the escapes `\n`, `\|` and `\\`); this catches a file it cannot read, or one nobody added
//! to `spec::PORTED_FEATURES`, on the PR that adds it.

mod common;

use rift_conformance_runner::spec::{PORTED_FEATURES, features_dir};

fn feature_files() -> Vec<std::path::PathBuf> {
    let dir = features_dir();
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "feature"))
        .collect();
    paths.sort();
    paths
}

#[test]
fn the_runner_runs_every_feature_on_disk() {
    let on_disk: Vec<String> = feature_files()
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let mut ported: Vec<String> = PORTED_FEATURES.iter().map(|f| (*f).to_string()).collect();
    ported.sort();
    assert_eq!(
        ported, on_disk,
        "spec::PORTED_FEATURES must list exactly the feature files under features/rift"
    );
}

#[test]
fn every_feature_parses_with_the_runner_parser() {
    let dir = features_dir();
    let paths = feature_files();
    assert!(
        !paths.is_empty(),
        "no .feature files under {}",
        dir.display()
    );

    let failures: Vec<String> = paths
        .iter()
        .filter_map(|p| {
            common::parse_feature(p).err().map(|e| {
                let name = p.file_name().map_or(p.as_os_str(), |n| n).to_string_lossy();
                format!("{name}: {}", with_sources(&e))
            })
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} features do not parse:\n{}",
        failures.len(),
        paths.len(),
        failures.join("\n")
    );
}

/// The error and every `source()` below it: gherkin-rs puts the line and column of a parse
/// failure in the innermost one, which the outer `Display` leaves out.
fn with_sources(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut next = e.source();
    while let Some(cause) = next {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        next = cause.source();
    }
    out
}
