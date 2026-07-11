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
/// chain → `503 {"error":"backendUnavailable",...}`; anything else → `500
/// {"error":"internalError",...}`. Never a silent fallback.
pub fn backend_error_response(err: &anyhow::Error) -> Response<Full<Bytes>> {
    let (status, body) = match err.downcast_ref::<BackendUnavailable>() {
        Some(b) => (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({
                "error": "backendUnavailable",
                "feature": b.feature,
                "detail": b.detail,
            }),
        ),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({
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
        assert_eq!(json["error"], "backendUnavailable");
        assert_eq!(json["feature"], "flowState");
        assert_eq!(json["detail"], "redis connection refused");
    }

    #[tokio::test]
    async fn other_errors_map_to_500() {
        let resp = backend_error_response(&anyhow::anyhow!("boom"));
        assert_eq!(resp.status(), hyper::StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(json["error"], "internalError");
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
