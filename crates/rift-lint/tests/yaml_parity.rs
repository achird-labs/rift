//! YAML backend parity corpus (issue #1315).
//!
//! `fixtures/yaml-parity/*.json` hold one golden per corpus case: the YAML text, and what
//! [`rift_lint::parse_yaml_document`] made of it (the parsed value and the repeated keys it
//! recorded, or the fact that it failed). They were generated once with the `serde_yaml` backend
//! and are the regression corpus for any later backend change. **A mismatch is a behaviour change
//! to report, never a reason to regenerate.**
//!
//! Regenerating (only when deliberately re-pinning, with the backend whose behaviour is wanted):
//! `python3 scripts/yaml-parity-corpus.py /abs/path/cases.json`, then run this target alone with
//! `YAML_PARITY_CASES=/abs/path/cases.json` and `--test-threads=1` (the test runs from the
//! package root, and the comparison tests read the goldens the generator is writing). Delete the
//! old goldens first if cases were removed.

use std::fs;
use std::path::{Path, PathBuf};

use rift_lint::parse_yaml_document;
use serde_json::{Value, json};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/yaml-parity")
}

/// What the backend made of `text`, in the shape stored in a golden's `outcome`.
fn outcome(text: &str) -> Value {
    match parse_yaml_document(text) {
        Ok(doc) => {
            let duplicates: Vec<Value> = doc
                .duplicate_keys()
                .map(|(location, key)| json!([location, key]))
                .collect();
            json!({ "ok": { "value": doc.value, "duplicates": duplicates } })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

#[test]
fn generate_goldens_when_asked() {
    let Ok(cases_path) = std::env::var("YAML_PARITY_CASES") else {
        return;
    };
    let cases: Vec<Value> =
        serde_json::from_str(&fs::read_to_string(&cases_path).expect("read cases file"))
            .expect("cases file is JSON");
    let dir = fixtures_dir();
    fs::create_dir_all(&dir).expect("create fixtures dir");
    for case in cases {
        let name = case["name"].as_str().expect("name");
        let yaml = case["yaml"].as_str().expect("yaml");
        let golden = json!({
            "name": name,
            "source": case["source"],
            "yaml": yaml,
            "outcome": outcome(yaml),
        });
        let mut body = serde_json::to_string_pretty(&golden).expect("serialize golden");
        body.push('\n');
        fs::write(dir.join(format!("{name}.json")), body).expect("write golden");
    }
}

fn goldens() -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = fs::read_dir(fixtures_dir())
        .expect("fixtures dir exists")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let golden: Value =
                serde_json::from_str(&fs::read_to_string(&p).expect("read golden")).expect("JSON");
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                golden,
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn corpus_is_present() {
    assert!(
        goldens().len() >= 90,
        "the parity corpus must not shrink silently"
    );
}

/// The parsed value, the recorded repeated keys, and whether parsing failed are identical to what
/// the goldens' backend produced. Error wording is deliberately not compared (the backends word
/// errors differently, and rift only wraps it as `Invalid YAML: {e}`).
#[test]
fn every_case_parses_exactly_as_its_golden() {
    let mut mismatches = Vec::new();
    for (file, golden) in goldens() {
        let yaml = golden["yaml"].as_str().expect("yaml");
        let got = outcome(yaml);
        let want = &golden["outcome"];
        let same = match (got.get("error"), want.get("error")) {
            (Some(_), Some(_)) => true,
            (None, None) => got == *want,
            _ => false,
        };
        if !same {
            mismatches.push(format!("{file}:\n  want {want}\n  got  {got}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} golden(s) differ:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// Behaviours rift relies on, pinned directly rather than through a golden.
#[test]
fn duplicate_keys_are_reported_not_refused() {
    let doc = parse_yaml_document("a: 1\na: 2\n").expect("a repeated key is not a parse error");
    assert_eq!(doc.duplicate_keys().count(), 1);
}

#[test]
fn multi_document_stream_is_an_error() {
    assert!(parse_yaml_document("- a: 1\n---\n- b: 2\n").is_err());
}

#[test]
fn yaml_floats_arrive_as_floats() {
    let doc = parse_yaml_document("- port: 3000.0\n").expect("parses");
    assert!(doc.value[0]["port"].is_f64());
}
