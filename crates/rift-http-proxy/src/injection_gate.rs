//! The `--allowInjection` classifier: does a stub carry a Mountebank scripting surface?
//!
//! Extracted from the admin imposter handlers (issue #612) so every door that admits an imposter
//! config — `POST/PUT /imposters`, `--configfile`, `--datadir`, `POST /admin/reload`, and the
//! FFI's `rift_serve_admin` `configFile` (issue #616) — asks one classifier the same question.
//! The gate used to live behind the admin API only, so the same document was refused by an HTTP
//! POST and executed when loaded from a file.
//!
//! The intercept **rule** doors ask it too (issue #657): `POST /intercept/rules`, the `rules` array
//! on `POST /intercept`, and the `--configfile` `intercept` block (issue #655). A rule's predicates
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
//! `rift_intercept_add_rules`, `rift_start_intercept`, and `rift_serve_admin`'s inline `config`)
//! is the trusted host path and is deliberately never gated (issue #492) — that host can already
//! execute code in the process, so gating its own JSON would restrict nobody.
//!
//! Both lists above are exhaustive on purpose, and adding a door means adding it to one of them:
//! #657 happened because a door existed in neither, so "which doors ask the gate?" had to be
//! re-derived from the code — and the answer was wrong.

use crate::imposter::{ImposterConfig, Predicate, PredicateOperation, Stub, StubResponse};

/// The gated surfaces, named the same way by every door (issue #612). The list only — each door
/// appends its own clause, so this must not carry one.
pub const GATED_SCRIPT_SURFACES: &str = "inject/decorate/shellTransform/JS-function wait";

/// True if `config`'s stubs carry a scripting surface gated by `--allowInjection`: an inject
/// response, a `decorate` behavior, a `shellTransform`, a `wait` expressed as a JS function, a
/// predicate `inject`, a `predicateGenerators.inject`, or `_rift.script`.
///
/// The classifier behind every `allowInjection` door. A door calls this to decide admission and
/// supplies its own failure semantics — this only answers the question, and answers it identically
/// for all of them. Classification fails **closed**: a `_behaviors` block that cannot be parsed is
/// treated as scripted rather than admitted as safe.
pub fn config_uses_script_surface(config: &ImposterConfig) -> bool {
    stubs_contain_script_surface(&config.stubs)
}

/// The explicit ports of every config in `configs` that trips [`config_uses_script_surface`], as a
/// door would name them to a human; empty when all are admissible. Shared so the `--configfile` and
/// FFI `configFile` doors list offenders identically — each still writes its own message, because
/// their remedies differ (`--allowInjection` vs `"allowInjection": true`).
pub fn gated_offender_ports(configs: &[ImposterConfig]) -> Vec<String> {
    configs
        .iter()
        .filter(|config| config_uses_script_surface(config))
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
/// this — `POST /intercept/rules`, the `rules` array on `POST /intercept`, and the `--configfile`
/// `intercept` block — the same question `--configfile` imposters answer via
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
pub(crate) fn stubs_contain_script_surface(stubs: &[Stub]) -> bool {
    stubs.iter().any(|stub| {
        stub.predicates.iter().any(predicate_has_inject)
            || stub.responses.iter().any(response_has_script_surface)
    })
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

/// True if `response` uses any script surface: an inject response, a decorate behavior, a
/// shellTransform behavior, a JS-function `wait` behavior, or `_rift.script`.
///
/// A behaviors block on a `proxy`, `fault` or `_rift`-only response is classified too, although
/// only its `repeat` takes effect there (issues #1181, #1188): Mountebank runs the rest on `proxy`,
/// and the gate must already be closed on the day Rift does.
fn response_has_script_surface(response: &StubResponse) -> bool {
    match response {
        StubResponse::Inject { .. } => true,
        StubResponse::RiftScript {
            rift,
            ignored_behaviors,
        } => rift.script.is_some() || behaviors_are_scripted(ignored_behaviors.as_ref()),
        StubResponse::Is {
            behaviors, rift, ..
        } => {
            behaviors_are_scripted(behaviors.as_ref())
                || rift.as_ref().is_some_and(|r| r.script.is_some())
        }
        StubResponse::Proxy {
            proxy,
            ignored_behaviors,
            ..
        } => {
            proxy.add_decorate_behavior.is_some()
                || proxy
                    .predicate_generators
                    .iter()
                    .any(|g| g.get("inject").and_then(|v| v.as_str()).is_some())
                || behaviors_are_scripted(ignored_behaviors.as_ref())
        }
        StubResponse::Fault {
            ignored_behaviors, ..
        } => behaviors_are_scripted(ignored_behaviors.as_ref()),
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
    // is the second, independent layer.
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
