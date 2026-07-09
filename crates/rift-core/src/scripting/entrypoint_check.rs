//! Static entrypoint/arity checking for `rift script check` (issue #360 Item 1).
//!
//! Complements the per-response syntax-only checks in `stub_validator.rs`/`rift-lint`: this
//! module answers "does the script actually define the entrypoint the given hook will call at
//! request time?" — issue #357 already added detection for exactly this ("script defines
//! function(s) but none is the `respond` entrypoint...") inside each engine's runtime dispatch
//! (`run_entrypoint` in `rhai_engine.rs`, and its JS equivalent in `js_engine.rs`), but only as
//! a request-time error. This module exposes the same
//! function-name-based detection statically, so `rift script check` can catch a misnamed
//! entrypoint (e.g. `fn respnod(ctx)`) before any request ever exercises it.
//!
//! "Static" here means never calling the entrypoint function itself. Rhai needs no evaluation at
//! all — `rhai::AST::iter_functions()` lists declared functions straight from the parsed AST.
//! JS has no cheap public AST walk, so it compiles the script (syntax check, WITHOUT running),
//! then evaluates its top level ONCE to discover which functions it declared, by diffing the
//! global object before and after — the same technique the runtime path uses to spot a
//! misnamed entrypoint. Crucially that top-level eval binds the SAME host globals the real
//! execution path binds (`ctx`/`http`/`pass`/`delay`/`reset`/`request`/`flow_store`) under the
//! engine's runtime limits (a JS loop-iteration cap) — so a legitimate #357 bare-expression
//! response script (`http(503, "boom")`) is neither mis-flagged as an unbound-global error nor
//! able to hang the check with a top-level `while(true){}`. That binding-and-budgeting lives in
//! the engine itself (`js_engine::declared_functions_js`), reusing its real ctx/result-constructor
//! setup.

use super::entrypoints;

/// Which entrypoint contract a script satisfies for the checked hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntrypointMatch {
    /// The legacy v1 `should_inject(request, flow_store)` wrapper (only possible when the
    /// checked hook is `respond`).
    V1ShouldInject,
    /// A v2 function declared with exactly the hook's name (e.g. `respond`, `matches`).
    Named,
    /// No functions declared at all — a legal v2 bare-expression script.
    Bare,
}

/// Why a script failed the entrypoint/arity check.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntrypointCheckError {
    #[error("Unknown script engine type: '{0}'")]
    UnknownEngine(String),
    #[error("{engine} engine is not enabled (requires the '{feature}' feature)")]
    EngineDisabled { engine: String, feature: String },
    #[error(
        "the {0} scripting engine was removed (issue #450); use engine \"rhai\" or \"javascript\""
    )]
    EngineRemoved(String),
    #[error("Syntax error: {0}")]
    Syntax(String),
    #[error(
        "script defines function(s) ({declared}) but none is the `{hook}` entrypoint (and \
         there is no bare expression to evaluate); did you mean `{hook}`?"
    )]
    Mismatch { hook: String, declared: String },
}

/// Statically check whether `script` (for `engine_type`) defines a valid entrypoint for `hook`
/// (`"respond"`, `"matches"`, `"transform"`, or `"delay"`). `Err` covers both a compile/syntax
/// error and an entrypoint mismatch — see [`EntrypointCheckError`]'s variants.
pub fn check_entrypoint(
    engine_type: &str,
    script: &str,
    hook: &str,
) -> Result<EntrypointMatch, EntrypointCheckError> {
    let declared = declared_functions(engine_type, script)?;
    decide(&declared, hook)
}

/// Apply the same has-should-inject/has-named/has-any-function decision `run_entrypoint` (issue
/// #357) makes at request time, given the set of top-level function names a script declares.
fn decide(declared: &[String], hook: &str) -> Result<EntrypointMatch, EntrypointCheckError> {
    if hook == entrypoints::RESPOND && declared.iter().any(|n| n == entrypoints::SHOULD_INJECT) {
        return Ok(EntrypointMatch::V1ShouldInject);
    }
    if declared.iter().any(|n| n == hook) {
        return Ok(EntrypointMatch::Named);
    }
    if declared.is_empty() {
        return Ok(EntrypointMatch::Bare);
    }
    Err(EntrypointCheckError::Mismatch {
        hook: hook.to_string(),
        declared: declared.join(", "),
    })
}

/// The top-level function names `script` declares, for `engine_type`. Empty means the script has
/// no function declarations at all (a bare expression).
fn declared_functions(
    engine_type: &str,
    script: &str,
) -> Result<Vec<String>, EntrypointCheckError> {
    match engine_type {
        "rhai" => rhai_declared_functions(script),
        #[cfg(feature = "javascript")]
        "javascript" | "js" => super::js_engine::declared_functions_js(script)
            .map_err(|e| EntrypointCheckError::Syntax(e.to_string())),
        #[cfg(not(feature = "javascript"))]
        "javascript" | "js" => Err(EntrypointCheckError::EngineDisabled {
            engine: "javascript".to_string(),
            feature: "javascript".to_string(),
        }),
        "lua" => Err(EntrypointCheckError::EngineRemoved("Lua".to_string())),
        other => Err(EntrypointCheckError::UnknownEngine(other.to_string())),
    }
}

/// Pure AST introspection — `compile` only parses, it never runs top-level statements, so this
/// has zero execution side effects (unlike the JS path, which has no public AST walk).
fn rhai_declared_functions(script: &str) -> Result<Vec<String>, EntrypointCheckError> {
    let engine = rhai::Engine::new();
    let ast = engine
        .compile(script)
        .map_err(|e| EntrypointCheckError::Syntax(e.to_string()))?;
    Ok(ast.iter_functions().map(|f| f.name.to_string()).collect())
}

/// True when `code` declares the deprecated v1 `should_inject` wrapper (`fn should_inject` in
/// Rhai, `function should_inject` in JS) — the same signal rift-lint's E041 lint keys on
/// (`crate::validator::check_script_v1_deprecation` there), exposed here so `rift script check`
/// can flag a raw script file too (no imposter config for rift-lint's own config-shaped pass to
/// run over).
pub fn is_v1_should_inject(code: &str) -> bool {
    // The pattern is a fixed literal (never fails to compile in practice), but `OnceLock` can
    // only cache a value, not un-panic a `.expect()` — cache the fallible `Option` instead so a
    // hypothetical compile failure degrades to "not detected" rather than a panic.
    static SHOULD_INJECT_RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    SHOULD_INJECT_RE
        .get_or_init(|| regex::Regex::new(r"\b(?:fn|function)\s+should_inject\b").ok())
        .as_ref()
        .is_some_and(|re| re.is_match(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rhai_respond_named_matches() {
        let script = "fn respond(ctx) { pass() }";
        assert_eq!(
            check_entrypoint("rhai", script, "respond"),
            Ok(EntrypointMatch::Named)
        );
    }

    #[test]
    fn rhai_should_inject_is_v1() {
        let script = "fn should_inject(request, flow_store) { #{ inject: false } }";
        assert_eq!(
            check_entrypoint("rhai", script, "respond"),
            Ok(EntrypointMatch::V1ShouldInject)
        );
    }

    #[test]
    fn rhai_bare_expression_is_ok() {
        assert_eq!(
            check_entrypoint("rhai", "pass()", "respond"),
            Ok(EntrypointMatch::Bare)
        );
    }

    // AC (issue #360): a syntax-valid script whose only function is misnamed fails naming the
    // expected entrypoint.
    #[test]
    fn rhai_misnamed_entrypoint_fails_naming_respond() {
        let script = "fn respnod(ctx) { pass() }";
        let err = check_entrypoint("rhai", script, "respond").unwrap_err();
        match err {
            EntrypointCheckError::Mismatch { hook, declared } => {
                assert_eq!(hook, "respond");
                assert_eq!(declared, "respnod");
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn rhai_syntax_error_is_syntax_variant() {
        let script = "fn respond(ctx { pass() }";
        let err = check_entrypoint("rhai", script, "respond").unwrap_err();
        assert!(matches!(err, EntrypointCheckError::Syntax(_)));
    }

    #[test]
    fn rhai_matches_hook_checks_matches_name() {
        assert_eq!(
            check_entrypoint("rhai", "fn matches(ctx) { true }", "matches"),
            Ok(EntrypointMatch::Named)
        );
        let err = check_entrypoint("rhai", "fn respond(ctx) { pass() }", "matches").unwrap_err();
        assert!(matches!(err, EntrypointCheckError::Mismatch { .. }));
    }

    #[test]
    fn unknown_engine_errors() {
        let err = check_entrypoint("cobol", "whatever", "respond").unwrap_err();
        assert!(matches!(err, EntrypointCheckError::UnknownEngine(e) if e == "cobol"));
    }

    #[test]
    fn v1_deprecation_detects_declaration_not_substring() {
        assert!(is_v1_should_inject(
            "fn should_inject(request, flow_store) { #{ inject: false } }"
        ));
        assert!(!is_v1_should_inject("fn respond(ctx) { pass() }"));
        // A comment mentioning should_inject without declaring it must not false-positive.
        assert!(!is_v1_should_inject(
            "// migrated off should_inject\nfn respond(ctx) { pass() }"
        ));
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn js_respond_named_matches() {
        let script = "function respond(ctx) { return pass(); }";
        assert_eq!(
            check_entrypoint("javascript", script, "respond"),
            Ok(EntrypointMatch::Named)
        );
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn js_should_inject_is_v1() {
        let script = "function should_inject(request, flow_store) { return {inject: false}; }";
        assert_eq!(
            check_entrypoint("javascript", script, "respond"),
            Ok(EntrypointMatch::V1ShouldInject)
        );
    }

    // AC (issue #360), JS variant: misnamed entrypoint fails naming `respond`.
    #[cfg(feature = "javascript")]
    #[test]
    fn js_misnamed_entrypoint_fails_naming_respond() {
        let script = "function respnod(ctx) { return pass(); }";
        let err = check_entrypoint("javascript", script, "respond").unwrap_err();
        match err {
            EntrypointCheckError::Mismatch { hook, declared } => {
                assert_eq!(hook, "respond");
                assert_eq!(declared, "respnod");
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn js_syntax_error_is_syntax_variant() {
        let err = check_entrypoint("javascript", "function respond(ctx {", "respond").unwrap_err();
        assert!(matches!(err, EntrypointCheckError::Syntax(_)));
    }

    // B1 regression (issue #360): a legitimate #357 bare-expression JS response script must NOT
    // false-fail — the top-level `http(...)`/`ctx.*` references resolve against the bound host
    // globals, exactly as at request time.
    #[cfg(feature = "javascript")]
    #[test]
    fn js_bare_expression_calling_http_is_ok() {
        assert_eq!(
            check_entrypoint("javascript", r#"http(503, "boom")"#, "respond"),
            Ok(EntrypointMatch::Bare)
        );
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn js_bare_expression_reading_ctx_request_is_ok() {
        let script = r#"(ctx.request.method === "POST") ? http(503, {error:"x"}) : pass()"#;
        assert_eq!(
            check_entrypoint("javascript", script, "respond"),
            Ok(EntrypointMatch::Bare)
        );
    }

    // B1 regression: a top-level `while(true){}` must TERMINATE the check (via the loop-iteration
    // cap), not hang. It surfaces as OK/Bare here (no functions declared) — the point is it
    // returns at all.
    #[cfg(feature = "javascript")]
    #[test]
    fn js_top_level_infinite_loop_terminates() {
        let script = "while (true) {}";
        // Returns (bounded) rather than hanging; the exact classification is immaterial.
        let _ = check_entrypoint("javascript", script, "respond");
    }
}
