//! Issue #1156: `rift-lint` cannot depend on the engine, so it carries its own copy of the rule the
//! engine uses to decide whether a `decorate` script runs as JavaScript or as Rhai. That copy decides
//! whether the linter Boa-parses a decorate at all — get it wrong in one direction and valid Rhai
//! fails `E028`; in the other, broken JavaScript lints clean. A copied routing rule is exactly what
//! drifts (#1153 deleted a whole engine that had), so this pins the linter's copy against the
//! engine's own function, on every shape either side special-cases.
//!
//! It lives here because this crate dev-depends on both.

/// The engine's routing, as `apply_js_or_rhai_decorate` applies it.
fn engine_runs_as_javascript(script: &str) -> bool {
    rift_mock_core::behaviors::is_js_config_decorate(script)
        || script.trim().starts_with("function")
}

#[test]
fn the_linter_routes_decorate_scripts_exactly_as_the_engine_does() {
    let cases = [
        // JavaScript: the (request, response) function form.
        "function (request, response) { response.statusCode = 201; }",
        "function(req, res) { res.body = 'x'; }",
        "  function (r, s) {}",
        // JavaScript: the Mountebank `config` convention, every spelling.
        "config => { config.response.statusCode = 202; }",
        "config=>{}",
        "(config) => { config.response.body = 'x'; }",
        "(config)=>{}",
        "function(config) { config.response.statusCode = 200; }",
        "function (config) {}",
        // JavaScript: a bare body in the `config` convention (the engine wraps it).
        "config.response.body = JSON.stringify({ a: 1 });",
        "var x = config.request.path;",
        // Rhai.
        r#"for h in [1, 2] { response.body += h; } response.headers = #{ "X-A": "b" };"#,
        "response.status_code = 202;",
        "fn add(a, b) { a + b } response.body = add(1, 2);",
        "switch x { 1 => response.body = \"one\", _ => () }",
        // Edges.
        "",
        "   ",
        "async function (r, s) {}",
    ];
    for script in cases {
        assert_eq!(
            rift_lint::is_javascript_decorate(script),
            engine_runs_as_javascript(script),
            "the linter and the engine disagree on whether this decorate is JavaScript: {script:?}"
        );
    }
}
