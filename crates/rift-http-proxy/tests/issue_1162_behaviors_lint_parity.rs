//! Issue #1162: the engine refuses a behaviors block it cannot parse, and `rift-lint` exists so a
//! file is not first found broken at load. `rift-lint` cannot depend on the engine, so it carries its
//! own description of `ResponseBehaviors` — and a copied description is exactly what drifts. This
//! pins the two verdicts against each other on every shape either side special-cases.
//!
//! It lives here because this crate dev-depends on both.

use rift_http_proxy::imposter::StubResponse;
use rift_lint::{LintOptions, LintResult, Severity, validate_response};
use serde_json::{Value, json};
use std::path::Path;

fn engine_refuses(response: &Value) -> bool {
    serde_json::from_value::<StubResponse>(response.clone()).is_err()
}

fn lint_errors(response: &Value) -> Vec<String> {
    let mut result = LintResult::new();
    validate_response(
        Path::new("<parity>"),
        response,
        "r",
        &mut result,
        &LintOptions::default(),
        &Value::Null,
    );
    result
        .issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .map(|i| format!("{} {}", i.code, i.message))
        .collect()
}

fn using() -> Value {
    json!({ "method": "regex", "selector": ".*" })
}

fn csv() -> Value {
    json!({ "csv": { "path": "x.csv", "keyColumn": "id" } })
}

fn lookup_with(key: Value, source: Value, into: Value) -> Value {
    json!({ "lookup": { "key": key, "fromDataSource": source, "into": into } })
}

/// Blocks whose verdict the two must agree on. `repeat: 0` (lint-only, stricter by design) and
/// JavaScript syntax (E028, checked by a different mechanism) are deliberately absent.
fn blocks() -> Vec<Value> {
    vec![
        // wait
        json!({ "wait": 100 }),
        json!({ "wait": { "min": 1, "max": 9 } }),
        json!({ "wait": "function() { return 5; }" }),
        json!({ "wait": { "inject": "function() { return 5; }" } }),
        json!({ "wait": { "min": "1", "max": "2" } }),
        json!({ "wait": 500.5 }),
        json!({ "wait": -1 }),
        json!({ "wait": { "min": 1 } }),
        json!({ "wait": true }),
        // repeat
        json!({ "repeat": 3 }),
        json!({ "repeat": 4_294_967_295_u64 }),
        json!({ "repeat": 4_294_967_296_u64 }),
        json!({ "repeat": 2.0 }),
        json!({ "repeat": "3" }),
        // decorate / shellTransform
        json!({ "decorate": "function (r, s) {}" }),
        json!({ "decorate": 5 }),
        json!({ "shellTransform": "echo a" }),
        json!({ "shellTransform": ["echo a", "echo b"] }),
        json!({ "shellTransform": ["echo a", 1] }),
        json!({ "shellTransform": 5 }),
        // copy
        json!({ "copy": { "from": "path", "into": "${P}", "using": using() } }),
        json!({ "copy": [{ "from": { "query": "q" }, "into": "${P}", "using": using() }] }),
        json!({ "copy": { "from": "path", "into": "${P}",
                          "using": { "method": "jsonpath", "selector": "$.a" } } }),
        json!({ "copy": { "from": "path", "into": "${P}",
                          "using": { "method": "regex", "selector": ".", "options": { "ignoreCase": true } } } }),
        json!({ "copy": { "from": "path", "into": "${P}" } }),
        json!({ "copy": { "from": "path", "into": "${P}", "using": null } }),
        json!({ "copy": { "from": "path", "into": "${P}", "using": { "method": "regex" } } }),
        json!({ "copy": { "from": "path", "into": "${P}", "using": { "method": "sed", "selector": "x" } } }),
        json!({ "copy": { "from": "path", "into": "${P}",
                          "using": { "method": "regex", "selector": ".", "options": { "ignoreCase": "yes" } } } }),
        json!({ "copy": { "from": 5, "into": "${P}", "using": using() } }),
        json!({ "copy": { "from": { "query": 5 }, "into": "${P}", "using": using() } }),
        json!({ "copy": { "from": "path", "into": 5, "using": using() } }),
        json!({ "copy": { "into": "${P}", "using": using() } }),
        json!({ "copy": [{ "from": "path", "into": "${P}", "using": using() }, "oops"] }),
        json!({ "copy": [null] }),
        json!({ "copy": "path" }),
        // lookup
        lookup_with(
            json!({ "from": "path", "using": using() }),
            csv(),
            json!("${R}"),
        ),
        json!({ "lookup": [{ "key": { "from": "path", "using": using() },
                             "fromDataSource": csv(), "into": "${R}" }] }),
        lookup_with(json!({ "from": "path" }), csv(), json!("${R}")),
        lookup_with(json!({ "using": using() }), csv(), json!("${R}")),
        lookup_with(json!("path"), csv(), json!("${R}")),
        lookup_with(
            json!({ "from": "path", "using": using() }),
            json!({}),
            json!("${R}"),
        ),
        lookup_with(
            json!({ "from": "path", "using": using() }),
            json!({ "csv": { "path": "x.csv" } }),
            json!("${R}"),
        ),
        lookup_with(
            json!({ "from": "path", "using": using() }),
            json!({ "csv": { "path": "x.csv", "keyColumn": "id", "delimiter": ";;" } }),
            json!("${R}"),
        ),
        lookup_with(
            json!({ "from": "path", "using": using() }),
            json!({ "csv": { "path": "x.csv", "keyColumn": "id", "delimiter": ";" } }),
            json!("${R}"),
        ),
        lookup_with(json!({ "from": "path", "using": using() }), csv(), json!(5)),
        json!({ "lookup": { "fromDataSource": csv(), "into": "${R}" } }),
        json!({ "lookup": [5] }),
        json!({ "lookup": "x" }),
        // null keys are absent to both
        json!({ "wait": null, "repeat": null, "copy": null, "lookup": null,
                "decorate": null, "shellTransform": null }),
    ]
}

/// `behaviors` arrays whose verdict depends on the fold (issue #1195): `copy`, `lookup` and
/// `shellTransform` accumulate across elements unless a later `null` clears them; every other key
/// is last-wins.
fn arrays() -> Vec<Value> {
    let copy = json!({ "from": "path", "into": "${P}", "using": using() });
    let no_from = json!({ "into": "${P}", "using": using() });
    let lookup = json!({ "key": { "from": "path", "using": using() },
                         "fromDataSource": csv(), "into": "${R}" });
    vec![
        json!([{ "copy": copy }, { "copy": copy }]),
        json!([{ "copy": no_from }, { "copy": copy }]),
        json!([{ "copy": copy }, { "copy": no_from }]),
        json!([{ "copy": no_from }, { "copy": [] }]),
        json!([{ "copy": no_from }, { "copy": null }]),
        json!([{ "copy": no_from }, { "copy": null }, { "copy": copy }]),
        json!([{ "copy": copy }, { "copy": null }, { "copy": no_from }, { "copy": null }, { "copy": copy }]),
        json!([{ "copy": no_from }, { "copy": null }, { "copy": copy }, { "copy": null }, { "copy": no_from }]),
        json!([{ "copy": "path" }, { "copy": copy }]),
        json!([{ "copy": [copy, no_from] }, { "copy": copy }]),
        json!([{ "lookup": [5] }, { "lookup": lookup }]),
        json!([{ "lookup": lookup }, { "lookup": lookup }]),
        json!([{ "shellTransform": 5 }, { "shellTransform": "echo a" }]),
        json!([{ "shellTransform": ["echo a", 1] }, { "shellTransform": null }]),
        json!([{ "shellTransform": "echo a" }, { "shellTransform": ["echo b"] }]),
        // Scalars stay last-wins.
        json!([{ "wait": true }, { "wait": 5 }]),
        json!([{ "wait": 5 }, { "wait": true }]),
        json!([{ "decorate": 5 }, { "decorate": "function (r, s) {}" }]),
    ]
}

#[test]
fn the_linter_errors_on_exactly_the_behaviors_arrays_the_engine_refuses() {
    let mut disagreements = Vec::new();
    for array in arrays() {
        let response = json!({ "is": { "statusCode": 200 }, "behaviors": array });
        let engine = engine_refuses(&response);
        let lint = lint_errors(&response);
        if engine != !lint.is_empty() {
            disagreements.push(format!(
                "{response}\n    engine refuses: {engine}, lint errors: {lint:?}"
            ));
        }
    }
    assert!(
        disagreements.is_empty(),
        "rift-lint and the engine disagree:\n  {}",
        disagreements.join("\n  ")
    );
}

#[test]
fn the_linter_errors_on_exactly_the_behaviors_blocks_the_engine_refuses() {
    let mut disagreements = Vec::new();
    for block in blocks() {
        // Every response type carries the block through the same check, `proxy` included.
        for response in [
            json!({ "is": { "statusCode": 200 }, "_behaviors": block }),
            json!({ "proxy": { "to": "http://localhost:1" }, "_behaviors": block }),
        ] {
            let engine = engine_refuses(&response);
            let lint = lint_errors(&response);
            if engine != !lint.is_empty() {
                disagreements.push(format!(
                    "{response}\n    engine refuses: {engine}, lint errors: {lint:?}"
                ));
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "rift-lint and the engine disagree:\n  {}",
        disagreements.join("\n  ")
    );
}
