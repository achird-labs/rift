//! The `--allowInjection` and `--allowWasm` classifier: does a stub carry a scripting surface, and
//! of which kind?
//!
//! Two kinds (issue #1752). **Host code** — inject, decorate, shellTransform, a JS-function wait,
//! a predicate or `predicateGenerators` inject, a Rhai or JavaScript `_rift.script` — runs with
//! Rift's own privileges and needs `--allowInjection`. A **sandboxed** surface — a `_rift.script`
//! with `engine: "wasm"` — runs a component that can reach nothing on the host, and needs
//! `--allowWasm`, which is on by default. A document can carry both, and then needs both flags.
//!
//! Extracted from the admin imposter handlers (issue #612) so every door that admits an imposter
//! config — `POST/PUT /imposters`, `--configfile`, `--datadir`, `POST /admin/reload`, and the
//! FFI's `rift_serve_admin` `configFile` (issue #616) — asks one classifier the same question.
//! The gate used to live behind the admin API only, so the same document was refused by an HTTP
//! POST and executed when loaded from a file.
//!
//! The intercept **rule** doors ask it too (issue #657): `POST /intercept/rules`,
//! `PUT /intercept/rules` (issue #1272), the `rules` array on `POST /intercept`, and the
//! `--configfile` `intercept` block (issue #655). A rule's predicates
//! are evaluated per intercepted request, so an `inject` there is executable code arriving over the
//! same boundaries — the #612 sweep missed this door, and the same predicate was refused by
//! `POST /imposters` and executed by `POST /intercept/rules`.
//!
//! This module only *classifies*. Each door owns its own failure semantics (400, startup abort,
//! per-file skip, or FFI NULL), which is why the response builder stays with the admin handlers.
//!
//! The gate's subject is the *document*, not the caller: it asks whether config that crossed a
//! trust boundary carries executable surface. In-process config supplied by an embedding host
//! (`rift_apply_config`, `rift_create_imposter`, `rift_add_stub`, `rift_replace_stubs`,
//! `rift_intercept_add_rules`, `rift_intercept_replace_rules`, `rift_start_intercept`, and `rift_serve_admin`'s inline `config`)
//! is the trusted host path and is deliberately never gated (issue #492) — that host can already
//! execute code in the process, so gating its own JSON would restrict nobody.
//!
//! Both lists above are exhaustive on purpose, and adding a door means adding it to one of them:
//! #657 happened because a door existed in neither, so "which doors ask the gate?" had to be
//! re-derived from the code — and the answer was wrong.

use crate::imposter::{
    ImposterConfig, Predicate, PredicateOperation, RiftScriptConfig, Stub, StubResponse,
};

/// Which kinds of script a document carries (issue #1752): a set, not an order, so a door can name
/// every flag it is missing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[must_use]
pub struct ScriptSurface {
    host_code: bool,
    sandboxed: bool,
}

impl ScriptSurface {
    /// No script at all.
    pub const NONE: Self = Self {
        host_code: false,
        sandboxed: false,
    };
    const HOST_CODE: Self = Self {
        host_code: true,
        sandboxed: false,
    };
    const SANDBOXED: Self = Self {
        host_code: false,
        sandboxed: true,
    };
    const BOTH: Self = Self {
        host_code: true,
        sandboxed: true,
    };

    /// Carries code that runs with Rift's privileges: needs `--allowInjection`.
    #[must_use]
    pub fn host_code(self) -> bool {
        self.host_code
    }

    /// Carries a sandboxed wasm component: needs `--allowWasm`.
    #[must_use]
    pub fn sandboxed(self) -> bool {
        self.sandboxed
    }

    fn union(self, other: Self) -> Self {
        Self {
            host_code: self.host_code || other.host_code,
            sandboxed: self.sandboxed || other.sandboxed,
        }
    }
}

/// The two flags a door checks a document against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptGates {
    /// `--allowInjection`: host code.
    pub allow_injection: bool,
    /// `--allowWasm`: sandboxed components.
    pub allow_wasm: bool,
}

impl ScriptGates {
    /// `--allowInjection` as given and `--allowWasm` at its default (on): what every door was
    /// before the wasm flag, for tests written against that one flag.
    #[cfg(test)]
    pub(crate) fn from_injection(allow_injection: bool) -> Self {
        Self {
            allow_injection,
            allow_wasm: true,
        }
    }

    /// The flag `surface` needs and these gates lack, `None` when it is admitted. A document that
    /// lacks both is refused for host code first, as Mountebank refuses an inject.
    #[must_use]
    pub fn refusal(self, surface: ScriptSurface) -> Option<GateRefusal> {
        if surface.host_code && !self.allow_injection {
            Some(GateRefusal::Injection)
        } else if surface.sandboxed && !self.allow_wasm {
            Some(GateRefusal::Wasm)
        } else {
            None
        }
    }
}

/// Which flag a refused document needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRefusal {
    /// `--allowInjection`.
    Injection,
    /// `--allowWasm`.
    Wasm,
}

/// The message every door gives a wasm component on a server started with `--allowWasm false`.
pub const WASM_DISALLOWED: &str = "engine \"wasm\" requires --allowWasm (disabled on this server)";

/// What a `_rift.script` resolves against, so the gate classifies the engine it will actually run
/// (issue #1752): the imposter's `_rift.scripts` registry, which a `ref` takes its engine from, and
/// its `defaultEngine`, which an engine-less inline script takes. A stub door supplies the target
/// imposter's.
#[derive(Debug, Clone, Copy)]
pub struct ScriptContext<'a> {
    pub registry: &'a std::collections::HashMap<String, RiftScriptConfig>,
    pub default_engine: &'a str,
}

/// The script surfaces of `config`'s stubs, resolved against its own registry and default engine.
pub fn classify_config(config: &ImposterConfig) -> ScriptSurface {
    static EMPTY: std::sync::LazyLock<std::collections::HashMap<String, RiftScriptConfig>> =
        std::sync::LazyLock::new(Default::default);
    let registry = config.rift.as_ref().map_or(&*EMPTY, |rift| &rift.scripts);
    classify_stubs(
        &config.stubs,
        ScriptContext {
            registry,
            default_engine: config.default_script_engine(),
        },
    )
}

/// The script surfaces of `stubs`. Classification fails **closed**: a script whose engine cannot
/// be known here counts as both kinds, so each flag refuses it.
pub fn classify_stubs(stubs: &[Stub], ctx: ScriptContext<'_>) -> ScriptSurface {
    stubs.iter().fold(ScriptSurface::NONE, |acc, stub| {
        let predicates = if stub.predicates.iter().any(predicate_has_inject) {
            ScriptSurface::HOST_CODE
        } else {
            ScriptSurface::NONE
        };
        stub.responses.iter().fold(acc.union(predicates), |acc, r| {
            acc.union(response_surface(r, ctx))
        })
    })
}

/// The engine a `_rift.script` runs on, decided as the resolver decides it: a `ref` takes its
/// registry entry's engine (the referencing script's own `engine` is overwritten), an explicit
/// `engine` wins, then a file extension that names an engine, then `defaultEngine`. Both kinds when
/// the `ref` names no entry, or the entry is itself a `ref`.
fn script_surface(script: &RiftScriptConfig, ctx: ScriptContext<'_>) -> ScriptSurface {
    if let Some(name) = &script.ref_name {
        return match ctx.registry.get(name) {
            Some(entry) if entry.ref_name.is_none() => leaf_surface(entry, ctx.default_engine),
            _ => ScriptSurface::BOTH,
        };
    }
    leaf_surface(script, ctx.default_engine)
}

fn leaf_surface(script: &RiftScriptConfig, default_engine: &str) -> ScriptSurface {
    use crate::scripting::ScriptEngineKind;
    let by_extension = || {
        script
            .file
            .as_deref()
            .and_then(|f| std::path::Path::new(f).extension())
            .and_then(|e| e.to_str())
            .and_then(ScriptEngineKind::from_extension)
    };
    let wasm = match script.engine.as_deref() {
        Some(engine) => ScriptEngineKind::is_wasm(engine),
        None => match by_extension() {
            Some(Ok(kind)) => kind == ScriptEngineKind::Wasm,
            Some(Err(_)) => false,
            None => ScriptEngineKind::is_wasm(default_engine),
        },
    };
    if wasm {
        ScriptSurface::SANDBOXED
    } else {
        ScriptSurface::HOST_CODE
    }
}

/// The gated host-code surfaces, named the same way by every door (issue #612). The list only —
/// each door appends its own clause, so this must not carry one.
pub const GATED_SCRIPT_SURFACES: &str = "inject/decorate/shellTransform/JS-function wait";

/// True if `config`'s stubs carry any scripting surface: an inject response, a `decorate` behavior,
/// a `shellTransform`, a `wait` expressed as a JS function, a predicate `inject`, a
/// `predicateGenerators.inject`, or a `_rift.script` of any engine, wasm included. Kept with that
/// meaning for callers outside this crate that gate on it alone; the doors here ask
/// [`classify_config`] and [`ScriptGates::refusal`], which tell the two flags apart (issue #1752).
///
/// The classifier behind every `allowInjection` door. A door calls this to decide admission and
/// supplies its own failure semantics — this only answers the question, and answers it identically
/// for all of them. Classification fails **closed**: a `_behaviors` block that cannot be parsed is
/// treated as scripted rather than admitted as safe.
pub fn config_uses_script_surface(config: &ImposterConfig) -> bool {
    classify_config(config) != ScriptSurface::NONE
}

/// The explicit ports of every config in `configs` that `gates` refuses for `refusal`, as a door
/// would name them to a human; empty when none is. Shared so the `--configfile` and FFI
/// `configFile` doors list offenders identically — each still writes its own message, because
/// their remedies differ (`--allowInjection` vs `"allowInjection": true`).
pub fn gated_offender_ports(
    configs: &[ImposterConfig],
    gates: ScriptGates,
    refusal: GateRefusal,
) -> Vec<String> {
    configs
        .iter()
        .filter(|config| gates.refusal(classify_config(config)) == Some(refusal))
        .map(|config| match config.explicit_port() {
            Some(port) => port.to_string(),
            None => "<auto-assigned>".to_string(),
        })
        .collect()
}

/// True if `rule` carries a scripting surface gated by `--allowInjection` (issues #655, #657).
///
/// An intercept rule's only executable surface is a predicate `inject`: its `serve` action is a
/// fixed status/headers/body stub and `forward` is a port number, so neither can carry script — a
/// serve body that merely looks like JavaScript is inert data. Every door that admits a rule asks
/// this — `POST` and `PUT /intercept/rules`, the `rules` array on `POST /intercept`, and the
/// `--configfile` `intercept` block — the same question `--configfile` imposters answer via
/// [`config_uses_script_surface`], so one document cannot be refused as an imposter stub and
/// executed as an intercept predicate.
pub fn intercept_rule_uses_script_surface(rule: &crate::intercept_rules::InterceptRule) -> bool {
    rule.predicates.iter().any(predicate_has_inject)
}

/// True if any stub in `stubs` uses a Mountebank scripting surface gated by `--allowInjection`
/// (issue #355 Item 4): an inject response, a decorate behavior (`_behaviors.decorate` / a
/// proxy's `addDecorateBehavior`), a `_behaviors.shellTransform` (runs a host shell command),
/// a `wait` behavior expressed as a JS function (which this engine now executes on Boa), a
/// predicate `inject`, a `predicateGenerators.inject`, or `_rift.script`. Mirrors Mountebank's
/// `allowInjection` gate.
#[cfg(test)]
pub(crate) fn stubs_contain_script_surface(stubs: &[Stub]) -> bool {
    let registry = std::collections::HashMap::new();
    classify_stubs(
        stubs,
        ScriptContext {
            registry: &registry,
            default_engine: "rhai",
        },
    )
    .host_code()
}

/// True if `predicate` (or anything nested under a `not`/`or`/`and`) is an `inject` predicate.
pub(crate) fn predicate_has_inject(predicate: &Predicate) -> bool {
    match &predicate.operation {
        PredicateOperation::Inject(_) => true,
        PredicateOperation::Not(inner) => predicate_has_inject(inner),
        PredicateOperation::Or(preds) | PredicateOperation::And(preds) => {
            preds.iter().any(predicate_has_inject)
        }
        _ => false,
    }
}

/// The script surfaces of `response`: host code for an inject response, a decorate behavior, a
/// shellTransform behavior, a JS-function `wait` behavior, or a Rhai/JavaScript `_rift.script`;
/// sandboxed for a wasm `_rift.script`.
///
/// A behaviors block on a `fault` or `_rift`-only response is classified too, although only its
/// `repeat` takes effect there (issues #1181, #1188): the gate judges what a file asks for, not
/// what this engine happens to run.
fn response_surface(response: &StubResponse, ctx: ScriptContext<'_>) -> ScriptSurface {
    let host = |scripted: bool| {
        if scripted {
            ScriptSurface::HOST_CODE
        } else {
            ScriptSurface::NONE
        }
    };
    let script = |script: Option<&RiftScriptConfig>| {
        script.map_or(ScriptSurface::NONE, |s| script_surface(s, ctx))
    };
    match response {
        StubResponse::Inject { .. } => ScriptSurface::HOST_CODE,
        StubResponse::RiftScript {
            rift,
            ignored_behaviors,
        } => script(rift.script.as_ref())
            .union(host(behaviors_are_scripted(ignored_behaviors.as_ref()))),
        StubResponse::Is {
            behaviors, rift, ..
        } => host(behaviors_are_scripted(behaviors.as_ref()))
            .union(script(rift.as_ref().and_then(|r| r.script.as_ref()))),
        StubResponse::Proxy {
            proxy, behaviors, ..
        } => host(
            proxy.add_decorate_behavior.is_some()
                || proxy
                    .predicate_generators
                    .iter()
                    .any(|g| g.get("inject").and_then(|v| v.as_str()).is_some())
                || behaviors_are_scripted(behaviors.as_ref()),
        ),
        StubResponse::Fault {
            ignored_behaviors, ..
        } => host(behaviors_are_scripted(ignored_behaviors.as_ref())),
    }
}

fn behaviors_are_scripted(behaviors: Option<&serde_json::Value>) -> bool {
    behaviors.is_some_and(raw_behaviors_are_scripted)
}

/// True if a raw `_behaviors` block carries a scripting surface: `decorate` (JS/Rhai),
/// `shellTransform` (runs a host shell command — B1), or a `wait` that is not plainly numeric
/// (executed on Boa since issue #355 Item 6 — B2).
///
/// Read from the raw JSON rather than a parsed [`ResponseBehaviors`](crate::behaviors::ResponseBehaviors)
/// deliberately (issue #610). The gate's question is only "could this execute code?", which the
/// script-relevant keys answer on their own — so a block the *executor's* parser rejects can still
/// be classified, and the gate never has to agree with that parser to stay closed. Parsing first
/// and treating a parse failure as safe was the fail-open bug; treating it as *scripted* fixed the
/// hole but 400'd `{"repeat": 2.0}` as an injection error, which is neither true nor this gate's
/// business.
///
/// Fail-closed lives in `wait_is_plainly_numeric`: a `wait` is waved through only when it is
/// provably a delay, never merely because it failed to parse.
fn raw_behaviors_are_scripted(behaviors: &serde_json::Value) -> bool {
    // A stored block is a compiled program (issue #1198): every element is a step that runs, so
    // any scripted element makes the response scripted. A step a later `null` removed is already
    // gone, and neither runs nor needs the flag.
    if let Some(program) = behaviors.as_array() {
        // A compiled program is never empty and holds only objects, so anything else is not one
        // and fails closed (issue #1101).
        return program.is_empty()
            || program
                .iter()
                .any(|element| !element.is_object() || raw_behaviors_are_scripted(element));
    }
    let Some(obj) = behaviors.as_object() else {
        // A non-object block has no keys to classify, and "it will not parse" is not proof it is
        // inert: serde reads a JSON array into `ResponseBehaviors` by field position, so its fifth
        // element was a shellTransform (issue #1101). The stub parser now refuses these shapes;
        // the gate still fails closed rather than depend on that. `null` is the block absent.
        return !behaviors.is_null();
    };
    // An explicit `null` is the key absent (issue #1093): it parses to no behavior, so it is
    // provably inert, not merely unparsed.
    let present = |key: &str| obj.get(key).filter(|v| !v.is_null());
    let scripted_key_present = present("decorate").is_some() || present("shellTransform").is_some();
    let wait_is_scripted = present("wait").is_some_and(|w| !wait_is_plainly_numeric(w));
    scripted_key_present || wait_is_scripted
}

/// True only for the two wait spellings that cannot execute code: a fixed millisecond number and
/// the `{min, max}` range. Everything else — a bare JS string, `{"inject": ...}`, or a shape this
/// gate does not recognize — is treated as executable (issue #610). A `null` wait never reaches
/// here: the caller treats it as absent (issue #1093).
fn wait_is_plainly_numeric(wait: &serde_json::Value) -> bool {
    if wait.is_number() {
        return true;
    }
    wait.as_object().is_some_and(|o| {
        o.len() == 2
            && o.get("min").is_some_and(|v| v.is_number())
            && o.get("max").is_some_and(|v| v.is_number())
    })
}

#[cfg(test)]
mod tests {
    use super::raw_behaviors_are_scripted;
    use serde_json::json;

    fn classify(stub: serde_json::Value) -> super::ScriptSurface {
        let stub: crate::imposter::Stub = serde_json::from_value(stub).expect("stub");
        let registry = std::collections::HashMap::new();
        super::classify_stubs(
            &[stub],
            super::ScriptContext {
                registry: &registry,
                default_engine: "rhai",
            },
        )
    }

    fn script(script: serde_json::Value) -> super::ScriptSurface {
        classify(json!({"responses": [{"_rift": {"script": script}}]}))
    }

    // Issue #1752: a `_rift.script` is sandboxed only when its engine is provably wasm.
    #[test]
    fn only_a_provably_wasm_script_is_sandboxed() {
        for wasm in [
            json!({"engine": "wasm", "code": "AA=="}),
            json!({"file": "twins/mapper.wasm"}),
            json!({"engine": "wasm", "file": "twins/mapper.bin"}),
        ] {
            let surface = script(wasm.clone());
            assert!(surface.sandboxed() && !surface.host_code(), "{wasm}");
        }
        for host in [
            json!({"engine": "rhai", "code": "pass()"}),
            json!({"engine": "javascript", "code": "pass()"}),
            json!({"code": "pass()"}),
            json!({"file": "x.rhai"}),
            // A `.wasm` file the author named another engine for runs as that engine.
            json!({"engine": "rhai", "file": "x.wasm"}),
            json!({"engine": "lua", "code": "x"}),
        ] {
            let surface = script(host.clone());
            assert!(surface.host_code() && !surface.sandboxed(), "{host}");
        }
    }

    fn config_surface(config: serde_json::Value) -> super::ScriptSurface {
        let config: crate::imposter::ImposterConfig =
            serde_json::from_value(config).expect("config");
        super::classify_config(&config)
    }

    // A `ref` runs its registry entry's engine: the resolver overwrites the referencing script's
    // own `engine`, so `engine: "wasm"` beside a `ref` to JavaScript is host code. This was the
    // bypass the first cut of #1752 had.
    #[test]
    fn a_ref_is_classified_by_its_registry_entry() {
        let with_entry = |entry: serde_json::Value, script: serde_json::Value| {
            config_surface(json!({"protocol": "http",
                "_rift": {"scripts": {"x": entry}},
                "stubs": [{"responses": [{"_rift": {"script": script}}]}]}))
        };
        let js = json!({"engine": "javascript", "code": "1"});
        let surface = with_entry(js.clone(), json!({"engine": "wasm", "ref": "x"}));
        assert!(surface.host_code() && !surface.sandboxed(), "{surface:?}");
        let surface = with_entry(json!({"code": "1"}), json!({"engine": "wasm", "ref": "x"}));
        assert!(
            surface.host_code() && !surface.sandboxed(),
            "engine-less entry is rhai"
        );
        let surface = with_entry(json!({"file": "c.wasm"}), json!({"ref": "x"}));
        assert!(surface.sandboxed() && !surface.host_code(), "{surface:?}");
        let surface = with_entry(
            json!({"engine": "wasm", "code": "AA=="}),
            json!({"ref": "x"}),
        );
        assert!(surface.sandboxed() && !surface.host_code(), "{surface:?}");
        // A ref to nothing, or to another ref, is unknown: both flags refuse it.
        let surface = with_entry(js, json!({"ref": "missing"}));
        assert!(surface.host_code() && surface.sandboxed(), "{surface:?}");
        let surface = with_entry(json!({"ref": "y"}), json!({"ref": "x"}));
        assert!(surface.host_code() && surface.sandboxed(), "{surface:?}");
    }

    // An engine-less inline script runs on `defaultEngine`.
    #[test]
    fn an_engine_less_script_takes_the_default_engine() {
        let surface = config_surface(json!({"protocol": "http",
            "_rift": {"scriptEngine": {"defaultEngine": "wasm"}},
            "stubs": [{"responses": [{"_rift": {"script": {"code": "AA=="}}}]}]}));
        assert!(surface.sandboxed() && !surface.host_code(), "{surface:?}");
        // A `.rhai` file outranks the default, as the resolver's extension rule does.
        let surface = config_surface(json!({"protocol": "http",
            "_rift": {"scriptEngine": {"defaultEngine": "wasm"}},
            "stubs": [{"responses": [{"_rift": {"script": {"file": "a.rhai"}}}]}]}));
        assert!(surface.host_code() && !surface.sandboxed(), "{surface:?}");
    }

    // A scripted behaviors block beside a wasm script on the same response is host code too.
    #[test]
    fn a_wasm_script_does_not_launder_scripted_behaviors() {
        for response in [
            json!({"_behaviors": {"decorate": "function (r, s) {}"},
                   "_rift": {"script": {"engine": "wasm", "code": "AA=="}}}),
            json!({"_behaviors": {"shellTransform": "cat"},
                   "_rift": {"script": {"engine": "wasm", "code": "AA=="}}}),
        ] {
            let surface = classify(json!({"responses": [response.clone()]}));
            assert!(surface.host_code() && surface.sandboxed(), "{response}");
        }
    }

    #[test]
    fn a_document_with_both_kinds_carries_both() {
        let surface = classify(json!({
            "predicates": [{"inject": "function () { return true; }"}],
            "responses": [{"_rift": {"script": {"engine": "wasm", "code": "AA=="}}}]
        }));
        assert!(surface.host_code() && surface.sandboxed());
        let surface = classify(json!({"responses": [{"is": {"statusCode": 200}}]}));
        assert_eq!(surface, super::ScriptSurface::NONE);
    }

    #[test]
    fn each_flag_answers_for_its_own_kind() {
        use super::{GateRefusal, ScriptGates};
        let both = classify(json!({
            "predicates": [{"inject": "function () { return true; }"}],
            "responses": [{"_rift": {"script": {"engine": "wasm", "code": "AA=="}}}]
        }));
        let wasm = script(json!({"engine": "wasm", "code": "AA=="}));
        let gates = |allow_injection, allow_wasm| ScriptGates {
            allow_injection,
            allow_wasm,
        };
        assert_eq!(gates(false, true).refusal(wasm), None);
        assert_eq!(gates(true, false).refusal(wasm), Some(GateRefusal::Wasm));
        assert_eq!(
            gates(false, false).refusal(both),
            Some(GateRefusal::Injection)
        );
        assert_eq!(gates(true, false).refusal(both), Some(GateRefusal::Wasm));
        assert_eq!(
            gates(false, true).refusal(both),
            Some(GateRefusal::Injection)
        );
        assert_eq!(gates(true, true).refusal(both), None);
        let host = script(json!({"engine": "rhai", "code": "pass()"}));
        assert_eq!(
            gates(true, false).refusal(host),
            None,
            "wasm off leaves host code alone"
        );
        assert_eq!(gates(false, false).refusal(wasm), Some(GateRefusal::Wasm));
    }

    /// Issue #1093: `null` deserializes to no behavior, so it is provably inert.
    #[test]
    fn null_behavior_keys_are_not_scripted() {
        for block in [
            json!({"wait": null}),
            json!({"decorate": null}),
            json!({"shellTransform": null}),
            json!({"wait": null, "decorate": null, "shellTransform": null}),
        ] {
            assert!(!raw_behaviors_are_scripted(&block), "{block}");
        }
    }

    #[test]
    fn scripted_behaviors_are_still_scripted() {
        for block in [
            json!({"wait": "function() { return 1; }"}),
            json!({"wait": {"inject": "function() { return 1; }"}}),
            json!({"wait": true}),
            // Moved from the handler tests when the parser began refusing these (issue #1162):
            // the gate must still close on them without leaning on the parser.
            json!({"wait": {"bogus": true}}),
            json!({"wait": {"inject": 42}}),
            json!({"wait": {"min": 1}}),
            json!({"wait": {"min": "1", "max": "2"}}),
            json!({"decorate": "response.body = 'x';"}),
            json!({"decorate": 1}),
            json!({"shellTransform": "echo hi"}),
            json!({"shellTransform": []}),
            // A null sibling must not launder a live script.
            json!({"wait": null, "decorate": "response.body = 'x';"}),
            json!({"decorate": null, "shellTransform": "echo hi"}),
            json!({"shellTransform": null, "wait": "function() { return 1; }"}),
        ] {
            assert!(raw_behaviors_are_scripted(&block), "{block}");
        }
    }

    // Issue #1101: the gate must not lean on the parser to stay closed. A non-object block was
    // classified inert on the premise it could not parse, but serde reads an array positionally,
    // so its fifth element ran as a shellTransform. The parser now refuses these shapes too; this
    // is the second, independent layer. Since #1198 a stored block is an array of step objects, so
    // an array is classified element by element — and one that is not a program still fails closed.
    #[test]
    fn non_object_blocks_fail_closed() {
        for block in [
            json!([null, null, null, null, "echo pwned"]),
            json!([]),
            json!("echo pwned"),
            json!(5),
            json!(true),
        ] {
            assert!(raw_behaviors_are_scripted(&block), "{block}");
        }
    }

    // Issue #1198: a compiled program is classified step by step, so a script step anywhere in it —
    // not only in the last element of its key — needs the flag.
    #[test]
    fn a_program_is_scripted_when_any_step_is() {
        for program in [
            json!([{"wait": "function () { return 1; }"}, {"wait": 5}]),
            json!([{"wait": 1}, {"decorate": "function (req, res) {}"}]),
            json!([{"repeat": 2}, {"shellTransform": "cat"}]),
        ] {
            assert!(raw_behaviors_are_scripted(&program), "{program}");
        }
        for program in [
            json!([{"wait": 5}, {"wait": {"min": 1, "max": 2}}]),
            json!([{"repeat": 2}, {"copy": {"from": "path", "into": "${P}"}}]),
        ] {
            assert!(!raw_behaviors_are_scripted(&program), "{program}");
        }
    }

    // An explicit `null` block is the key absent (issue #1098): provably inert.
    #[test]
    fn a_null_block_is_not_scripted() {
        assert!(!raw_behaviors_are_scripted(&json!(null)));
    }

    #[test]
    fn plain_delays_are_not_scripted() {
        for block in [
            json!({"wait": 100}),
            json!({"wait": {"min": 1, "max": 5}}),
            json!({"repeat": 2}),
            // Malformed but script-free: the parser refuses these (issue #1162), and the gate must
            // not misdiagnose them as injection if one ever reaches it.
            json!({"repeat": 2.0}),
            json!({"wait": 100.0}),
        ] {
            assert!(!raw_behaviors_are_scripted(&block), "{block}");
        }
    }

    // Issue #1195: the gate reads the folded block, so a `shellTransform` in an earlier element that
    // the fold now keeps is gated, and one a later `null` clears — never run — is not.
    #[test]
    fn the_gate_sees_every_shell_transform_the_fold_keeps() {
        use super::stubs_contain_script_surface;
        use crate::imposter::Stub;
        let gated = |behaviors: serde_json::Value| {
            let stub: Stub = serde_json::from_value(
                json!({"responses": [{"is": {"statusCode": 200}, "behaviors": behaviors}]}),
            )
            .expect("stub");
            stubs_contain_script_surface(&[stub])
        };
        assert!(gated(
            json!([{"shellTransform": "a"}, {"shellTransform": "b"}])
        ));
        assert!(gated(json!([{"shellTransform": "a"}, {"wait": 5}])));
        assert!(!gated(
            json!([{"shellTransform": "a"}, {"shellTransform": null}])
        ));
    }

    // Issue #1181: a scripted block on a response no behavior runs on is still a script surface,
    // so the day those responses start running it the gate is already closed. A plain delay is not.
    #[test]
    fn a_scripted_behaviors_block_on_any_response_is_gated() {
        use super::stubs_contain_script_surface;
        use crate::imposter::Stub;
        let gated = |response: serde_json::Value| {
            let stub: Stub =
                serde_json::from_value(json!({"responses": [response]})).expect("stub");
            stubs_contain_script_surface(&[stub])
        };
        let decorate = json!({"decorate": "function (req, res) {}"});
        let shell = json!({"shellTransform": "cat"});
        for shape in [
            json!({"proxy": {"to": "http://127.0.0.1:1"}}),
            json!({"fault": "CONNECTION_RESET_BY_PEER"}),
            json!({"_rift": {}}),
        ] {
            for block in [&decorate, &shell] {
                let mut response = shape.clone();
                response["_behaviors"] = block.clone();
                assert!(gated(response.clone()), "{response}");
            }
            let mut plain = shape.clone();
            plain["_behaviors"] = json!({"wait": 500});
            assert!(!gated(plain.clone()), "{plain}");
            let mut array = shape.clone();
            array["behaviors"] = json!([decorate.clone()]);
            assert!(gated(array.clone()), "{array}");
        }
    }
}
