//! Issue #608: the shipped `examples/*.json` must actually work on the engine that ships them.
//!
//! `_behaviors` is stored as raw JSON and parsed once into a cache at stub construction. That
//! parse used to be swallowed, so an example using a documented-but-unsupported shape started
//! cleanly and served requests with its behaviors silently gone — no error, no log, no latency.
//! Asserting the file *loads* is therefore not enough: these tests assert the behaviors survive
//! construction, which is what "the example works" actually means.

use rift_mock_core::imposter::{ImposterConfig, StubResponse};
use std::path::PathBuf;

fn examples_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/rift-mock-core
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .canonicalize()
        .expect("examples dir")
}

fn configs_in(file: &str) -> Vec<ImposterConfig> {
    let raw = std::fs::read_to_string(examples_dir().join(file))
        .unwrap_or_else(|e| panic!("read {file}: {e}"));
    let v: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{file} is not valid JSON: {e}"));
    let imposters = match v.get("imposters") {
        Some(serde_json::Value::Array(a)) => a.clone(),
        _ => vec![v],
    };
    imposters
        .into_iter()
        .map(|i| {
            serde_json::from_value(i.clone())
                .unwrap_or_else(|e| panic!("{file}: imposter does not deserialize: {e}"))
        })
        .collect()
}

/// Every `_behaviors` block present in raw JSON must survive into the parsed cache. A `None` cache
/// beside a `Some` raw block is the silent drop.
fn assert_behaviors_survive(file: &str) {
    let mut checked = 0;
    for config in configs_in(file) {
        for (s, stub) in config.stubs.iter().enumerate() {
            for (r, response) in stub.responses.iter().enumerate() {
                if let StubResponse::Is {
                    behaviors: Some(raw),
                    behaviors_parsed,
                    ..
                } = response
                {
                    assert!(
                        behaviors_parsed.is_some(),
                        "{file} stub[{s}].responses[{r}]: _behaviors present in JSON but dropped \
                         at construction — the engine cannot honour its own shipped example. raw: {raw}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 0, "{file} exercised no _behaviors block");
}

// AC 608-3: the example that motivated the issue — its `/random-latency` stub uses the documented
// object-form wait, which was silently dropped before the `Inject` variant existed.
#[test]
fn latency_testing_example_behaviors_survive() {
    assert_behaviors_survive("latency-testing.json");
}

// The same guarantee for every other shipped example that carries behaviors, so a future
// documented-but-unsupported shape cannot slip in through a different file.
#[test]
fn all_shipped_examples_deserialize() {
    let dir = examples_dir();
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("read examples dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf8 filename");
        // Panics on any imposter that fails to deserialize.
        let _ = configs_in(name);
        seen += 1;
    }
    assert!(seen >= 5, "expected the shipped examples, found {seen}");
}
