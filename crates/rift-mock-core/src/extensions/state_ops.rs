//! Declarative post-response state operations (issue #969): `_rift.stateOps` on an `is`
//! response — `set` / `increment` / `delete` / `clearFlow` against the request's resolved flow id,
//! without a script.
//!
//! Reading flow state is declarative (`{{ state.<key> }}` under `_rift.templated`); until this
//! module, writing it was code — an `inject`/`_rift.script` response, with the injection gate, a
//! script engine on the request path and a body of JS/Rhai for one line of intent. These are data,
//! evaluated by the already-ungated `{{ }}` grammar, so they carry none of that.
//!
//! Semantics, all pinned by tests:
//!
//! - **When:** after the `is` response is fully rendered (templates, behaviors), just before it is
//!   written. A body reading `{{ state.hits }}` in the same response therefore sees the value
//!   *before* this request's ops — the WireMock semantics, and the one that makes "show the count,
//!   then bump it" expressible.
//! - **Order:** sequential, in the array's order; a later op sees an earlier op's write.
//! - **`increment`** is `FlowStore::increment_by`: atomic on every backend that has one, so a
//!   counter never loses a bump under concurrency. Never renders anything.
//! - **`set`** renders `value` in the `{{ }}` grammar plus one extra head, `previousValue` — the
//!   key's value before this operation, empty when it had none. A `set` whose value mentions
//!   `previousValue` is a bounded compare-and-set loop (read → render → CAS on the value read;
//!   retry on conflict, up to [`CAS_ATTEMPTS`]), so concurrent appends never lose one; a `set`
//!   that does not is a plain write. The in-memory store's default CAS is get-then-set (not
//!   atomic), which is exactly why the loop is bounded and the semantics are the *contract* an
//!   atomic backend meets rather than something only it provides.
//! - **Errors** follow the templating policy: with `debug` on, the first failing op aborts and the
//!   caller serves a 500 naming it; otherwise it is logged at `warn` and the remaining ops still
//!   run. A template token that fails inside a `set` value follows `template_fn`'s own policy
//!   (empty substitution + warn, or an error in debug).
//! - **Absent field ⇒ byte-identical behaviour**, including the prepared-response fast path.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extensions::flow_state::{CasOutcome, FlowStore};
use crate::extensions::template::RequestData;
use crate::extensions::template_fn::{TemplateContext, render_templated};

/// How many times a `previousValue`-bearing `set` retries its compare-and-set before giving up.
///
/// Sized to the worst case rather than the typical one: with `n` writers racing on one key, each
/// round can admit only one, so the unluckiest writer may need `n` rounds — 64 covers a
/// thundering herd of that size on a single key with immediate retries, which is well past what a
/// mock sees. A key hot enough to lose 64 races in a row is a load pattern, not a race, and is
/// reported (in debug, as the op's failure; otherwise a `warn`) rather than looped on forever.
pub const CAS_ATTEMPTS: usize = 64;

fn one() -> i64 {
    1
}

/// One post-response state mutation. Serialized with `op` as the tag, camelCase, so it reads as
/// `{ "op": "set", "key": "...", "value": "..." }` in a stub.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "op")]
pub enum StateOp {
    /// Set `key` to the rendered `value` template (`{{ }}` grammar plus `previousValue`).
    Set { key: String, value: String },
    /// Add `by` (default 1) to an integer key, creating it at 0. Atomic where the backend is.
    Increment {
        key: String,
        #[serde(default = "one")]
        by: i64,
    },
    /// Delete one key.
    Delete { key: String },
    /// Delete every key in the request's flow.
    ClearFlow,
}

impl StateOp {
    /// A short name for logs and error messages.
    fn describe(&self) -> String {
        match self {
            StateOp::Set { key, .. } => format!("set {key:?}"),
            StateOp::Increment { key, by } => format!("increment {key:?} by {by}"),
            StateOp::Delete { key } => format!("delete {key:?}"),
            StateOp::ClearFlow => "clearFlow".to_owned(),
        }
    }
}

/// Run `ops` in order against `flow_id` on `store`, rendering `set` values against `request`.
///
/// `debug` selects the error policy (see the module doc): `Err` is returned only in debug mode,
/// naming the op that failed; otherwise every failure is logged and `Ok(())` is returned once every
/// op has been attempted.
///
/// # Errors
///
/// In debug mode, the first op that fails — a store error, a value template that fails to render,
/// or a `previousValue` `set` that lost its compare-and-set race [`CAS_ATTEMPTS`] times.
pub fn execute_state_ops(
    ops: &[StateOp],
    store: &dyn FlowStore,
    flow_id: &str,
    request: &RequestData,
    debug: bool,
) -> Result<(), String> {
    for op in ops {
        if let Err(reason) = execute_one(op, store, flow_id, request, debug) {
            let message = format!("stateOps: {} failed: {reason}", op.describe());
            if debug {
                return Err(message);
            }
            tracing::warn!(target: "rift::state_ops", op = %op.describe(), reason = %reason,
                "state operation failed; continuing with the next");
        }
    }
    Ok(())
}

fn execute_one(
    op: &StateOp,
    store: &dyn FlowStore,
    flow_id: &str,
    request: &RequestData,
    debug: bool,
) -> Result<(), String> {
    match op {
        StateOp::Set { key, value } => {
            if value.contains("previousValue") {
                set_with_previous(store, flow_id, key, value, request, debug)
            } else {
                let rendered = render_value(value, store, flow_id, request, None, debug)?;
                store
                    .set(flow_id, key, Value::String(rendered))
                    .map_err(|e| format!("flow store: {e:#}"))
            }
        }
        StateOp::Increment { key, by } => store
            .increment_by(flow_id, key, *by)
            .map(|_| ())
            .map_err(|e| format!("flow store: {e:#}")),
        StateOp::Delete { key } => store
            .delete(flow_id, key)
            .map_err(|e| format!("flow store: {e:#}")),
        StateOp::ClearFlow => store
            .clear_flow(flow_id)
            .map_err(|e| format!("flow store: {e:#}")),
    }
}

/// The bounded compare-and-set loop a `previousValue`-bearing `set` runs: the value rendered
/// against what was read is written only if the key still holds what was read.
fn set_with_previous(
    store: &dyn FlowStore,
    flow_id: &str,
    key: &str,
    value: &str,
    request: &RequestData,
    debug: bool,
) -> Result<(), String> {
    for _ in 0..CAS_ATTEMPTS {
        let current = store
            .get(flow_id, key)
            .map_err(|e| format!("flow store: {e:#}"))?;
        let rendered = render_value(value, store, flow_id, request, current.as_ref(), debug)?;
        match store
            .compare_and_set(flow_id, key, current.as_ref(), Value::String(rendered))
            .map_err(|e| format!("flow store: {e:#}"))?
        {
            CasOutcome::Applied => return Ok(()),
            CasOutcome::Conflict(_) => continue,
        }
    }
    Err(format!(
        "lost the compare-and-set race {CAS_ATTEMPTS} times; the key is being written faster than \
         this operation can read-render-write it"
    ))
}

fn render_value(
    value: &str,
    store: &dyn FlowStore,
    flow_id: &str,
    request: &RequestData,
    previous_value: Option<&Value>,
    debug: bool,
) -> Result<String, String> {
    let ctx = TemplateContext {
        request,
        flow_id,
        flow_store: store,
        previous_value,
    };
    render_templated(value, &ctx, debug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::InMemoryFlowStore;
    use serde_json::json;

    fn request() -> RequestData {
        RequestData::new(
            "GET",
            "/orders",
            Some("id=7&item=apple"),
            &Default::default(),
            None,
        )
    }

    #[test]
    fn ops_parse_and_serialize_with_op_as_the_tag() {
        let ops: Vec<StateOp> = serde_json::from_value(json!([
            { "op": "set", "key": "lastId", "value": "{{ request.query.id }}" },
            { "op": "increment", "key": "hits" },
            { "op": "increment", "key": "score", "by": -3 },
            { "op": "delete", "key": "tmp" },
            { "op": "clearFlow" }
        ]))
        .expect("parses");
        assert_eq!(
            ops,
            vec![
                StateOp::Set {
                    key: "lastId".into(),
                    value: "{{ request.query.id }}".into()
                },
                StateOp::Increment {
                    key: "hits".into(),
                    by: 1
                },
                StateOp::Increment {
                    key: "score".into(),
                    by: -3
                },
                StateOp::Delete { key: "tmp".into() },
                StateOp::ClearFlow,
            ]
        );
        // Round-trips through the same shape, `by` included, so `GET /imposters` reads back what
        // was written.
        let back = serde_json::to_value(&ops).expect("serializes");
        assert_eq!(
            back[1],
            json!({ "op": "increment", "key": "hits", "by": 1 })
        );
        assert_eq!(back[4], json!({ "op": "clearFlow" }));
    }

    #[test]
    fn an_unknown_op_is_a_parse_error_naming_it() {
        let err = serde_json::from_value::<StateOp>(json!({ "op": "frobnicate", "key": "k" }))
            .expect_err("unknown op");
        assert!(err.to_string().contains("frobnicate"), "{err}");
    }

    #[test]
    fn set_renders_from_the_request_and_previous_value_is_empty_when_absent() {
        let store = InMemoryFlowStore::new(300);
        let ops = vec![
            StateOp::Set {
                key: "lastId".into(),
                value: "{{ request.query.id }}".into(),
            },
            StateOp::Set {
                key: "trail".into(),
                value: "{{ previousValue }}|{{ request.query.item }}".into(),
            },
        ];
        execute_state_ops(&ops, &store, "f", &request(), true).expect("ops run");
        assert_eq!(store.get("f", "lastId").unwrap(), Some(json!("7")));
        assert_eq!(store.get("f", "trail").unwrap(), Some(json!("|apple")));
        // The second time, `previousValue` is what the first write left.
        execute_state_ops(&ops, &store, "f", &request(), true).expect("ops run");
        assert_eq!(
            store.get("f", "trail").unwrap(),
            Some(json!("|apple|apple"))
        );
    }

    #[test]
    fn increment_delete_and_clear_flow_run_in_order() {
        let store = InMemoryFlowStore::new(300);
        let ops = vec![
            StateOp::Increment {
                key: "hits".into(),
                by: 1,
            },
            StateOp::Increment {
                key: "hits".into(),
                by: 10,
            },
            StateOp::Set {
                key: "tmp".into(),
                value: "x".into(),
            },
            StateOp::Delete { key: "tmp".into() },
        ];
        execute_state_ops(&ops, &store, "f", &request(), true).expect("ops run");
        assert_eq!(store.get("f", "hits").unwrap(), Some(json!(11)));
        assert_eq!(store.get("f", "tmp").unwrap(), None);
        execute_state_ops(&[StateOp::ClearFlow], &store, "f", &request(), true).expect("clear");
        assert_eq!(store.get("f", "hits").unwrap(), None);
    }

    #[test]
    fn debug_aborts_on_the_first_failure_and_non_debug_continues() {
        let store = InMemoryFlowStore::new(300);
        // A `set` whose template fails to render: in debug that is the op's failure.
        let ops = vec![
            StateOp::Set {
                key: "a".into(),
                value: "{{ request.query.missing }}".into(),
            },
            StateOp::Increment {
                key: "hits".into(),
                by: 1,
            },
        ];
        let err = execute_state_ops(&ops, &store, "f", &request(), true).expect_err("debug fails");
        assert!(err.contains("set \"a\""), "{err}");
        assert_eq!(
            store.get("f", "hits").unwrap(),
            None,
            "the later op did not run"
        );

        // Non-debug: the failing token substitutes empty, the op is not a failure at all, and the
        // increment runs.
        execute_state_ops(&ops, &store, "f", &request(), false).expect("non-debug never errors");
        assert_eq!(store.get("f", "a").unwrap(), Some(json!("")));
        assert_eq!(store.get("f", "hits").unwrap(), Some(json!(1)));
    }
}
