//! Scenario FSM + flow-state admin handlers (issue #190).
//!
//! Scenario state and arbitrary flow-state both live in the imposter's `FlowStore`,
//! partitioned by `flow_id`. When a `flowId` is not supplied, the imposter's default
//! flow (`resolve_flow_id` with no headers ⇒ the `imposter_port` flow) is used.

use crate::admin_api::handlers::imposters::{
    admin_script_base, imposter_script_registry, reject_stubs_if_injection_disallowed,
};
use crate::admin_api::types::{collect_body, error_response, json_response};
use crate::extensions::decorate::backend_error_response;
use crate::imposter::{Imposter, ImposterManager, Stub, resolve_stub_scripts};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn default_flow_id(imposter: &Imposter) -> String {
    imposter.resolve_flow_id(&HashMap::new())
}

/// Collect and JSON-parse a request body, returning a `400` response on failure.
// The error channel here *is* the rendered `400`, which is what lets a handler return it with
// `?`. `Response` is hyper's type and its size is not ours to shrink; boxing it would only move
// the unboxing to every call site, since each one hands the response straight back.
#[allow(clippy::result_large_err)]
async fn parse_json_body(
    req: Request<Incoming>,
) -> Result<serde_json::Value, Response<Full<Bytes>>> {
    let body = collect_body(req)
        .await
        .map_err(|e| error_response(e.status_code(), &e.to_string()))?;
    serde_json::from_slice(&body)
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {e}")))
}

/// Extract `flowId=` from a query string.
fn flow_id_from_query(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "flowId").then(|| {
            // Domain-optional decode: an undecodable value passes through raw, the repo's
            // convention for percent-decoding query values (issue #611).
            urlencoding::decode(v)
                .map(|d| d.into_owned())
                .unwrap_or_else(|_| v.to_string())
        })
    })
}

/// GET /imposters/:port/scenarios[?flowId=] → `{flowId, scenarios:[{name,state}]}`
pub async fn handle_list_scenarios(
    port: u16,
    query: Option<&str>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let flow_id = flow_id_from_query(query).unwrap_or_else(|| default_flow_id(&imposter));
            let mut scenarios = Vec::new();
            for name in imposter.scenario_names() {
                match imposter.scenario_state(&flow_id, &name) {
                    Ok(state) => {
                        scenarios.push(serde_json::json!({ "name": name, "state": state }))
                    }
                    Err(e) => return backend_error_response(&e),
                }
            }
            json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "scenarios": scenarios }),
            )
        }
        Err(e) => e.into(),
    }
}

/// PUT /imposters/:port/scenarios/:name/state — body `{"state":"…","flowId":"…"?}`
pub async fn handle_set_scenario_state(
    port: u16,
    name: &str,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let payload = match parse_json_body(req).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(state) = payload.get("state").and_then(|v| v.as_str()) else {
        return error_response(StatusCode::BAD_REQUEST, "missing required field: state");
    };
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let flow_id = payload
                .get("flowId")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| default_flow_id(&imposter));
            match imposter.set_scenario_state(&flow_id, name, state) {
                Ok(()) => json_response(
                    StatusCode::OK,
                    &serde_json::json!({ "flowId": flow_id, "name": name, "state": state }),
                ),
                Err(e) => backend_error_response(&e),
            }
        }
        Err(e) => e.into(),
    }
}

/// POST /imposters/:port/scenarios/reset — body `{"flowId":"…"?}` (resets ONLY that flow's slice).
pub async fn handle_reset_scenarios(
    port: u16,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(e.status_code(), &e.to_string()),
    };
    let flow_id_opt = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(v) => v.get("flowId").and_then(|v| v.as_str()).map(String::from),
            Err(e) => {
                return error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {e}"));
            }
        }
    };
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let flow_id = flow_id_opt.unwrap_or_else(|| default_flow_id(&imposter));
            for name in imposter.scenario_names() {
                if let Err(e) = imposter.delete_scenario_state(&flow_id, &name) {
                    return backend_error_response(&e);
                }
            }
            json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "reset": true }),
            )
        }
        Err(e) => e.into(),
    }
}

/// GET /admin/imposters/:port/flow-state/:flow_id/:key → `{flowId,key,value}` | 404
pub async fn handle_get_flow_state(
    port: u16,
    flow_id: &str,
    key: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => match imposter.flow_get(flow_id, key) {
            Ok(Some(value)) => json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "key": key, "value": value }),
            ),
            Ok(None) => error_response(StatusCode::NOT_FOUND, "flow-state key not found"),
            Err(e) => backend_error_response(&e),
        },
        Err(e) => e.into(),
    }
}

/// PUT /admin/imposters/:port/flow-state/:flow_id/:key — body `{"value": …}`
pub async fn handle_put_flow_state(
    port: u16,
    flow_id: &str,
    key: &str,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let payload = match parse_json_body(req).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(value) = payload.get("value") else {
        return error_response(StatusCode::BAD_REQUEST, "missing required field: value");
    };
    match manager.get_imposter(port) {
        Ok(imposter) => match imposter.flow_set(flow_id, key, value.clone()) {
            Ok(()) => json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "key": key, "value": value }),
            ),
            Err(e) => backend_error_response(&e),
        },
        Err(e) => e.into(),
    }
}

/// DELETE /admin/imposters/:port/flow-state/:flow_id — clear every key in a flow (issue #530).
/// Idempotent: clearing an absent/empty flow still returns 200. 404 only when the imposter/port
/// does not exist.
pub async fn handle_clear_flow_state(
    port: u16,
    flow_id: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => match imposter.flow_clear(flow_id) {
            Ok(()) => json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "cleared": true }),
            ),
            Err(e) => backend_error_response(&e),
        },
        Err(e) => e.into(),
    }
}

/// DELETE /admin/imposters/:port/flow-state/:flow_id/:key
pub async fn handle_delete_flow_state(
    port: u16,
    flow_id: &str,
    key: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => match imposter.flow_delete(flow_id, key) {
            Ok(()) => json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "key": key, "deleted": true }),
            ),
            Err(e) => backend_error_response(&e),
        },
        Err(e) => e.into(),
    }
}

// ── Correlated-isolation "space" endpoints (issue #223) ─────────────────────────

/// Every field name a `Stub` recognises, in the JSON spelling a client sends (issue #336).
///
/// Kept beside the check that uses it rather than derived from `StubRaw`: `StubRaw` is private to
/// `rift-mock-core` and its `rename_all = "camelCase"` means the Rust identifiers are not the wire
/// names anyway. The cost is that a new stub field must be added here too — which is why the
/// message this list produces names the fields, so a client hitting it can see immediately whether
/// the list is stale rather than being told only that their body was rejected.
const STUB_FIELD_NAMES: [&str; 12] = [
    "scenarioName",
    "requiredScenarioState",
    "newScenarioState",
    "space",
    "id",
    "routePattern",
    "predicates",
    "rules",
    "responses",
    "recordedFrom",
    "delayRange",
    "_verify",
];

/// Refuse a body that is not shaped like a stub (issue #336).
///
/// `Stub` deserializes through `StubRaw`, where every field is `#[serde(default)]` and unknown keys
/// are discarded — so **any** JSON object deserializes, and an object with only unrecognised keys
/// deserializes to the vacuous stub: no id, no predicates, no responses. That stub is the worst one
/// the engine can hold. No predicates means it matches everything in its space, shadowing the stubs
/// that were meant to answer; no responses means it serves a default nobody authored; and no id
/// means the console cannot address it to delete it. Recovery is tearing down the whole space.
///
/// The trap is reachable by an ordinary typo because the two sibling routes disagree about their
/// envelope: `POST /imposters/:port/stubs` takes `{"stub": {…}}`, this route takes the bare stub.
/// Posting the documented shape of the former here yields exactly the vacuous stub above — the
/// `stub` key is unrecognised and every field it carried is discarded.
///
/// The rule is deliberately about **shape, not emptiness**: it asks the raw JSON whether any
/// recognised field name is present, before serde erases the distinction. After deserialization
/// `{}` and an explicit `{"predicates": []}` are indistinguishable, so a post-hoc emptiness check
/// would also reject the legitimate, explicitly-authored space-wide default. This does not change
/// what a legal `Stub` is; it only refuses bodies that are not stubs at all.
///
/// Scoped to this route on purpose. The space surface is a Rift extension (issue #223), not a
/// Mountebank-parity one, so tightening it breaks no compatibility contract — the imposter-level
/// routes keep `additionalProperties: true` and the bare-`{}`-inside-the-envelope allowance exactly
/// as upstream Mountebank has them.
fn reject_if_not_a_stub(payload: &serde_json::Value) -> Option<Response<Full<Bytes>>> {
    let reason = not_a_stub_reason(payload)?;
    Some(error_response(StatusCode::BAD_REQUEST, &reason))
}

/// Why `payload` is not shaped like a stub, or `None` if it is (or is not an object at all, which
/// deserialization refuses on its own with a better message).
///
/// The decision half of [`reject_if_not_a_stub`], public because an embedder can *terminate* this
/// route instead of proxying to the handler above — the clustered admin front answers it as a
/// replicated write — and must then apply this rule itself. It returns the reason rather than a
/// `Response` because such an embedder renders refusals in its own error envelope; the rule and
/// its wording belong here, the rendering belongs to whoever answers.
///
/// This exists so there is exactly one [`STUB_FIELD_NAMES`]. A second copy in another crate goes
/// stale the moment a stub field is added here, and that staleness is invisible and fails the
/// wrong way: a legitimate stub using the new field is answered `400`.
#[must_use]
pub fn not_a_stub_reason(payload: &serde_json::Value) -> Option<String> {
    let object = payload.as_object()?;
    if object
        .keys()
        .any(|key| STUB_FIELD_NAMES.contains(&key.as_str()))
    {
        return None;
    }
    // Name the actual mistake when we can recognise it. A caller who sent the imposter-level
    // envelope has made a specific, common error, and "no recognised stub field present" would
    // leave them re-reading their stub for a fault that is not in it.
    let message = if object.contains_key("stub") {
        "this route takes the stub object directly — the `{\"stub\": …}` envelope belongs to \
         POST /imposters/{port}/stubs"
            .to_string()
    } else {
        format!(
            "body is not a stub: no recognised stub field present (expected at least one of {})",
            STUB_FIELD_NAMES.join(", ")
        )
    };
    Some(message)
}

/// POST /imposters/:port/spaces/:flowId/stubs — register a stub scoped to that space.
pub async fn handle_add_space_stub(
    port: u16,
    flow_id: &str,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    let payload = match parse_json_body(req).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // Before serde, which cannot tell "not a stub" from "an empty stub" (issue #336).
    if let Some(rejection) = reject_if_not_a_stub(&payload) {
        return rejection;
    }
    let mut stub: Stub = match serde_json::from_value(payload) {
        Ok(s) => s,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("Invalid stub: {e}")),
    };
    // Gate any scripting surface behind --allowInjection before mutating state (B3, issue #355).
    if let Some(rejection) =
        reject_stubs_if_injection_disallowed(std::slice::from_ref(&stub), allow_injection)
    {
        return rejection;
    }
    // Resolve `_rift.script` `file:`/`ref:` sources before persisting (issue #356 B1): escape /
    // unknown-ref / unconfigured `file:` → 400, nothing unresolved is ever stored.
    {
        let registry = imposter_script_registry(&manager, port);
        let base = admin_script_base(&scripts_dir);
        if let Err(e) = resolve_stub_scripts(std::slice::from_mut(&mut stub), &registry, &base) {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Script resolution failed: {e}"),
            );
        }
    }
    // The path `:flowId` is the source of truth for the scope; ignore any `space` in the body.
    stub.space = Some(flow_id.to_string());
    match manager.add_stub(port, stub, None).await {
        Ok(()) => match manager.get_imposter(port) {
            Ok(imposter) => json_response(
                StatusCode::CREATED,
                &serde_json::json!({ "space": flow_id, "stubs": imposter.space_stubs(flow_id) }),
            ),
            Err(e) => e.into(),
        },
        Err(e) => e.into(),
    }
}

/// GET /imposters/:port/spaces/:flowId/stubs — list a space's scoped stubs.
pub async fn handle_list_space_stubs(
    port: u16,
    flow_id: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => json_response(
            StatusCode::OK,
            &serde_json::json!({ "space": flow_id, "stubs": imposter.space_stubs(flow_id) }),
        ),
        Err(e) => e.into(),
    }
}

/// GET /imposters/:port/spaces/:flowId — inspect a space (stubs + scenario states + request count).
pub async fn handle_get_space(
    port: u16,
    flow_id: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let mut scenarios = Vec::new();
            for name in imposter.scenario_names() {
                match imposter.scenario_state(flow_id, &name) {
                    Ok(state) => {
                        scenarios.push(serde_json::json!({ "name": name, "state": state }))
                    }
                    Err(e) => return backend_error_response(&e),
                }
            }
            let number_of_requests = imposter
                .get_recorded_requests()
                .iter()
                .filter(|r| imposter.resolve_flow_id_recorded(&r.headers) == flow_id)
                .count();
            json_response(
                StatusCode::OK,
                &serde_json::json!({
                    "space": flow_id,
                    "stubs": imposter.space_stubs(flow_id),
                    "scenarios": scenarios,
                    "numberOfRequests": number_of_requests
                }),
            )
        }
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port/spaces/:flowId — one-call per-space teardown
/// (scoped stubs + recorded requests + scenario state), never a global reset.
pub async fn handle_teardown_space(
    port: u16,
    flow_id: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.teardown_space(port, flow_id).await {
        Ok(()) => json_response(
            StatusCode::OK,
            &serde_json::json!({ "space": flow_id, "tornDown": true }),
        ),
        Err(e) => e.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::not_a_stub_reason;

    #[test]
    fn a_body_carrying_any_recognised_stub_field_is_a_stub() {
        assert!(not_a_stub_reason(&serde_json::json!({ "predicates": [] })).is_none());
        // Shape, not emptiness: an explicitly-authored space-wide default is legal.
        assert!(not_a_stub_reason(&serde_json::json!({ "responses": [] })).is_none());
        assert!(not_a_stub_reason(&serde_json::json!({ "_verify": {} })).is_none());
    }

    #[test]
    fn the_imposter_level_envelope_is_named_rather_than_described() {
        let reason = not_a_stub_reason(&serde_json::json!({
            "stub": { "predicates": [], "responses": [] }
        }))
        .expect("the envelope mistake is not a stub");
        assert!(
            reason.contains("envelope"),
            "a caller who sent the sibling route's documented shape has made a specific mistake \
             and must be told which one: {reason}"
        );
    }

    #[test]
    fn a_body_with_no_recognised_field_lists_the_fields_it_wanted() {
        let reason = not_a_stub_reason(&serde_json::json!({ "complete": "nonsense", "zzz": 123 }))
            .expect("no recognised stub field is present");
        assert!(
            reason.contains("no recognised stub field present"),
            "{reason}"
        );
        // The message names the list so a caller can see whether it is stale, rather than being
        // told only that their body was refused.
        assert!(reason.contains("predicates"), "{reason}");
    }

    #[test]
    fn a_payload_that_is_not_an_object_is_left_to_deserialization() {
        // Not "no reason to refuse" — a different refusal, with a better message, one step later.
        // The guard only classifies objects; anything else fails `from_value::<Stub>` on its own.
        assert!(not_a_stub_reason(&serde_json::json!([])).is_none());
        assert!(not_a_stub_reason(&serde_json::json!("a string")).is_none());
        assert!(not_a_stub_reason(&serde_json::Value::Null).is_none());
    }
}
