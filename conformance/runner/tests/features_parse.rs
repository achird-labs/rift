//! Every feature in the specification parses with the runner's parser (issue #1466), not just the
//! ones `spec::PORTED_FEATURES` lists today. gherkin-rs is stricter than zio-bdd and official
//! Gherkin (a table cell accepts only the escapes `\n`, `\|` and `\\`), so a feature written
//! against the Scala runner can parse there and fail here; this catches it on the PR that adds it.

mod common;

use rift_conformance_runner::spec::features_dir;

#[test]
fn every_feature_parses_with_the_runner_parser() {
    let dir = features_dir();
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "feature"))
        .collect();
    paths.sort();
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
