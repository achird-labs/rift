//! Issue #1170: `rift-lint` syntax-checks `inject` JavaScript with its own Boa parse, and the engine
//! checks an `inject` response at the admin door with another (`validate_stubs`). Two call sites of
//! the same parser are exactly what drifts, so this pins their verdicts against each other.
//!
//! `validate_stubs` only checks inject *responses*. Predicate and predicate-generator injects run
//! through the same `var __injectFn = {inject_fn};` wrapper (`execute_predicate_inject_in` and
//! `execute_predicate_generator_inject` in `rift-mock-core/src/scripting/js_engine.rs`), so this
//! table covers them too — a change to either wrapper must revisit it.
//!
//! It lives here because this crate dev-depends on both.

use rift_http_proxy::imposter::Stub;
use rift_http_proxy::scripting::validate_stubs;
use rift_lint::{LintOptions, LintResult, validate_response};
use serde_json::{Value, json};
use std::path::Path;

fn engine_refuses(script: &str) -> bool {
    let stub: Stub = serde_json::from_value(json!({ "responses": [{ "inject": script }] }))
        .expect("an inject response with a string script deserializes");
    !validate_stubs(&[stub]).is_valid()
}

fn lint_reports_e028(script: &str) -> bool {
    let mut result = LintResult::new();
    validate_response(
        Path::new("<parity>"),
        &json!({ "inject": script }),
        "r",
        &mut result,
        &LintOptions::default(),
        &Value::Null,
    );
    result.issues.iter().any(|i| i.code == "E028")
}

// Without the engine's JavaScript there is no engine verdict to compare against.
#[cfg(feature = "javascript")]
#[test]
fn the_linter_reports_e028_exactly_when_the_engine_refuses_an_inject() {
    let scripts = [
        // accepted
        "function (config) { return { statusCode: 200 }; }",
        "function (request, state, logger, callback) { callback({ body: 'x' }); }",
        "function named(config) { return {}; }",
        "async function (config) { return {}; }",
        "function* (config) { yield 1; }",
        "(config) => ({ body: 'x' })",
        "config => { return {}; }",
        "function (config) { return {}; };",
        "function (config) { return {}; } // trailing comment",
        "function (config) {\n  return { body: `${config.request.path}` };\n}\n",
        // refused
        "function (config) { return 1 ]; }",
        "function (config) { return { statusCode: ",
        "function (config) { return {}; } }",
        "",
        "   ",
        "function (config) { var = 1; }",
        "return { statusCode: 200 };",
    ];
    let disagreements: Vec<String> = scripts
        .iter()
        .filter(|s| engine_refuses(s) != lint_reports_e028(s))
        .map(|s| {
            format!(
                "{s:?}: engine refuses {}, lint E028 {}",
                engine_refuses(s),
                lint_reports_e028(s)
            )
        })
        .collect();
    assert!(
        disagreements.is_empty(),
        "rift-lint and the engine disagree:\n  {}",
        disagreements.join("\n  ")
    );
}
