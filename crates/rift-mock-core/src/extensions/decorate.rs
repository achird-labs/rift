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
pub async fn with_annotation_scope<F: Future>(fut: F) -> (F::Output, Annotations) {
    ANNOTATIONS
        .scope(RefCell::new(Vec::new()), async move {
            let out = fut.await;
            let collected = ANNOTATIONS.with(|a| a.borrow_mut().drain(..).collect());
            (out, collected)
        })
        .await
}

/// Annotations collected on a thread that had no request scope, waiting to be replayed onto the
/// request task by whoever awaited the work.
pub(crate) type Annotations = Vec<(&'static str, String)>;

/// The synchronous twin of [`with_annotation_scope`], for work that has left the request task.
///
/// `spawn_blocking` runs its closure on a pool thread that carries no task-locals, so an
/// [`annotate`] made there hits the no-op path and is silently dropped (issue #987). Running the
/// closure under a fresh scope on *that* thread collects those annotations so the caller can
/// [`replay_annotations`] them once it is back on the request task.
#[must_use = "dropping this discards the annotations collected off-task, which is the exact \
              failure issue #987 fixed -- pass them to `replay_annotations`"]
pub(crate) fn with_sync_annotation_scope<T>(f: impl FnOnce() -> T) -> (T, Annotations) {
    ANNOTATIONS.sync_scope(RefCell::new(Vec::new()), || {
        let out = f();
        let collected = ANNOTATIONS.with(|a| a.borrow_mut().drain(..).collect());
        (out, collected)
    })
}

/// Append annotations collected off-task to the current task's scope, in order. Like [`annotate`],
/// a no-op outside a scope.
pub(crate) fn replay_annotations(collected: Annotations) {
    for (key, value) in collected {
        annotate(key, value);
    }
}

/// The way request-path code spawns blocking work: the closure runs under its own annotation scope
/// and the handle yields what it annotated alongside its result, for [`replay_annotations`] on the
/// request task (issue #987).
///
/// Prefer this over a bare `tokio::task::spawn_blocking` anywhere a request-scoped annotation could
/// be made — which is anywhere the offloaded work can reach a [`FlowStore`](crate::extensions::flow_state::FlowStore),
/// since a failing backend annotates the op it failed on.
///
/// `spawn_blocking` is not the only way off the request task: the Mountebank JS pool
/// (`scripting::js_engine`'s `with_mb_js_thread`) is a second thread hop that carries no
/// task-locals either, and has no equivalent seam. That is harmless only because no flow store is
/// ever bound on an MB worker — binding `ctx.state` there would reintroduce this bug with nothing
/// to catch it.
pub(crate) fn spawn_blocking_annotated<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<(T, Annotations)> {
    tokio::task::spawn_blocking(move || with_sync_annotation_scope(f))
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
/// Serves the Mountebank envelope only. This door predates the #611/#797 envelope sweeps and
/// served its own `{"error":"backendUnavailable",…}` shape through 0.15.0; #800 added the
/// envelope alongside those keys, and #801 removed them in 0.18.0 after the deprecation window
/// (deprecated 0.16.0, retained through 0.17.0, no consumer left parsing them — confirmed via
/// rift-enterprise#90).
///
/// `errors[0]` carries the `type` slug consumers branch on; `feature`/`detail` ride inside the
/// error object rather than being flattened into `message`, because naming *which* backend
/// failed is this door's whole purpose.
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
            }),
        ),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({
                "errors": [{
                    "code": StatusCode::INTERNAL_SERVER_ERROR.as_str(),
                    "type": crate::response::ErrorKind::InternalError.slug(),
                    // "{err:#}" keeps the whole context chain — the outermost message alone
                    // rarely says why ("Redis GET failed" without the refused connection).
                    "message": format!("{err:#}"),
                }],
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

    // Issue #987: `spawn_blocking` runs on a pool thread that carries no task-locals, so an
    // `annotate` made there is silently dropped. These pin the sync-scope + replay mechanism that
    // carries them back.

    // Edge case 2 + 1 + 3: annotations made off-thread are collected AND land in order relative to
    // the inline ones around them. Ordering is the property the replay could plausibly get wrong.
    #[tokio::test]
    async fn sync_scope_collects_off_thread_and_replay_lands_in_order() {
        let ((), notes) = with_annotation_scope(async {
            annotate("before", "1".to_string());
            // A real OS thread, not just a different task: this is what `spawn_blocking` does, and
            // it is what makes the task-local invisible.
            let collected = std::thread::spawn(|| {
                let ((), notes) = with_sync_annotation_scope(|| {
                    annotate("off-a", "2".to_string());
                    annotate("off-b", "3".to_string());
                });
                notes
            })
            .join()
            .expect("probe thread");
            replay_annotations(collected);
            annotate("after", "4".to_string());
        })
        .await;

        assert_eq!(
            notes,
            vec![
                ("before", "1".to_string()),
                ("off-a", "2".to_string()),
                ("off-b", "3".to_string()),
                ("after", "4".to_string()),
            ]
        );
    }

    // Edge case 4 + 13: an offload that annotates nothing replays nothing, and the closure's return
    // value still round-trips through the tuple.
    #[test]
    fn sync_scope_returns_the_value_and_an_empty_vec_when_nothing_annotates() {
        let (value, notes) = with_sync_annotation_scope(|| 7_i64);
        assert_eq!(value, 7);
        assert!(notes.is_empty());
    }

    // Edge case 6: `sync_scope` shadows rather than inherits — a fresh scope must not observe the
    // outer task's annotations, or replay would double-count them.
    #[tokio::test]
    async fn a_sync_scope_starts_empty_even_inside_an_outer_scope() {
        let ((), notes) = with_annotation_scope(async {
            annotate("outer", "1".to_string());
            let ((), inner) = with_sync_annotation_scope(|| {
                annotate("inner", "2".to_string());
            });
            assert_eq!(
                inner,
                vec![("inner", "2".to_string())],
                "the sync scope must not see the outer scope's entries"
            );
            replay_annotations(inner);
        })
        .await;

        // Edge case 7: exactly one `outer` and one `inner` — nothing leaked or double-counted.
        assert_eq!(
            notes,
            vec![("outer", "1".to_string()), ("inner", "2".to_string())]
        );
    }

    // Edge case 5: replay outside any scope is a silent no-op, exactly like `annotate` itself.
    #[test]
    fn replay_outside_a_scope_is_a_noop() {
        replay_annotations(vec![("orphan", "x".to_string())]);
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
        // Issue #801: the deprecation window is closed — the legacy 0.15.0 top-level keys are
        // gone, and these assertions are what stops them creeping back.
        assert!(
            json.get("error").is_none()
                && json.get("feature").is_none()
                && json.get("detail").is_none(),
            "top-level legacy keys were removed in 0.18.0 (#801), got: {json}"
        );

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
        // The exact join both docs promise — coverage previously carried by the deleted
        // dual-shape tripwire test, and independent of the legacy keys' existence.
        assert_eq!(
            json["errors"][0]["message"], "flowState: redis connection refused",
            "message is the `feature: detail` join the docs show verbatim"
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
        // Issue #801: no legacy keys on the 500 branch either.
        assert!(
            json.get("error").is_none() && json.get("detail").is_none(),
            "top-level legacy keys were removed in 0.18.0 (#801), got: {json}"
        );
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
