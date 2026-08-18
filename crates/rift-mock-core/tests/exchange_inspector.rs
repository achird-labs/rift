//! The exchange inspector seam (issue #966): a synchronous, per-imposter hook pair that sees a
//! request before it is matched and the response before it is written, and may replace either.
//! See `crate::extensions::exchange_inspector` for the full contract.
//!
//! These tests drive the real imposter serve loop end-to-end (`ImposterManager` + reqwest, the
//! same pattern as `journal_match_outcome.rs`) and pin:
//!   - a request-side reject: the client sees the reject verbatim, the request is still
//!     journaled (with the rejection's status), and a rejected request never advances a
//!     response cycler — matching never ran for it
//!   - a response-side reject: the inspector sees the response the imposter actually built
//!     (and the same request view the request-side hook saw), and the client sees the
//!     replacement instead
//!   - `Proceed` on both hooks changes nothing observable — byte-identical to an imposter with
//!     no inspector at all
//!   - the provider is consulted once per imposter, with that imposter's own config; `None`
//!     means that imposter gets no hooks, and a provider installed for one imposter never
//!     touches another
//!   - a disabled imposter reaches neither hook

use bytes::Bytes;
use parking_lot::Mutex;
use rift_mock_core::extensions::exchange_inspector::{
    ExchangeInspector, ExchangeInspectorProvider, InspectRequest, InspectResponse, InspectVerdict,
};
use rift_mock_core::imposter::{ImposterConfig, ImposterManager};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

async fn create(manager: &ImposterManager, cfg: serde_json::Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("valid imposter config");
    let port = manager.create_imposter(config).await.expect("create");
    // Give the listener a moment to bind before the request (matches the sibling HTTP tests).
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

async fn get(port: u16, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("send")
}

/// A canned verdict, cheap to construct in test bodies.
#[derive(Debug, Clone)]
enum Verdict {
    Proceed,
    Reject {
        status: u16,
        content_type: &'static str,
        body: &'static [u8],
    },
}

impl From<Verdict> for InspectVerdict {
    fn from(v: Verdict) -> Self {
        match v {
            Verdict::Proceed => InspectVerdict::Proceed,
            Verdict::Reject {
                status,
                content_type,
                body,
            } => InspectVerdict::Reject {
                status,
                content_type: content_type.to_string(),
                body: Bytes::from_static(body),
            },
        }
    }
}

/// A `(method, path)` pair as the inspector saw it.
type RequestCall = (String, String);
/// A `(status, body, method, path)` tuple as the inspector saw the built response and the
/// request that produced it.
type ResponseCall = (u16, Vec<u8>, String, String);

/// A recording [`ExchangeInspector`]: each hook pops the next queued verdict (falling back to a
/// default once the queue is drained) and records what it was called with.
struct RecordingInspector {
    request_verdicts: Mutex<VecDeque<Verdict>>,
    default_request_verdict: Verdict,
    response_verdicts: Mutex<VecDeque<Verdict>>,
    default_response_verdict: Verdict,
    request_calls: Mutex<Vec<RequestCall>>,
    response_calls: Mutex<Vec<ResponseCall>>,
}

impl RecordingInspector {
    fn new(default_request: Verdict, default_response: Verdict) -> Self {
        Self {
            request_verdicts: Mutex::new(VecDeque::new()),
            default_request_verdict: default_request,
            response_verdicts: Mutex::new(VecDeque::new()),
            default_response_verdict: default_response,
            request_calls: Mutex::new(Vec::new()),
            response_calls: Mutex::new(Vec::new()),
        }
    }

    /// Queue a one-shot verdict for the next `inspect_request` call.
    fn queue_request_verdict(&self, verdict: Verdict) {
        self.request_verdicts.lock().push_back(verdict);
    }

    /// Queue a one-shot verdict for the next `inspect_response` call.
    fn queue_response_verdict(&self, verdict: Verdict) {
        self.response_verdicts.lock().push_back(verdict);
    }
}

impl ExchangeInspector for RecordingInspector {
    fn inspect_request(&self, req: &InspectRequest<'_>) -> InspectVerdict {
        self.request_calls
            .lock()
            .push((req.method.to_string(), req.path.to_string()));
        let verdict = self
            .request_verdicts
            .lock()
            .pop_front()
            .unwrap_or_else(|| self.default_request_verdict.clone());
        verdict.into()
    }

    fn inspect_response(
        &self,
        req: &InspectRequest<'_>,
        resp: &InspectResponse<'_>,
    ) -> InspectVerdict {
        self.response_calls.lock().push((
            resp.status,
            resp.body.to_vec(),
            req.method.to_string(),
            req.path.to_string(),
        ));
        let verdict = self
            .response_verdicts
            .lock()
            .pop_front()
            .unwrap_or_else(|| self.default_response_verdict.clone());
        verdict.into()
    }
}

/// Installs the same inspector for every imposter, unconditionally.
struct AlwaysProvide(Arc<dyn ExchangeInspector>);

impl ExchangeInspectorProvider for AlwaysProvide {
    fn provide(&self, _config: &ImposterConfig) -> Option<Arc<dyn ExchangeInspector>> {
        Some(Arc::clone(&self.0))
    }
}

/// Installs the inspector only for the imposter whose config carries `only_name`, and records
/// every `name` it was consulted with (so a test can assert it was asked once per imposter).
struct NamedGateProvider {
    only_name: &'static str,
    inspector: Arc<dyn ExchangeInspector>,
    provide_calls: Mutex<Vec<Option<String>>>,
}

impl ExchangeInspectorProvider for NamedGateProvider {
    fn provide(&self, config: &ImposterConfig) -> Option<Arc<dyn ExchangeInspector>> {
        self.provide_calls.lock().push(config.name.clone());
        if config.name.as_deref() == Some(self.only_name) {
            Some(Arc::clone(&self.inspector))
        } else {
            None
        }
    }
}

// (a) A request-side reject: the client sees the reject verbatim, the request is journaled
// anyway (carrying the rejection's status), and — because rejection runs before matching — a
// cycling stub's cursor never advances for the rejected request. A following `Proceed` request
// therefore still lands on response #1, not #2.
#[tokio::test]
async fn request_side_reject_blocks_before_matching_and_leaves_the_cycler_untouched() {
    let inspector = Arc::new(RecordingInspector::new(Verdict::Proceed, Verdict::Proceed));
    inspector.queue_request_verdict(Verdict::Reject {
        status: 429,
        content_type: "application/json",
        body: b"{\"blocked\":true}",
    });
    let manager = ImposterManager::new()
        .with_exchange_inspector_provider(Arc::new(AlwaysProvide(inspector.clone())));
    let port = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http", "recordRequests": true,
            "stubs": [{
                "responses": [
                    { "is": { "statusCode": 200, "body": "one" } },
                    { "is": { "statusCode": 200, "body": "two" } }
                ]
            }]
        }),
    )
    .await;

    // First request: rejected before it ever reaches matching.
    let resp = get(port, "/cycle").await;
    assert_eq!(resp.status(), 429, "the client sees the reject status");
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        resp.bytes().await.expect("body").as_ref(),
        b"{\"blocked\":true}"
    );

    let imposter = manager.get_imposter(port).expect("imposter exists");
    let recorded = imposter.get_recorded_requests();
    assert_eq!(recorded.len(), 1, "the rejected request is still journaled");
    assert_eq!(
        recorded[0].status,
        Some(429),
        "the journal entry carries the rejection's status"
    );

    // Second request: proceeds normally. If the first (rejected) request had advanced the
    // cycler, this would land on response #2 ("two") instead of #1.
    let resp2 = get(port, "/cycle").await;
    assert_eq!(resp2.status(), 200);
    assert_eq!(
        resp2.text().await.expect("body"),
        "one",
        "a request that never matched must not have advanced the response cycler"
    );

    manager.delete_all().await;
}

// (b) A response-side reject: the inspector observes the response the imposter actually built
// (status + body) and the same request view (method/path) the request-side hook saw; the client
// sees the replacement, not the original.
#[tokio::test]
async fn response_side_reject_replaces_the_built_response() {
    let inspector = Arc::new(RecordingInspector::new(Verdict::Proceed, Verdict::Proceed));
    inspector.queue_response_verdict(Verdict::Reject {
        status: 503,
        content_type: "application/json",
        body: b"{\"replaced\":true}",
    });
    let manager = ImposterManager::new()
        .with_exchange_inspector_provider(Arc::new(AlwaysProvide(inspector.clone())));
    let port = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "original" } }] }]
        }),
    )
    .await;

    let resp = get(port, "/resp-reject").await;
    assert_eq!(resp.status(), 503, "the client sees the replacement");
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        resp.bytes().await.expect("body").as_ref(),
        b"{\"replaced\":true}"
    );

    {
        let response_calls = inspector.response_calls.lock();
        assert_eq!(response_calls.len(), 1);
        let (status, body, method, path) = &response_calls[0];
        assert_eq!(*status, 200, "the inspector saw the ORIGINAL built status");
        assert_eq!(body.as_slice(), b"original", "and the original built body");
        assert_eq!(method, "GET");
        assert_eq!(path, "/resp-reject");
    }

    manager.delete_all().await;
}

// (c) `Proceed` on both hooks must be a no-op on the wire: byte-identical (aside from `date`) to
// an imposter with no inspector installed at all — both hooks ran (proven by the call counts),
// but neither changed anything observable.
#[tokio::test]
async fn proceed_on_both_hooks_is_byte_identical_to_no_inspector() {
    let inspector = Arc::new(RecordingInspector::new(Verdict::Proceed, Verdict::Proceed));
    let with_inspector = ImposterManager::new()
        .with_exchange_inspector_provider(Arc::new(AlwaysProvide(inspector.clone())));
    let without_inspector = ImposterManager::new();

    let stub_cfg = serde_json::json!({
        "port": 0, "protocol": "http",
        "stubs": [{
            "responses": [{
                "is": {
                    "statusCode": 200,
                    "body": "unchanged",
                    "headers": { "X-Custom": "yes" }
                }
            }]
        }]
    });
    let port_with = create(&with_inspector, stub_cfg.clone()).await;
    let port_without = create(&without_inspector, stub_cfg).await;

    let resp_with = get(port_with, "/proceed").await;
    let status_with = resp_with.status();
    let mut headers_with = resp_with.headers().clone();
    headers_with.remove("date");
    let body_with = resp_with.bytes().await.expect("body");

    let resp_without = get(port_without, "/proceed").await;
    let status_without = resp_without.status();
    let mut headers_without = resp_without.headers().clone();
    headers_without.remove("date");
    let body_without = resp_without.bytes().await.expect("body");

    assert_eq!(status_with, status_without);
    assert_eq!(headers_with, headers_without);
    assert_eq!(body_with, body_without);

    assert_eq!(
        inspector.request_calls.lock().len(),
        1,
        "the request-side hook did run"
    );
    assert_eq!(
        inspector.response_calls.lock().len(),
        1,
        "the response-side hook did run"
    );

    with_inspector.delete_all().await;
    without_inspector.delete_all().await;
}

// (d) The provider is consulted once per imposter, with that imposter's own config. `None`
// means no hooks for that imposter — a provider installed for one imposter must never reach
// another's traffic.
#[tokio::test]
async fn provider_is_consulted_per_imposter_and_none_means_no_hooks() {
    let inspector = Arc::new(RecordingInspector::new(
        Verdict::Reject {
            status: 403,
            content_type: "application/json",
            body: b"{\"gated\":true}",
        },
        Verdict::Proceed,
    ));
    let provider = Arc::new(NamedGateProvider {
        only_name: "gated",
        inspector: inspector.clone(),
        provide_calls: Mutex::new(Vec::new()),
    });
    let manager =
        ImposterManager::new().with_exchange_inspector_provider(Arc::clone(&provider) as _);

    let gated_port = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http", "name": "gated",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "gated-ok" } }] }]
        }),
    )
    .await;
    let plain_port = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http", "name": "plain",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "plain-ok" } }] }]
        }),
    )
    .await;

    assert_eq!(
        provider.provide_calls.lock().clone(),
        vec![Some("gated".to_string()), Some("plain".to_string())],
        "the provider is consulted once per imposter, with that imposter's own config"
    );

    let gated_resp = get(gated_port, "/x").await;
    assert_eq!(
        gated_resp.status(),
        403,
        "the gated imposter got the inspector"
    );

    let plain_resp = get(plain_port, "/x").await;
    assert_eq!(
        plain_resp.status(),
        200,
        "the ungated imposter's traffic must never reach the inspector"
    );
    assert_eq!(plain_resp.text().await.expect("body"), "plain-ok");

    assert_eq!(
        inspector.request_calls.lock().len(),
        1,
        "the inspector was called exactly once — for the gated imposter's request only"
    );

    manager.delete_all().await;
}

// (e) A disabled imposter returns its 503 before there is anything to inspect: neither hook
// runs.
#[tokio::test]
async fn disabled_imposter_never_reaches_either_hook() {
    let inspector = Arc::new(RecordingInspector::new(Verdict::Proceed, Verdict::Proceed));
    let manager = ImposterManager::new()
        .with_exchange_inspector_provider(Arc::new(AlwaysProvide(inspector.clone())));
    let port = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }]
        }),
    )
    .await;
    manager
        .get_imposter(port)
        .expect("imposter exists")
        .set_enabled(false);

    let resp = get(port, "/x").await;
    assert_eq!(resp.status(), 503);

    assert!(
        inspector.request_calls.lock().is_empty(),
        "the request-side hook must not run for a disabled imposter"
    );
    assert!(
        inspector.response_calls.lock().is_empty(),
        "the response-side hook must not run for a disabled imposter"
    );

    manager.delete_all().await;
}
