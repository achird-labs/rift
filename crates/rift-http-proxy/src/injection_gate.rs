//! The `--allowInjection` classifier: does a stub carry a Mountebank scripting surface?
//!
//! Extracted from the admin imposter handlers (issue #612) so every door that admits an imposter
//! config — `POST/PUT /imposters`, `--configfile`, `--datadir`, and `POST /admin/reload` — asks
//! one classifier the same question. The gate used to live behind the admin API only, so the same
//! document was refused by an HTTP POST and executed when loaded from a file.
//!
//! This module only *classifies*. Each door owns its own failure semantics (400, startup abort,
//! or per-file skip), which is why the response builder stays with the admin handlers.

use crate::imposter::{ImposterConfig, Predicate, PredicateOperation, Stub, StubResponse};

/// True if `config`'s stubs carry a scripting surface gated by `--allowInjection`.
pub(crate) fn config_uses_script_surface(config: &ImposterConfig) -> bool {
    stubs_contain_script_surface(&config.stubs)
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
fn response_has_script_surface(response: &StubResponse) -> bool {
    match response {
        StubResponse::Inject { .. } => true,
        StubResponse::RiftScript { rift } => rift.script.is_some(),
        StubResponse::Is {
            behaviors, rift, ..
        } => {
            let behavior_is_scripted = behaviors.as_ref().is_some_and(raw_behaviors_are_scripted);
            behavior_is_scripted || rift.as_ref().is_some_and(|r| r.script.is_some())
        }
        StubResponse::Proxy { proxy } => {
            proxy.add_decorate_behavior.is_some()
                || proxy
                    .predicate_generators
                    .iter()
                    .any(|g| g.get("inject").and_then(|v| v.as_str()).is_some())
        }
        StubResponse::Fault { .. } => false,
    }
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
        // Not an object (e.g. an array) — no key this gate recognizes, so nothing it can
        // classify as executable. Such a block does not parse into `ResponseBehaviors` either,
        // so it is inert: dropped at construction, with `new_is` logging the drop.
        return false;
    };
    let scripted_key_present = obj.contains_key("decorate") || obj.contains_key("shellTransform");
    let wait_is_scripted = obj.get("wait").is_some_and(|w| !wait_is_plainly_numeric(w));
    scripted_key_present || wait_is_scripted
}

/// True only for the two wait spellings that cannot execute code: a fixed millisecond number and
/// the `{min, max}` range. Everything else — a bare JS string, `{"inject": ...}`, or a shape this
/// gate does not recognize — is treated as executable (issue #610).
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
