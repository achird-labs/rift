//! Issue #1484: admin route names are case-insensitive, as Mountebank 2.9.5's Express 5 router
//! compiles every route without `caseSensitive`. Only the route's own literal segments fold; the
//! values in it (imposter ids, stub ids, space and scenario names, flow-state keys) are data and
//! stay case-sensitive. The `/__rift/` gateway prefix is Rift's own and stays exact-case.
//!
//! Measured against `mb` 2.9.5 first: `GET /IMPOSTERS`, `GET /Imposters/`, `GET /CONFIG`,
//! `GET /LOGS`, `GET /METRICS`, `GET /Imposters/:id`, `OPTIONS /Imposters/:id`,
//! `OPTIONS /imposters/:id/STUBS`, `POST /imposters/:id/STUBS`, `PUT /imposters/:id/Stubs/0`,
//! `DELETE /imposters/:id/SAVEDREQUESTS` and `DELETE /IMPOSTERS/:id/savedproxyresponses` all answer
//! as their table spelling does.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use rift_mock_core::extensions::authz::{AdminAuthorizer, AuthzDecision, AuthzRequest, actions};

async fn start() -> (String, RunningAdminApi) {
    start_with(AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    ))
    .await
}

async fn start_with(server: AdminApiServer) -> (String, RunningAdminApi) {
    let running = server.bind().await.expect("admin binds");
    (format!("http://{}", running.local_addr()), running)
}

/// Creates an imposter on a port the manager assigns and returns that port.
async fn create(client: &reqwest::Client, base: &str) -> u16 {
    create_from(
        client,
        base,
        serde_json::json!({
            "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }]
        }),
    )
    .await
}

async fn create_from(client: &reqwest::Client, base: &str, config: serde_json::Value) -> u16 {
    let response = client
        .post(format!("{base}/imposters"))
        .json(&config)
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(response.status(), 201, "create imposter");
    let body: serde_json::Value = response.json().await.expect("json");
    let port = body["port"].as_u64().expect("assigned port");
    u16::try_from(port).expect("port fits u16")
}

async fn send(
    client: &reqwest::Client,
    method: &str,
    url: String,
    body: Option<serde_json::Value>,
) -> (u16, Option<String>, String) {
    let mut request = client.request(
        reqwest::Method::from_bytes(method.as_bytes()).expect("method"),
        &url,
    );
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = tokio::time::timeout(Duration::from_secs(5), request.send())
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

async fn get(client: &reqwest::Client, url: String) -> (u16, Option<String>, String) {
    send(client, "GET", url, None).await
}

#[tokio::test]
async fn a_mixed_case_route_answers_exactly_as_its_table_spelling() {
    let (base, admin) = start().await;
    let client = reqwest::Client::new();
    let port = create(&client, &base).await;

    let read = &["GET", "HEAD", "OPTIONS"][..];
    let cases: Vec<(String, String, &[&str])> = vec![
        ("/IMPOSTERS".into(), "/imposters".into(), read),
        ("/Imposters/".into(), "/imposters".into(), read),
        (
            "/IMPOSTERS?replayable=true".into(),
            "/imposters?replayable=true".into(),
            read,
        ),
        (
            format!("/Imposters/{port}"),
            format!("/imposters/{port}"),
            read,
        ),
        (
            format!("/IMPOSTERS/{port}/STUBS"),
            format!("/imposters/{port}/stubs"),
            read,
        ),
        (
            format!("/Imposters/{port}/Stubs/0"),
            format!("/imposters/{port}/stubs/0"),
            read,
        ),
        (
            format!("/imposters/{port}/SavedRequests"),
            format!("/imposters/{port}/savedRequests"),
            read,
        ),
        (
            format!("/imposters/{port}/savedrequests"),
            format!("/imposters/{port}/savedRequests"),
            read,
        ),
        (
            format!("/imposters/{port}/REQUESTS"),
            format!("/imposters/{port}/requests"),
            read,
        ),
        // The SSE alias: `GET` would hold the connection open, so only `HEAD` and `OPTIONS`.
        (
            format!("/imposters/{port}/SavedRequests/Stream"),
            format!("/imposters/{port}/savedRequests/stream"),
            &["HEAD", "OPTIONS"][..],
        ),
        ("/EVENTS".into(), "/events".into(), &["HEAD", "OPTIONS"][..]),
        ("/CONFIG".into(), "/config".into(), read),
        ("/Health".into(), "/health".into(), read),
        ("/LOGS".into(), "/logs".into(), &["OPTIONS"][..]),
        ("/METRICS".into(), "/metrics".into(), &["OPTIONS"][..]),
        (
            format!("/IMPOSTERS/{port}/SCENARIOS"),
            format!("/imposters/{port}/scenarios"),
            read,
        ),
        (
            format!("/imposters/{port}/SPACES/f1/STUBS"),
            format!("/imposters/{port}/spaces/f1/stubs"),
            read,
        ),
        (
            format!("/Admin/Imposters/{port}/Flow-State/f1/k"),
            format!("/admin/imposters/{port}/flow-state/f1/k"),
            read,
        ),
        (
            format!("/IMPOSTERS/{port}/Verify"),
            format!("/imposters/{port}/verify"),
            &["OPTIONS"][..],
        ),
        (
            "/ADMIN/RELOAD".into(),
            "/admin/reload".into(),
            &["OPTIONS"][..],
        ),
        // Unknown either way: the folding must not turn a 404 into a route.
        (
            format!("/IMPOSTERS/{port}/NOPE"),
            format!("/imposters/{port}/nope"),
            read,
        ),
    ];

    for (mixed, plain, methods) in &cases {
        for method in *methods {
            let expected = send(&client, method, format!("{base}{plain}"), None).await;
            let actual = send(&client, method, format!("{base}{mixed}"), None).await;
            assert_eq!(
                actual, expected,
                "{method} {mixed} answers as {method} {plain}"
            );
        }
    }
    for path in ["/LOGS", "/METRICS", "/Logs/"] {
        for method in ["GET", "HEAD"] {
            assert_eq!(
                send(&client, method, format!("{base}{path}"), None).await.0,
                200,
                "{method} {path}"
            );
        }
    }
    admin.shutdown().await;
}

#[tokio::test]
async fn mixed_case_mutations_mutate() {
    let (base, admin) = start().await;
    let client = reqwest::Client::new();
    let port = create(&client, &base).await;

    let stub = serde_json::json!({ "stub": { "responses": [{ "is": { "body": "second" } }] } });
    let (status, _, _) = send(
        &client,
        "POST",
        format!("{base}/imposters/{port}/STUBS"),
        Some(stub),
    )
    .await;
    assert_eq!(status, 200, "POST /imposters/{port}/STUBS");
    let (_, _, body) = get(&client, format!("{base}/imposters/{port}/stubs")).await;
    let stubs: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(stubs["stubs"].as_array().map(Vec::len), Some(2), "{body}");

    let (status, _, _) = send(
        &client,
        "DELETE",
        format!("{base}/Imposters/{port}/Stubs/1"),
        None,
    )
    .await;
    assert_eq!(status, 200, "DELETE /Imposters/{port}/Stubs/1");

    let (status, _, _) = send(&client, "DELETE", format!("{base}/IMPOSTERS/{port}"), None).await;
    assert_eq!(status, 200, "DELETE /IMPOSTERS/{port}");
    let (status, _, _) = get(&client, format!("{base}/imposters/{port}")).await;
    assert_eq!(status, 404, "DELETE /IMPOSTERS/{port} deleted the imposter");

    create(&client, &base).await;
    create(&client, &base).await;
    let (status, _, body) = send(&client, "DELETE", format!("{base}/IMPOSTERS"), None).await;
    assert_eq!(status, 200, "DELETE /IMPOSTERS");
    let deleted: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        deleted["imposters"].as_array().map(Vec::len),
        Some(2),
        "{body}"
    );
    admin.shutdown().await;
}

/// The values in a route are data: folding them would name a different resource.
#[tokio::test]
async fn space_names_and_flow_state_keys_stay_case_sensitive() {
    let (base, admin) = start().await;
    let client = reqwest::Client::new();
    let port = create_from(
        &client,
        &base,
        serde_json::json!({
            "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }]
        }),
    )
    .await;

    let stub = serde_json::json!({ "responses": [{ "is": { "body": "in Alpha" } }] });
    let (status, _, _) = send(
        &client,
        "POST",
        format!("{base}/IMPOSTERS/{port}/SPACES/Alpha/STUBS"),
        Some(stub),
    )
    .await;
    assert_eq!(status, 201, "POST into space Alpha");

    let stubs_of = |body: &str| -> usize {
        let value: serde_json::Value = serde_json::from_str(body).expect("json");
        value["stubs"].as_array().map_or(0, Vec::len)
    };
    let (_, _, alpha) = get(
        &client,
        format!("{base}/imposters/{port}/spaces/Alpha/stubs"),
    )
    .await;
    assert_eq!(stubs_of(&alpha), 1, "space Alpha holds the stub: {alpha}");
    for path in [
        format!("/imposters/{port}/spaces/alpha/stubs"),
        format!("/Imposters/{port}/Spaces/alpha/Stubs"),
        format!("/imposters/{port}/spaces/ALPHA/stubs"),
    ] {
        let (status, _, body) = get(&client, format!("{base}{path}")).await;
        assert_eq!(status, 200, "GET {path}");
        assert_eq!(
            stubs_of(&body),
            0,
            "GET {path} is another, empty space: {body}"
        );
    }

    let (status, _, _) = send(
        &client,
        "PUT",
        format!("{base}/ADMIN/IMPOSTERS/{port}/FLOW-STATE/Flow/Key"),
        Some(serde_json::json!({ "value": 7 })),
    )
    .await;
    assert_eq!(status, 200, "PUT flow-state Flow/Key");
    let (status, _, body) = get(
        &client,
        format!("{base}/admin/imposters/{port}/flow-state/Flow/Key"),
    )
    .await;
    assert_eq!(status, 200, "Flow/Key was stored: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        value,
        serde_json::json!({ "flowId": "Flow", "key": "Key", "value": 7 })
    );
    for path in [
        format!("/admin/imposters/{port}/flow-state/Flow/key"),
        format!("/Admin/Imposters/{port}/Flow-State/flow/Key"),
    ] {
        let (status, _, body) = get(&client, format!("{base}{path}")).await;
        assert_eq!(status, 404, "GET {path} names another key: {body}");
    }
    admin.shutdown().await;
}

/// `/__rift/` is Rift's own prefix, not a Mountebank route, so it is not folded: `/__RIFT/…` is no
/// gateway and no admin route.
#[tokio::test]
async fn the_gateway_prefix_is_exact_case() {
    let (base, admin) = start().await;
    let client = reqwest::Client::new();
    let port = create(&client, &base).await;

    let (status, _, body) = get(&client, format!("{base}/__rift/{port}/orders")).await;
    assert_eq!((status, body.as_str()), (200, "ok"), "the gateway serves");
    for prefix in ["__RIFT", "__Rift"] {
        for method in ["GET", "OPTIONS", "DELETE"] {
            let (status, allow, _) = send(
                &client,
                method,
                format!("{base}/{prefix}/{port}/orders"),
                None,
            )
            .await;
            assert_eq!(
                (status, allow),
                (404, None),
                "{method} /{prefix}/{port}/orders"
            );
        }
    }
    admin.shutdown().await;
}

struct Spy {
    seen: Mutex<Vec<(String, Option<u16>)>>,
    deny_all: bool,
}

impl AdminAuthorizer for Spy {
    fn authorize(&self, req: AuthzRequest<'_>) -> AuthzDecision {
        self.seen.lock().push((req.action.to_string(), req.port));
        if self.deny_all {
            AuthzDecision::Deny { reason: "no" }
        } else {
            AuthzDecision::Allow { principal: None }
        }
    }
}

/// The security half: `classify` answering `None` skips the authorizer, so a mixed-case delete
/// that reached its handler unclassified would delete with no authorization.
#[tokio::test]
async fn a_mixed_case_delete_is_authorized_as_a_delete() {
    for deny_all in [false, true] {
        let manager = Arc::new(ImposterManager::new());
        let port = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "http",
                    "stubs": [{ "responses": [{ "is": { "body": "ok" } }] }]
                }))
                .expect("config"),
            )
            .await
            .expect("create imposter");
        let spy = Arc::new(Spy {
            seen: Mutex::new(Vec::new()),
            deny_all,
        });
        let (base, admin) = start_with(
            AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
                .with_admin_authorizer(spy.clone()),
        )
        .await;
        let client = reqwest::Client::new();

        let (one, _, _) = send(&client, "DELETE", format!("{base}/IMPOSTERS/{port}"), None).await;
        let (all, _, _) = send(&client, "DELETE", format!("{base}/IMPOSTERS"), None).await;
        let (replace, _, _) = send(
            &client,
            "PUT",
            format!("{base}/Imposters"),
            Some(serde_json::json!({ "imposters": [] })),
        )
        .await;
        assert_eq!(
            *spy.seen.lock(),
            vec![
                (actions::IMPOSTER_DELETE.to_string(), Some(port)),
                (actions::IMPOSTER_DELETE.to_string(), None),
                (actions::IMPOSTER_DELETE.to_string(), None),
            ],
            "deny_all={deny_all}"
        );
        if deny_all {
            assert_eq!(one, 403, "DELETE /IMPOSTERS/{port} is refused");
            assert_eq!(all, 403, "DELETE /IMPOSTERS is refused");
            assert_eq!(replace, 403, "PUT /Imposters is refused");
            assert!(
                manager.get_imposter(port).is_ok(),
                "a refused delete deleted nothing"
            );
        } else {
            assert_eq!((one, all, replace), (200, 200, 200));
            assert!(manager.get_imposter(port).is_err(), "the delete deleted");
        }
        admin.shutdown().await;
    }
}
