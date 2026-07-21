//! Typed backend errors, per-request op annotations, and the response-decorator hook
//! (issue #318).
//!
//! Backends attach [`BackendUnavailable`] to a failed op (as the source of their
//! `anyhow::Error`); response boundaries map it to a structured 503 via
//! [`backend_error_response`]. Operational metadata travels per request through a tokio
//! task-local annotation scope: the server opens one per request task, backends call
//! [`annotate`] from sync code inside that task, and the server hands the collected
//! annotations to the configured [`ResponseDecorator`] before the response is written.
//! Annotations from other threads (e.g. script-pool workers) are best-effort: outside a
//! scope, [`annotate`] is an infallible no-op.

use crate::util::build_response_with_headers;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use std::cell::RefCell;

/// Attached by backends to a failed op; response boundaries map it to a structured 503.
#[derive(Debug, thiserror::Error)]
#[error("backend unavailable: {feature}: {detail}")]
pub struct BackendUnavailable {
    pub feature: &'static str,
    pub detail: String,
}

tokio::task_local! {
    static ANNOTATIONS: RefCell<Vec<(&'static str, String)>>;
}

/// Append a per-request operation annotation. Cheap and infallible; a no-op when no
/// annotation scope is open on the current task (documented best-effort for calls from
/// non-request threads).
pub fn annotate(key: &'static str, value: String) {
    let _ = ANNOTATIONS.try_with(|a| a.borrow_mut().push((key, value)));
}

/// Run `fut` inside a fresh annotation scope and return its output together with the
/// annotations collected while it ran. Task-locals follow the task across `.await`s, so
/// synchronous backend calls made anywhere inside the request task land in this scope.
pub async fn with_annotation_scope<F: Future>(fut: F) -> (F::Output, Vec<(&'static str, String)>) {
    ANNOTATIONS
        .scope(RefCell::new(Vec::new()), async move {
            let out = fut.await;
            let collected = ANNOTATIONS.with(|a| a.borrow_mut().drain(..).collect());
            (out, collected)
        })
        .await
}

/// Which response surface a decorator is being invoked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsePhase {
    /// A response served by an imposter (per-imposter port traffic).
    DataPlane,
    /// A response served by the admin API (including the `/__rift/` gateway, which rides
    /// the admin listener).
    Admin,
}

/// Inspect/annotate an outgoing response (headers only; the body is untouched). Invoked
/// synchronously on the response path — keep implementations fast and non-blocking, and
/// never panic: a panic tears down the connection serving the request.
pub trait ResponseDecorator: Send + Sync {
    fn decorate(
        &self,
        phase: ResponsePhase,
        req_port: Option<u16>,
        annotations: &[(&'static str, String)],
        headers: &mut hyper::HeaderMap,
    );
}

/// Map a backend/handler error to its response: [`BackendUnavailable`] anywhere in the
/// chain → `503`; anything else → `500`. Never a silent fallback.
///
/// Serves **two shapes in one body** (issue #800). This door predates the #611/#797 envelope
/// sweeps and was the last one still serving only its own `{"error":"backendUnavailable",…}`
/// shape, so a client that followed #797's guidance and switched to reading `errors[0].type` got
/// nothing here. It now carries the Mountebank envelope *as well*:
///
/// - `errors[0]` — the envelope, with the `type` slug new consumers branch on. `feature`/`detail`
///   ride inside the error object rather than being flattened into `message`, because naming
///   *which* backend failed is this door's whole purpose.
/// - top-level `error`/`feature`/`detail` — the legacy 0.15.0 keys, **frozen**. Deprecated in
///   0.16.0, removed in 0.17.0 by issue #801. They stay because `rift-enterprise` parses
///   them today; dropping them here would break the distributed edition mid-bump.
pub fn backend_error_response(err: &anyhow::Error) -> Response<Full<Bytes>> {
    let (status, body) = match err.downcast_ref::<BackendUnavailable>() {
        Some(b) => (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({
                "errors": [{
                    "code": StatusCode::SERVICE_UNAVAILABLE.as_str(),
                    "type": crate::response::ErrorKind::BackendUnavailable.slug(),
                    "message": format!("{}: {}", b.feature, b.detail),
                    "feature": b.feature,
                    "detail": b.detail,
                }],
                "error": "backendUnavailable",
                "feature": b.feature,
                "detail": b.detail,
            }),
        ),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({
                "errors": [{
                    "code": StatusCode::INTERNAL_SERVER_ERROR.as_str(),
                    "type": crate::response::ErrorKind::InternalError.slug(),
                    "message": format!("{err:#}"),
                }],
                "error": "internalError",
                // "{err:#}" keeps the whole context chain — the outermost message alone
                // rarely says why ("Redis GET failed" without the refused connection).
                "detail": format!("{err:#}"),
            }),
        ),
    };
    // The response body is otherwise the only record of this failure — keep operators
    // in the loop without requiring a client bug report.
    tracing::warn!("backend error surfaced as {}: {err:#}", status.as_u16());
    build_response_with_headers(
        status,
        [("content-type", "application/json")],
        body.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    // AC1: annotate inside a scope is collected (and follows the task across awaits).
    #[tokio::test]
    async fn annotations_collected_inside_scope_across_awaits() {
        let ((), notes) = with_annotation_scope(async {
            annotate("first", "1".to_string());
            tokio::task::yield_now().await;
            annotate("second", "2".to_string());
        })
        .await;
        assert_eq!(
            notes,
            vec![("first", "1".to_string()), ("second", "2".to_string()),]
        );
    }

    // AC1: annotate outside any scope is an infallible no-op.
    #[test]
    fn annotate_outside_scope_is_noop() {
        annotate("orphan", "x".to_string());
    }

    #[tokio::test]
    async fn scopes_start_empty_and_do_not_leak() {
        annotate("orphan", "x".to_string());
        let ((), notes) = with_annotation_scope(async {}).await;
        assert!(
            notes.is_empty(),
            "a fresh scope must not see outside writes"
        );
    }

    // The error mapper: BackendUnavailable → structured 503; anything else → 500.
    #[tokio::test]
    async fn backend_unavailable_maps_to_structured_503() {
        let err = anyhow::Error::new(BackendUnavailable {
            feature: "flowState",
            detail: "redis connection refused".to_string(),
        });
        let resp = backend_error_response(&err);
        assert_eq!(resp.status(), hyper::StatusCode::SERVICE_UNAVAILABLE);
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        // Legacy top-level keys (issue #800 AC2): byte-identical to 0.15.0. Deprecated in 0.16.0
        // and removed in 0.17.0 — these three assertions are what the removal issue deletes, and
        // until then they are what stops the deprecation window closing by accident.
        assert_eq!(json["error"], "backendUnavailable");
        assert_eq!(json["feature"], "flowState");
        assert_eq!(json["detail"], "redis connection refused");

        // AC1: the door now also serves the Mountebank envelope with the #797 `type` slug.
        assert_eq!(
            json["errors"][0]["code"], "503",
            "code is the status string, not a slug"
        );
        assert_eq!(
            json["errors"][0]["type"], "backend unavailable",
            "a dedicated slug — generic `unavailable` would not distinguish a backend outage \
             from any other 503, which is the whole reason this door exists"
        );
        assert!(
            json["errors"][0]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty()),
            "message must be non-empty, got: {json}"
        );
        // AC3: the structured split stays machine-readable inside the envelope — `feature` names
        // WHICH backend failed, which is the door's entire value and must not be flattened away.
        assert_eq!(json["errors"][0]["feature"], "flowState");
        assert_eq!(json["errors"][0]["detail"], "redis connection refused");
    }

    #[tokio::test]
    async fn other_errors_map_to_500() {
        let resp = backend_error_response(&anyhow::anyhow!("boom"));
        assert_eq!(resp.status(), hyper::StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        // Legacy key, frozen (AC2).
        assert_eq!(json["error"], "internalError");
        // AC1: envelope on this branch too — a generic internal error is exactly what it is.
        assert_eq!(json["errors"][0]["code"], "500");
        assert_eq!(json["errors"][0]["type"], "internal error");
        assert!(
            json["errors"][0]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty()),
            "message must carry the context chain, got: {json}"
        );
        // The 500 branch has no feature/detail split to preserve — it must not invent one.
        assert!(
            json["errors"][0]["feature"].is_null() && json["errors"][0]["detail"].is_null(),
            "the non-backend branch has neither `feature` nor `detail` inside the envelope, \
             got: {json}"
        );
    }

    // Drift tripwire, not an independent equivalence proof: both shapes are populated from the
    // same `b.feature`/`b.detail` in one `json!` literal, so this passes trivially today. Its job
    // is to fail the moment someone edits one shape and not the other — e.g. a fix applied only to
    // the legacy keys — which is how the two would silently start describing different failures.
    #[tokio::test]
    async fn legacy_and_envelope_shapes_agree_on_the_same_error() {
        let err = anyhow::Error::new(BackendUnavailable {
            feature: "proxyStore",
            detail: "connection reset".to_string(),
        });
        let resp = backend_error_response(&err);
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(json["feature"], json["errors"][0]["feature"]);
        assert_eq!(json["detail"], json["errors"][0]["detail"]);
        let msg = json["errors"][0]["message"].as_str().expect("message");
        assert!(
            msg.contains("proxyStore") && msg.contains("connection reset"),
            "message must carry both halves of the legacy split, got: {msg}"
        );
    }

    // BackendUnavailable survives an anyhow context chain (backends wrap with .context()).
    #[tokio::test]
    async fn downcast_works_through_context_chain() {
        use anyhow::Context;
        let err = Err::<(), _>(anyhow::Error::new(BackendUnavailable {
            feature: "flowState",
            detail: "down".to_string(),
        }))
        .context("while reading scenario state")
        .expect_err("err");
        let resp = backend_error_response(&err);
        assert_eq!(resp.status(), hyper::StatusCode::SERVICE_UNAVAILABLE);
    }
}
