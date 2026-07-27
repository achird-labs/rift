//! Issue #854: the `AdminAuthorizer` hook — inert by default, `403` on deny, and an ordering
//! guarantee that authentication always precedes route parsing.
//!
//! The ordering is the security-relevant part and is what most of this file is about. If the
//! api-key gate ever moved below route parsing, an unauthenticated request to an unknown path
//! would answer `404` instead of `401`, and unknown-path responses would become an
//! unauthenticated route-existence oracle.

use parking_lot::Mutex;
use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use rift_mock_core::extensions::authz::{AdminAuthorizer, AuthzDecision, AuthzRequest, actions};
use std::sync::Arc;

fn imposter_cfg(v: serde_json::Value) -> ImposterConfig {
    serde_json::from_value(v).expect("test imposter config")
}

/// What the hook was asked, flattened so assertions do not depend on borrowed lifetimes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenCall {
    action: String,
    port: Option<u16>,
    space: Option<String>,
    scope: Option<String>,
    credential: Option<String>,
    params: Vec<(String, String)>,
}

struct SpyAuthorizer {
    calls: Mutex<Vec<SeenCall>>,
    decision: AuthzDecision,
}

impl SpyAuthorizer {
    fn new(decision: AuthzDecision) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            decision,
        })
    }
    fn calls(&self) -> Vec<SeenCall> {
        self.calls.lock().clone()
    }
}

impl AdminAuthorizer for SpyAuthorizer {
    fn authorize(&self, req: AuthzRequest<'_>) -> AuthzDecision {
        self.calls.lock().push(SeenCall {
            action: req.action.to_string(),
            port: req.port,
            space: req.space.map(str::to_string),
            scope: req.scope.map(str::to_string),
            credential: req.credential.map(str::to_string),
            params: req
                .params
                .iter()
                .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
                .collect(),
        });
        self.decision.clone()
    }
}

async fn start_admin(
    admin_port: u16,
    api_key: Option<&str>,
    authorizer: Option<Arc<dyn AdminAuthorizer>>,
) {
    let manager = Arc::new(ImposterManager::new());
    let mut server = AdminApiServer::new(
        format!("127.0.0.1:{admin_port}").parse().expect("addr"),
        manager,
        api_key.map(str::to_string),
    );
    if let Some(a) = authorizer {
        server = server.with_admin_authorizer(a);
    }
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
}

fn base(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn no_authorizer_installed_changes_nothing() {
    // The seam's premise: install nothing, behave exactly as before.
    let port = 14801;
    start_admin(port, None, None).await;

    let resp = reqwest::get(format!("{}/imposters", base(port)))
        .await
        .expect("list imposters");
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn deny_on_an_authenticated_request_is_403_not_401() {
    // 401 would tell an authenticated caller to re-authenticate, sending them round a loop that
    // cannot succeed. The credential was fine; the permission was not.
    let port = 14802;
    let spy = SpyAuthorizer::new(AuthzDecision::Deny {
        reason: "not your tenant",
    });
    start_admin(port, Some("s3cret"), Some(spy.clone())).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/imposters", base(port)))
        .header("authorization", "s3cret")
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 403);

    // The house envelope (#797): `code` is the status, `type` is the slug. They must not be
    // identical strings — a client has to be able to tell them apart.
    let body: serde_json::Value = resp.json().await.expect("json envelope");
    assert_eq!(body["errors"][0]["code"], "403");
    assert_eq!(body["errors"][0]["type"], "insufficient access");
    assert_eq!(body["errors"][0]["message"], "not your tenant");
}

#[tokio::test]
async fn allow_lets_the_request_through() {
    let port = 14803;
    let spy = SpyAuthorizer::new(AuthzDecision::Allow {
        principal: Some("alice".into()),
    });
    start_admin(port, Some("s3cret"), Some(spy.clone())).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/imposters", base(port)))
        .header("authorization", "s3cret")
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    assert_eq!(spy.calls().len(), 1);
}

#[tokio::test]
async fn an_unauthenticated_request_is_401_and_never_reaches_the_authorizer() {
    // Both halves matter. The 401 is the ordering guarantee; the empty call log proves the hook
    // cannot be used as an oracle by an anonymous caller, and that an authorizer cannot
    // accidentally *grant* access to a request that failed authentication.
    let port = 14804;
    let spy = SpyAuthorizer::new(AuthzDecision::allow());
    start_admin(port, Some("s3cret"), Some(spy.clone())).await;

    let resp = reqwest::get(format!("{}/imposters", base(port)))
        .await
        .expect("request");
    assert_eq!(resp.status(), 401);
    assert!(
        spy.calls().is_empty(),
        "the hook must not see unauthenticated requests, saw: {:?}",
        spy.calls()
    );
}

#[tokio::test]
async fn an_unauthenticated_request_to_an_unknown_path_is_401_not_404() {
    // The route-existence oracle. If authentication ever moved after route parsing this would
    // answer 404 and leak which admin routes exist to an anonymous caller.
    let port = 14805;
    let spy = SpyAuthorizer::new(AuthzDecision::allow());
    start_admin(port, Some("s3cret"), Some(spy.clone())).await;

    for path in [
        "/definitely-not-a-route",
        "/imposters/4545/nope",
        "/admin/reload",
    ] {
        let resp = reqwest::get(format!("{}{path}", base(port)))
            .await
            .expect("request");
        assert_eq!(resp.status(), 401, "{path} leaked its existence");
    }
    assert!(spy.calls().is_empty());
}

#[tokio::test]
async fn an_unknown_path_still_404s_for_an_authorized_caller() {
    // `classify` returns None for unmatched paths, so the hook is skipped and the ordinary 404
    // stands. A denying authorizer must not turn 404s into 403s — that would be a behaviour
    // change for a route that reaches no handler.
    let port = 14806;
    let spy = SpyAuthorizer::new(AuthzDecision::Deny { reason: "nope" });
    start_admin(port, Some("s3cret"), Some(spy.clone())).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/definitely-not-a-route", base(port)))
        .header("authorization", "s3cret")
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 404);
    assert!(spy.calls().is_empty());
}

#[tokio::test]
async fn the_hook_receives_the_parsed_route_not_a_raw_path() {
    // The reason the seam exists: an embedder should not have to re-parse admin routes.
    let port = 14807;
    let spy = SpyAuthorizer::new(AuthzDecision::allow());
    start_admin(port, None, Some(spy.clone())).await;

    reqwest::Client::new()
        .post(format!("{}/imposters", base(port)))
        .header("x-rift-scope", "tenant-7")
        .json(&serde_json::json!({"protocol": "http", "port": 14899}))
        .send()
        .await
        .expect("create imposter");

    let create = spy.calls().into_iter().next().expect("hook was consulted");
    assert_eq!(create.action, actions::IMPOSTER_WRITE);
    assert_eq!(create.port, None, "a create names no port yet");
    assert_eq!(
        create.scope.as_deref(),
        Some("tenant-7"),
        "scope is how an authorizer targets a create"
    );

    reqwest::Client::new()
        .put(format!(
            "{}/imposters/14899/scenarios/checkout/state",
            base(port)
        ))
        .json(&serde_json::json!({"state": "Started"}))
        .send()
        .await
        .expect("set scenario state");

    let scenario = spy.calls().pop().expect("second call");
    assert_eq!(scenario.action, actions::IMPOSTER_WRITE);
    assert_eq!(scenario.port, Some(14899));
    assert!(
        scenario
            .params
            .contains(&("scenario".to_string(), "checkout".to_string())),
        "path params must reach the hook, saw: {:?}",
        scenario.params
    );
}

#[tokio::test]
async fn the_credential_is_passed_through_verbatim() {
    // Upstream must not parse or normalise it: an embedder's credential format is not upstream's
    // vocabulary.
    let port = 14808;
    let spy = SpyAuthorizer::new(AuthzDecision::allow());
    start_admin(port, None, Some(spy.clone())).await;

    reqwest::Client::new()
        .get(format!("{}/imposters", base(port)))
        .header("authorization", "Bearer  weird.token  ")
        .send()
        .await
        .expect("request");

    let seen = spy.calls().into_iter().next().expect("hook was consulted");
    assert_eq!(seen.credential.as_deref(), Some("Bearer  weird.token"));
}

#[tokio::test]
async fn gateway_traffic_is_not_authorized() {
    // `/__rift/` is data-plane imposter traffic. It skips the admin api key for the same reason,
    // and authorizing it would force the app under test to carry an admin identity.
    let port = 14809;
    let spy = SpyAuthorizer::new(AuthzDecision::Deny {
        reason: "would break the data plane",
    });
    start_admin(port, None, Some(spy.clone())).await;

    let manager_resp = reqwest::Client::new()
        .post(format!("{}/imposters", base(port)))
        .json(&serde_json::json!({
            "protocol": "http", "port": 14898,
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "gw"}}]}]
        }))
        .send()
        .await;
    // The create itself is denied (it is an admin route), which is fine — what matters is that a
    // gateway request is never handed to the hook at all.
    drop(manager_resp);
    spy.calls.lock().clear();

    let _ = reqwest::get(format!("{}/__rift/14898/anything", base(port))).await;
    assert!(
        spy.calls().is_empty(),
        "gateway traffic must not consult the admin authorizer, saw: {:?}",
        spy.calls()
    );
}

#[tokio::test]
async fn verify_is_not_classified_as_a_write() {
    // A POST that mutates nothing. A read-only principal must still be able to verify.
    let port = 14810;
    let spy = SpyAuthorizer::new(AuthzDecision::allow());
    start_admin(port, None, Some(spy.clone())).await;

    let manager = reqwest::Client::new();
    manager
        .post(format!("{}/imposters", base(port)))
        .json(&imposter_cfg(serde_json::json!({
            "protocol": "http", "port": 14897,
            "stubs": [{"responses": [{"is": {"statusCode": 200}}]}]
        })))
        .send()
        .await
        .expect("create");
    spy.calls.lock().clear();

    let _ = manager
        .post(format!("{}/imposters/14897/verify", base(port)))
        .json(&serde_json::json!({"predicates": [], "atLeast": 0}))
        .send()
        .await;

    let seen = spy.calls().into_iter().next().expect("hook was consulted");
    assert_eq!(seen.action, actions::IMPOSTER_VERIFY);
    assert_ne!(seen.action, actions::IMPOSTER_WRITE);
}

#[tokio::test]
async fn the_cross_imposter_event_stream_is_gated() {
    // `GET /events` is dispatched before the router, so it was invisible to the hook: a Deny-all
    // authorizer could not stop a caller streaming every imposter's recorded requests, headers
    // and bodies. The highest-value read on the admin plane must not be the one it cannot gate.
    let port = 14811;
    let spy = SpyAuthorizer::new(AuthzDecision::Deny {
        reason: "no event access",
    });
    start_admin(port, None, Some(spy.clone())).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/events", base(port)))
        .send()
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        403,
        "the event stream bypassed the authorizer"
    );

    let seen = spy.calls().into_iter().next().expect("hook was consulted");
    assert_eq!(seen.action, actions::EVENTS_READ);
    assert_eq!(
        seen.port, None,
        "the stream is cross-imposter, not port-scoped"
    );
}

#[tokio::test]
async fn the_per_imposter_stream_alias_is_gated_and_scoped() {
    let port = 14812;
    let spy = SpyAuthorizer::new(AuthzDecision::Deny { reason: "nope" });
    start_admin(port, None, Some(spy.clone())).await;

    let resp = reqwest::Client::new()
        .get(format!(
            "{}/imposters/4545/savedRequests/stream",
            base(port)
        ))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 403);

    let seen = spy.calls().into_iter().next().expect("hook was consulted");
    assert_eq!(seen.action, actions::IMPOSTER_READ);
    assert_eq!(seen.port, Some(4545), "the alias is scoped to its port");
}

#[tokio::test]
async fn an_empty_path_segment_cannot_route_past_the_classifier() {
    // Hyper does not normalise `//`, and the router does not filter empty segments. When the
    // classifier did, these dispatched into real handlers the hook had never seen.
    let port = 14813;
    let spy = SpyAuthorizer::new(AuthzDecision::Deny { reason: "denied" });
    start_admin(port, None, Some(spy.clone())).await;

    let client = reqwest::Client::new();
    for (method, path) in [
        ("PUT", "/imposters/4545/scenarios//state"),
        ("GET", "/imposters/4545/spaces/"),
        ("DELETE", "/imposters/4545/spaces/"),
        ("GET", "/imposters/4545/stubs/by-id/"),
    ] {
        let req = client.request(
            method.parse().expect("method"),
            format!("{}{path}", base(port)),
        );
        let status = req.send().await.expect("request").status();
        assert_ne!(
            status, 200,
            "{method} {path} reached a handler despite a Deny-all authorizer"
        );
    }
}

#[tokio::test]
async fn an_empty_space_segment_is_not_authorized_against_the_wrong_space() {
    // The subtler half: `POST /imposters/:port/spaces//stubs` writes into space "" but the old
    // classifier reported space "stubs", so an authorizer scoping by space granted on the wrong
    // subject rather than simply missing the route.
    let port = 14814;
    let spy = SpyAuthorizer::new(AuthzDecision::allow());
    start_admin(port, None, Some(spy.clone())).await;

    let _ = reqwest::Client::new()
        .post(format!("{}/imposters/4545/spaces//stubs", base(port)))
        .json(&serde_json::json!({"responses": [{"is": {"statusCode": 200}}]}))
        .send()
        .await;

    if let Some(seen) = spy.calls().into_iter().next() {
        assert_ne!(
            seen.space.as_deref(),
            Some("stubs"),
            "authorized against the literal path segment instead of the real space"
        );
    }
}

#[tokio::test]
async fn a_destructive_put_requires_delete_not_write() {
    // `PUT /imposters {"imposters":[]}` reconciles the set toward the payload and removes
    // everything. Classifying it as `imposter.write` would let a write-but-not-delete principal
    // wipe the server.
    let port = 14815;
    let spy = SpyAuthorizer::new(AuthzDecision::allow());
    start_admin(port, None, Some(spy.clone())).await;

    let _ = reqwest::Client::new()
        .put(format!("{}/imposters", base(port)))
        .json(&serde_json::json!({"imposters": []}))
        .send()
        .await;

    let seen = spy.calls().into_iter().next().expect("hook was consulted");
    assert_eq!(seen.action, actions::IMPOSTER_DELETE);
}
