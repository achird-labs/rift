//! Issue #1481: a trailing `/` on an admin path is ignored, as Mountebank 2.9.5's Express 5 router
//! ignores it (it never sets `strict`). Exactly one `/` is dropped and repeated slashes are never
//! collapsed, so `/imposters//` and `/config//` stay unknown routes.
//!
//! Measured against `mb` 2.9.5 first: `/imposters/`, `/imposters/:id/`, `/config/`, `/logs/` and the
//! per-imposter sub-routes with a trailing `/` answer as without it; `//` answers as `/`; and
//! `/imposters//`, `/imposters/:id//`, `//imposters/:id` and `/config//` are `404` on every method.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use rift_mock_core::extensions::authz::{AdminAuthorizer, AuthzDecision, AuthzRequest, actions};

async fn start() -> (String, RunningAdminApi) {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None)
        .bind()
        .await
        .expect("admin binds");
    (format!("http://{}", running.local_addr()), running)
}

/// Creates an imposter on a port the manager assigns and returns that port.
async fn create(client: &reqwest::Client, base: &str, stubs: serde_json::Value) -> u16 {
    let response = client
        .post(format!("{base}/imposters"))
        .json(&serde_json::json!({ "protocol": "http", "recordRequests": true, "stubs": stubs }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(response.status(), 201, "create imposter");
    let body: serde_json::Value = response.json().await.expect("json");
    let port = body["port"].as_u64().expect("assigned port");
    u16::try_from(port).expect("port fits u16")
}

fn ok_stub() -> serde_json::Value {
    serde_json::json!([{ "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }])
}

async fn send(
    client: &reqwest::Client,
    method: &str,
    url: String,
) -> (u16, Option<String>, String) {
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client
            .request(
                reqwest::Method::from_bytes(method.as_bytes()).expect("method"),
                &url,
            )
            .send(),
    )
    .await
    .unwrap_or_else(|_| panic!("{method} {url} never answered"))
    .expect("send");
    let status = response.status().as_u16();
    let allow = response
        .headers()
        .get("allow")
        .map(|v| v.to_str().expect("ascii").to_string());
    let body = tokio::time::timeout(Duration::from_secs(5), response.text())
        .await
        .unwrap_or_else(|_| panic!("{method} {url} body never ended"))
        .expect("body");
    (status, allow, body)
}

#[tokio::test]
async fn a_trailing_slash_answers_exactly_as_the_path_without_it() {
    let (base, admin) = start().await;
    let client = reqwest::Client::new();
    let port = create(&client, &base, ok_stub()).await;

    // (path with the slash, the same path without it, methods to compare)
    let read = &["GET", "HEAD", "OPTIONS"][..];
    let cases: Vec<(String, String, &[&str])> = vec![
        ("//".into(), "/".into(), read),
        ("/imposters/".into(), "/imposters".into(), read),
        (
            "/imposters/?replayable=true".into(),
            "/imposters?replayable=true".into(),
            read,
        ),
        (
            format!("/imposters/{port}/"),
            format!("/imposters/{port}"),
            read,
        ),
        (
            format!("/imposters/{port}/?replayable=true"),
            format!("/imposters/{port}?replayable=true"),
            read,
        ),
        (
            format!("/imposters/{port}/stubs/"),
            format!("/imposters/{port}/stubs"),
            read,
        ),
        (
            format!("/imposters/{port}/stubs/0/"),
            format!("/imposters/{port}/stubs/0"),
            read,
        ),
        (
            format!("/imposters/{port}/savedRequests/"),
            format!("/imposters/{port}/savedRequests"),
            read,
        ),
        // The SSE alias: `GET` would hold the connection open, so only `HEAD` and `OPTIONS`.
        (
            format!("/imposters/{port}/savedRequests/stream/"),
            format!("/imposters/{port}/savedRequests/stream"),
            &["HEAD", "OPTIONS"][..],
        ),
        (
            "/events/".into(),
            "/events".into(),
            &["HEAD", "OPTIONS"][..],
        ),
        ("/config/".into(), "/config".into(), read),
        ("/logs/".into(), "/logs".into(), &["OPTIONS"][..]),
        ("/health/".into(), "/health".into(), read),
        ("/metrics/".into(), "/metrics".into(), &["OPTIONS"][..]),
        // A Rift-only route.
        (
            format!("/imposters/{port}/scenarios/"),
            format!("/imposters/{port}/scenarios"),
            read,
        ),
        (
            "/admin/reload/".into(),
            "/admin/reload".into(),
            &["OPTIONS"][..],
        ),
    ];

    for (slashed, plain, methods) in &cases {
        for method in *methods {
            let expected = send(&client, method, format!("{base}{plain}")).await;
            let actual = send(&client, method, format!("{base}{slashed}")).await;
            assert_eq!(
                actual, expected,
                "{method} {slashed} answers as {method} {plain}"
            );
        }
    }
    // `/logs` and `/metrics` change between calls; their status is what the slash must not alter.
    for path in ["/logs/", "/metrics/"] {
        assert_eq!(
            send(&client, "GET", format!("{base}{path}")).await.0,
            200,
            "GET {path}"
        );
        assert_eq!(
            send(&client, "HEAD", format!("{base}{path}")).await.0,
            200,
            "HEAD {path}"
        );
    }
    admin.shutdown().await;
}

#[tokio::test]
async fn delete_with_a_trailing_slash_deletes() {
    let (base, admin) = start().await;
    let client = reqwest::Client::new();

    let one = create(&client, &base, ok_stub()).await;
    let (status, _, _) = send(&client, "DELETE", format!("{base}/imposters/{one}/")).await;
    assert_eq!(status, 200, "DELETE /imposters/{one}/");
    let (status, _, _) = send(&client, "GET", format!("{base}/imposters/{one}")).await;
    assert_eq!(status, 404, "DELETE /imposters/{one}/ deleted the imposter");

    create(&client, &base, ok_stub()).await;
    create(&client, &base, ok_stub()).await;
    let (status, _, body) = send(&client, "DELETE", format!("{base}/imposters/")).await;
    assert_eq!(status, 200, "DELETE /imposters/");
    let deleted: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        deleted["imposters"].as_array().map(Vec::len),
        Some(2),
        "DELETE /imposters/ answers the deleted set: {body}"
    );
    let (_, _, listing) = send(&client, "GET", format!("{base}/imposters")).await;
    let listing: serde_json::Value = serde_json::from_str(&listing).expect("json");
    assert_eq!(listing["imposters"], serde_json::json!([]));
    admin.shutdown().await;
}

#[tokio::test]
async fn only_one_slash_is_dropped() {
    let (base, admin) = start().await;
    let client = reqwest::Client::new();
    let port = create(&client, &base, ok_stub()).await;

    for path in [
        "/imposters//".to_string(),
        format!("/imposters/{port}//"),
        format!("//imposters/{port}"),
        format!("/imposters/{port}/stubs//"),
        "/config//".to_string(),
        "/imposters//stubs".to_string(),
    ] {
        for method in ["GET", "HEAD", "OPTIONS", "DELETE"] {
            let (status, allow, _) = send(&client, method, format!("{base}{path}")).await;
            assert_eq!(status, 404, "{method} {path}");
            assert_eq!(allow, None, "{method} {path}");
        }
    }
    let (status, _, _) = send(&client, "GET", format!("{base}/imposters/{port}")).await;
    assert_eq!(status, 200, "no 404 path deleted the imposter");
    admin.shutdown().await;
}

struct Spy {
    seen: Mutex<Vec<(String, Option<u16>)>>,
    deny_deletes: bool,
}

impl AdminAuthorizer for Spy {
    fn authorize(&self, req: AuthzRequest<'_>) -> AuthzDecision {
        self.seen.lock().push((req.action.to_string(), req.port));
        if self.deny_deletes && req.action == actions::IMPOSTER_DELETE {
            AuthzDecision::Deny { reason: "no" }
        } else {
            AuthzDecision::Allow { principal: None }
        }
    }
}

/// The security half: the authorizer is skipped for a path `classify` does not recognise, so a
/// trailing slash that reached a handler unclassified would be a delete with no authorization.
#[tokio::test]
async fn a_trailing_slash_delete_is_authorized_as_a_delete() {
    for deny_deletes in [true, false] {
        let spy = Arc::new(Spy {
            seen: Mutex::new(Vec::new()),
            deny_deletes,
        });
        let manager = Arc::new(ImposterManager::new());
        let admin = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None)
            .with_admin_authorizer(spy.clone())
            .bind()
            .await
            .expect("admin binds");
        let base = format!("http://{}", admin.local_addr());
        let client = reqwest::Client::new();
        let port = create(&client, &base, ok_stub()).await;
        spy.seen.lock().clear();

        let (one, _, _) = send(&client, "DELETE", format!("{base}/imposters/{port}/")).await;
        let (all, _, _) = send(&client, "DELETE", format!("{base}/imposters/")).await;
        assert_eq!(
            *spy.seen.lock(),
            vec![
                (actions::IMPOSTER_DELETE.to_string(), Some(port)),
                (actions::IMPOSTER_DELETE.to_string(), None),
            ],
            "deny_deletes={deny_deletes}"
        );
        let (still_there, _, _) = send(&client, "GET", format!("{base}/imposters/{port}")).await;
        if deny_deletes {
            assert_eq!(one, 403, "DELETE /imposters/{port}/ is refused");
            assert_eq!(all, 403, "DELETE /imposters/ is refused");
            assert_eq!(still_there, 200, "a refused delete deleted nothing");
        } else {
            assert_eq!(one, 200);
            assert_eq!(all, 200);
            assert_eq!(still_there, 404);
        }
        admin.shutdown().await;
    }
}

/// `/__rift/:port/…` is the imposter's own path: its trailing slash is data, never dropped.
#[tokio::test]
async fn the_gateway_keeps_its_trailing_slash() {
    let (base, admin) = start().await;
    let client = reqwest::Client::new();
    let port = create(
        &client,
        &base,
        serde_json::json!([
            {
                "predicates": [{ "equals": { "path": "/foo/" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "with slash" } }]
            },
            {
                "predicates": [{ "equals": { "path": "/foo" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "without slash" } }]
            }
        ]),
    )
    .await;

    let (status, _, body) = send(&client, "GET", format!("{base}/__rift/{port}/foo/")).await;
    assert_eq!((status, body.as_str()), (200, "with slash"));
    let (status, _, body) = send(&client, "GET", format!("{base}/__rift/{port}/foo")).await;
    assert_eq!((status, body.as_str()), (200, "without slash"));
    admin.shutdown().await;
}
