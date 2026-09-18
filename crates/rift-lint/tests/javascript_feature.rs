//! Issue #1156: released `rift-lint` artifacts were built without the `javascript` feature, so
//! `E028` fell back to brace counting and `E040` had no fallback at all — `validate_javascript`
//! returned `Ok(())` and a JS `_rift.script` got no syntax check whatsoever, with nothing said.
//!
//! The feature is now on by default. A build that opts out must say so: one `I004` per run.

use rift_lint::{LintOptions, lint_json};
use serde_json::json;

/// A document with two JavaScript sources, so "once per run" is distinguishable from "once per
/// script".
fn two_js_scripts() -> String {
    // `decorate`, not an `inject` response: the linter syntax-checks JavaScript in `decorate` and
    // function `wait`s, and in a JavaScript `_rift.script` — not in `inject` responses or predicates.
    json!({ "port": 4545, "protocol": "http", "stubs": [
        { "responses": [{ "is": { "statusCode": 200 },
          "_behaviors": { "decorate": "function (req, res) { res.statusCode = 200; }" } }] },
        { "responses": [{ "is": { "statusCode": 201 },
          "_behaviors": { "decorate": "function (req, res) { res.statusCode = 201; }" } }] }
    ] })
    .to_string()
}

fn i004s(result: &rift_lint::LintResult) -> usize {
    result.issues.iter().filter(|i| i.code == "I004").count()
}

// The shipped configuration is the one CI must hold to: the default build checks JavaScript. Without
// this, flipping the default back off would pass every test that runs under `--all-features`.
#[cfg(feature = "javascript")]
#[test]
fn a_javascript_build_reports_no_i004() {
    let result = lint_json(&two_js_scripts(), "a.json", &LintOptions::default());
    assert_eq!(
        i004s(&result),
        0,
        "a build with the feature has nothing to disclose"
    );
}

#[cfg(not(feature = "javascript"))]
#[test]
fn an_opt_out_build_says_so_once_per_document() {
    let result = lint_json(&two_js_scripts(), "a.json", &LintOptions::default());
    assert_eq!(
        i004s(&result),
        1,
        "one I004 per run, not per script, got {:?}",
        result.issues
    );
}

#[cfg(not(feature = "javascript"))]
#[test]
fn an_opt_out_build_says_so_once_per_directory_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.json"), two_js_scripts()).expect("write");
    std::fs::write(dir.path().join("b.json"), two_js_scripts()).expect("write");
    let result = rift_lint::lint_directory(dir.path(), &LintOptions::default());
    assert_eq!(
        i004s(&result),
        1,
        "two files, four scripts, still one I004 — it describes the build, not a document; got {:?}",
        result.issues
    );
}

// A document with no JavaScript has nothing to disclose, feature or not.
#[test]
fn no_i004_without_javascript_in_the_document() {
    let doc = json!({ "port": 4545, "protocol": "http",
        "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }] })
    .to_string();
    assert_eq!(
        i004s(&lint_json(&doc, "a.json", &LintOptions::default())),
        0
    );
}

// ─── A Rhai `decorate` is not JavaScript (issue #1156 review) ───────────────────────────────
//
// `decorate` runs as JavaScript only when the engine routes it there — a `function…` or the
// Mountebank `config` convention; anything else runs as Rhai. The linter used to Boa-parse every
// decorate, which never shipped only because releases lacked the `javascript` feature. Turning the
// feature on by default made valid Rhai decorates fail with E028. These pin that it does not.

/// Real Rhai — a `for … in`, a `#{}` object map — that Boa cannot parse.
const RHAI_DECORATE: &str =
    r#"for h in [1, 2] { response.body += h; } response.headers = #{ "X-A": "b" };"#;

fn decorate_doc(script: &str) -> String {
    json!({ "port": 4545, "protocol": "http", "stubs": [
        { "responses": [{ "is": { "statusCode": 200 },
          "_behaviors": { "decorate": script } }] }
    ] })
    .to_string()
}

#[test]
fn a_rhai_decorate_is_not_parsed_as_javascript() {
    let result = lint_json(
        &decorate_doc(RHAI_DECORATE),
        "a.json",
        &LintOptions::default(),
    );
    assert!(
        !result.issues.iter().any(|i| i.code == "E028"),
        "valid Rhai must not be reported as a JavaScript syntax error: {:?}",
        result.issues
    );
}

// Nor does an opt-out build claim it skipped JavaScript it never had.
#[test]
fn a_rhai_decorate_triggers_no_i004() {
    let result = lint_json(
        &decorate_doc(RHAI_DECORATE),
        "a.json",
        &LintOptions::default(),
    );
    assert_eq!(i004s(&result), 0, "no JavaScript was present to skip");
}

// A decorate the engine DOES run as JavaScript is still checked, so the fix is routing, not a
// blanket stand-down.
#[cfg(feature = "javascript")]
#[test]
fn a_javascript_decorate_is_still_syntax_checked() {
    let result = lint_json(
        &decorate_doc("function (req, res) { res.statusCode = ; }"),
        "a.json",
        &LintOptions::default(),
    );
    assert!(
        result.issues.iter().any(|i| i.code == "E028"),
        "a broken JavaScript decorate must still be E028: {:?}",
        result.issues
    );
}
