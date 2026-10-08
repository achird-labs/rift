//! Mountebank is the oracle (issue #1341): every case in `cases/` and every Mountebank-drivable
//! SDK-corpus fixture runs on Mountebank and on Rift, and every answer and stored imposter must
//! match, except where `allowlist.json` names the exact difference.
//!
//! Skipped (with a message) when Mountebank is not installed, unless `RIFT_DIFFERENTIAL_REQUIRE_MB`
//! is set — CI's `differential` job sets it so a missing oracle is a failure, not a pass.
//! `RIFT_DIFFERENTIAL_FILTER=<substring>` runs only the cases whose name contains it, and
//! `RIFT_DIFFERENTIAL_DUMP=<file>` writes every difference found (explained or not) as JSON — the
//! raw material for a precise allow-list entry.

use rift_differential::allow::{self, Rule};
use rift_differential::case::{Case, CaseFile, Step, fixture_case};
use rift_differential::driver::{Engines, run_case};
use rift_differential::engine::{self, client};
use std::fmt::Write as _;
use std::path::PathBuf;

fn harness_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    harness_dir().join("../..")
}

fn corpus_dir() -> PathBuf {
    repo_root().join("sdk-conformance/corpus")
}

fn load_cases() -> Vec<Case> {
    let dir = harness_dir().join("cases");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.expect("readable cases/ entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    let mut cases = Vec::new();
    for path in files {
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let file: CaseFile =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            !file.cases.is_empty() || !file.fixtures.is_empty(),
            "{}: no cases and no fixtures",
            path.display()
        );
        cases.extend(file.cases);
        for fixture in &file.fixtures {
            cases.push(
                fixture_case(&corpus_dir().join("imposters"), fixture)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display())),
            );
        }
    }
    cases
}

fn load_allow_list() -> Vec<Rule> {
    allow::load(&harness_dir().join("allowlist.json")).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn every_case_parses_and_names_are_unique() {
    let cases = load_cases();
    let mut names = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(
            names.insert(case.name.as_str()),
            "duplicate case name {:?}",
            case.name
        );
        assert!(!case.steps.is_empty(), "{:?} has no steps", case.name);
        for step in &case.steps {
            let method = match step {
                Step::Admin { method, .. } => method,
                Step::Send(send) | Step::Concurrent(send) => &send.method,
                Step::Reimport => continue,
            };
            assert!(
                reqwest::Method::from_bytes(method.as_bytes()).is_ok(),
                "{:?}: invalid HTTP method {method:?}",
                case.name
            );
        }
    }
    assert!(cases.len() >= 150, "corpus shrank to {} cases", cases.len());
}

/// Every corpus fixture is either driven or excluded with a reason, and every retired feature
/// file and the targeted cases are still in the corpus — so neither can shrink unnoticed.
#[test]
fn the_corpus_covers_every_source() {
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("sdk-conformance/manifest.json"))
            .expect("read sdk-conformance/manifest.json"),
    )
    .expect("manifest is JSON");
    let in_manifest: std::collections::BTreeSet<String> = manifest["fixtures"]
        .as_array()
        .expect("manifest fixtures")
        .iter()
        .filter_map(|f| f["file"].as_str())
        .map(|f| f.trim_start_matches("corpus/imposters/").to_string())
        .collect();
    let file: CaseFile = serde_json::from_str(
        &std::fs::read_to_string(harness_dir().join("cases/sdk-corpus.json"))
            .expect("read sdk-corpus.json"),
    )
    .expect("sdk-corpus.json parses");
    let accounted: std::collections::BTreeSet<String> = file
        .fixtures
        .iter()
        .map(|f| f.file.clone())
        .chain(file.not_driven.iter().map(|n| n.file.clone()))
        .collect();
    assert_eq!(
        accounted, in_manifest,
        "sdk-corpus.json must list every manifest fixture"
    );
    assert_eq!(file.fixtures.len(), 9, "the Mountebank-drivable fixtures");

    let prefixes: std::collections::BTreeSet<String> = load_cases()
        .iter()
        .filter_map(|c| c.name.split_once(": ").map(|(p, _)| p.to_string()))
        .collect();
    for prefix in [
        "admin_api",
        "alternative_formats",
        "complex_scenarios",
        "mountebank_compatibility_gaps",
        "predicates",
        "proxy",
        "recording",
        "responses",
        "stub_management",
        "sdk-corpus",
        "targeted",
    ] {
        assert!(prefixes.contains(prefix), "no case comes from {prefix}");
    }
}

#[test]
fn allow_list_is_valid_and_its_citations_resolve() {
    let rules = load_allow_list();
    allow::check_citations(&rules, &repo_root()).unwrap_or_else(|e| panic!("{e}"));
    let names: std::collections::BTreeSet<String> =
        load_cases().into_iter().map(|c| c.name).collect();
    for entry in rules.iter().map(|r| &r.entry) {
        assert!(
            entry.case == "*" || names.contains(&entry.case),
            "{}: names unknown case {:?}",
            entry.id,
            entry.case
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mountebank_is_the_oracle() {
    let Some(mb) = engine::locate_mountebank() else {
        assert!(
            std::env::var_os("RIFT_DIFFERENTIAL_REQUIRE_MB").is_none(),
            "RIFT_DIFFERENTIAL_REQUIRE_MB is set but no Mountebank was found \
             (RIFT_MB_BIN, ~/bench-mb/node_modules/.bin/mb, or `mb` on PATH)"
        );
        eprintln!(
            "SKIP mountebank_is_the_oracle: no Mountebank found. Install it with \
             `npm install --prefix ~/bench-mb mountebank@2.9.1` or set RIFT_MB_BIN."
        );
        return;
    };
    let rift_bin = engine::locate_rift().unwrap_or_else(|e| panic!("{e}"));
    let cwd = corpus_dir();
    let engines = Engines {
        mountebank: engine::start_mountebank(&mb, &cwd)
            .await
            .unwrap_or_else(|e| panic!("{e}")),
        rift: engine::start_rift(&rift_bin, &cwd)
            .await
            .unwrap_or_else(|e| panic!("{e}")),
    };
    let client = client().expect("http client");

    let filter = std::env::var("RIFT_DIFFERENTIAL_FILTER").ok();
    let cases: Vec<Case> = load_cases()
        .into_iter()
        .filter(|c| filter.as_deref().is_none_or(|f| c.name.contains(f)))
        .collect();
    assert!(
        !cases.is_empty(),
        "RIFT_DIFFERENTIAL_FILTER={filter:?} matches no case"
    );
    let rules = load_allow_list();
    let mut used = vec![0usize; rules.len()];
    let mut failures = String::new();
    // `wide`: every difference is one of the Rift-wide (`*` case) entries; `specific`: at least one
    // needed an entry naming the case.
    let (mut clean, mut wide, mut specific, mut failing) = (0, 0, 0, 0);
    let mut dump = Vec::new();

    for case in &cases {
        let report = run_case(&client, &engines, case).await;
        dump.extend(
            report
                .differences
                .iter()
                .map(|d| (report.name.clone(), d.clone())),
        );
        let unexplained = allow::classify(&report.name, &report.differences, &rules, &mut used);
        if report.errors.is_empty() && unexplained.is_empty() {
            let case_specific = report.differences.iter().any(|d| {
                rules
                    .iter()
                    .find(|r| r.matches(&report.name, d))
                    .is_some_and(|r| r.entry.case != "*")
            });
            match (report.differences.is_empty(), case_specific) {
                (true, _) => clean += 1,
                (false, false) => wide += 1,
                (false, true) => specific += 1,
            }
            continue;
        }
        failing += 1;
        let _ = writeln!(failures, "\n✗ {}", report.name);
        for error in &report.errors {
            let _ = writeln!(failures, "    error: {error}");
        }
        for diff in &unexplained {
            let _ = writeln!(failures, "    {diff}");
        }
        for stored in &report.stored_diffs {
            let _ = writeln!(failures, "    {}", stored.replace('\n', "\n    "));
        }
    }

    if let Some(path) = std::env::var_os("RIFT_DIFFERENTIAL_DUMP") {
        let rows: Vec<serde_json::Value> = dump
            .iter()
            .map(|(case, d)| serde_json::json!({"case": case, "difference": d}))
            .collect();
        let text = serde_json::to_string_pretty(&rows).expect("differences serialise");
        std::fs::write(&path, text).unwrap_or_else(|e| {
            panic!(
                "RIFT_DIFFERENTIAL_DUMP={}: {e}",
                PathBuf::from(&path).display()
            )
        });
    }

    let stale = allow::stale(&rules, &used);
    eprintln!(
        "differential: {} cases — {clean} identical, {wide} differ only by Rift-wide allow-list \
         entries, {specific} also by case-specific entries, {failing} failing; Mountebank {}",
        cases.len(),
        mb.display()
    );
    for (rule, n) in rules.iter().zip(&used) {
        eprintln!("  allow-list {:<48} matched {n}", rule.entry.id);
    }
    assert!(
        failures.is_empty(),
        "Rift and Mountebank answered differently:{failures}"
    );
    if filter.is_none() {
        assert!(
            stale.is_empty(),
            "allow-list entries that matched nothing (fixed? remove them): {stale:?}"
        );
    }
}
