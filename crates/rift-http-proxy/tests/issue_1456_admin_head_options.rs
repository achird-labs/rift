//! Issue #1456: the admin API answers `HEAD` and `OPTIONS` on every route the way Mountebank 2.9.5
//! (Express 5) does, and an imposter id that is not a port is an absent imposter.
//!
//! Every expectation here was measured against `mb` 2.9.5 first: `HEAD` is the `GET` without its
//! body, `OPTIONS` is `200` with a sorted `Allow`, `text/plain` and the same string as its body, and
//! `/imposters/abc` and `/imposters/70000` answer exactly as `/imposters/4999` does.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rift_http_proxy::admin_api::{ADMIN_ROUTES, AdminApiServer, RouteFamily, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use rift_mock_core::extensions::authz::{AdminAuthorizer, AuthzDecision, AuthzRequest, actions};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

async fn start(api_key: Option<&str>) -> (String, Arc<ImposterManager>, RunningAdminApi) {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        manager.clone(),
        api_key.map(str::to_string),
    )
    .bind()
    .await
    .expect("admin binds");
    (format!("http://{}", running.local_addr()), manager, running)
}

async fn create(manager: &ImposterManager, port: u16) {
    let config = serde_json::json!({
        "port": port, "protocol": "http",
        "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "from the imposter" } }] }]
    });
    manager
        .create_imposter(serde_json::from_value(config).expect("config"))
        .await
        .expect("create imposter");
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().expect("ascii header").to_string())
}

async fn options(client: &reqwest::Client, url: String) -> reqwest::Response {
    client
        .request(reqwest::Method::OPTIONS, url)
        .send()
        .await
        .expect("OPTIONS")
}

#[tokio::test]
async fn head_imposters_is_the_get_without_its_body() {
    let (base, manager, admin) = start(None).await;
    create(&manager, free_port()).await;
    let client = reqwest::Client::new();

    let get = client
        .get(format!("{base}/imposters"))
        .send()
        .await
        .expect("GET");
    let get_headers = get.headers().clone();
    let get_body = get.bytes().await.expect("GET body");
    assert!(!get_body.is_empty());

    let head = client
        .head(format!("{base}/imposters"))
        .send()
        .await
        .expect("HEAD");
    assert_eq!(head.status(), 200);
    let head_headers = head.headers().clone();
    assert_eq!(
        head_headers
            .get("content-length")
            .and_then(|v| v.to_str().ok()),
        Some(get_body.len().to_string().as_str()),
        "HEAD's Content-Length is the GET body's length"
    );
    for (name, value) in &get_headers {
        if name == "date" {
            continue;
        }
        assert_eq!(head_headers.get(name), Some(value), "header {name}");
    }
    assert_eq!(head_headers.len(), get_headers.len());
    assert!(head.bytes().await.expect("HEAD body").is_empty());
    admin.shutdown().await;
}

#[tokio::test]
async fn head_of_an_absent_imposter_is_404() {
    let (base, _manager, admin) = start(None).await;
    let head = reqwest::Client::new()
        .head(format!("{base}/imposters/{}", free_port()))
        .send()
        .await
        .expect("HEAD");
    assert_eq!(head.status(), 404);
    admin.shutdown().await;
}

#[tokio::test]
async fn options_imposters_lists_every_method_as_express_5_does() {
    let (base, _manager, admin) = start(None).await;
    let response = options(&reqwest::Client::new(), format!("{base}/imposters")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "allow").as_deref(),
        Some("DELETE, GET, HEAD, POST, PUT")
    );
    assert_eq!(
        header(&response, "content-type").as_deref(),
        Some("text/plain")
    );
    assert_eq!(
        header(&response, "x-content-type-options").as_deref(),
        Some("nosniff")
    );
    assert_eq!(
        response.text().await.expect("body"),
        "DELETE, GET, HEAD, POST, PUT"
    );
    admin.shutdown().await;
}

#[tokio::test]
async fn options_on_get_only_routes_and_absent_imposters() {
    let (base, _manager, admin) = start(None).await;
    let client = reqwest::Client::new();
    for path in ["/", "/config", "/logs"] {
        let response = options(&client, format!("{base}{path}")).await;
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(
            header(&response, "allow").as_deref(),
            Some("GET, HEAD"),
            "{path}"
        );
    }
    // An absent imposter still names a route, so Express answers its methods.
    for id in ["4999", "abc", "70000"] {
        let response = options(&client, format!("{base}/imposters/{id}")).await;
        assert_eq!(response.status(), 200, "{id}");
        assert_eq!(
            header(&response, "allow").as_deref(),
            Some("DELETE, GET, HEAD"),
            "{id}"
        );
    }
    let reload = options(&client, format!("{base}/admin/reload")).await;
    assert_eq!(header(&reload, "allow").as_deref(), Some("POST"));
    admin.shutdown().await;
}

#[tokio::test]
async fn options_and_head_on_an_unknown_path_are_404() {
    let (base, _manager, admin) = start(None).await;
    let client = reqwest::Client::new();
    assert_eq!(options(&client, format!("{base}/nope")).await.status(), 404);
    assert_eq!(
        client
            .head(format!("{base}/nope"))
            .send()
            .await
            .expect("HEAD")
            .status(),
        404
    );
    // A route with no GET has no HEAD either: Express answers 404.
    let head_reload = client
        .head(format!("{base}/admin/reload"))
        .send()
        .await
        .expect("HEAD");
    assert_eq!(head_reload.status(), 404);
    admin.shutdown().await;
}

/// Rift's admin API has no CORS layer, so a browser preflight gets the same answer as any OPTIONS
/// (Mountebank 2.9.5 without `--allowCORS` measured the same).
#[tokio::test]
async fn a_preflight_gets_the_same_options_answer() {
    let (base, _manager, admin) = start(None).await;
    let response = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{base}/imposters"))
        .header("Origin", "http://example.test")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .expect("OPTIONS");
    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "allow").as_deref(),
        Some("DELETE, GET, HEAD, POST, PUT")
    );
    assert_eq!(
        response.text().await.expect("body"),
        "DELETE, GET, HEAD, POST, PUT"
    );
    admin.shutdown().await;
}

#[tokio::test]
async fn a_non_numeric_imposter_id_is_an_absent_imposter() {
    let (base, _manager, admin) = start(None).await;
    let client = reqwest::Client::new();
    let absent = free_port();

    let numeric = client
        .get(format!("{base}/imposters/{absent}"))
        .send()
        .await
        .expect("GET");
    assert_eq!(numeric.status(), 404);
    let numeric: serde_json::Value = numeric.json().await.expect("json");

    for id in ["abc", "70000"] {
        let get = client
            .get(format!("{base}/imposters/{id}"))
            .send()
            .await
            .expect("GET");
        assert_eq!(get.status(), 404, "GET /imposters/{id}");
        let body: serde_json::Value = get.json().await.expect("json");
        assert_eq!(body["errors"][0]["code"], numeric["errors"][0]["code"]);
        assert_eq!(body["errors"][0]["type"], numeric["errors"][0]["type"]);

        let delete = client
            .delete(format!("{base}/imposters/{id}"))
            .send()
            .await
            .expect("DELETE");
        assert_eq!(delete.status(), 200, "DELETE /imposters/{id}");
        assert_eq!(
            delete.json::<serde_json::Value>().await.expect("json"),
            serde_json::json!({})
        );

        let head = client
            .head(format!("{base}/imposters/{id}"))
            .send()
            .await
            .expect("HEAD");
        assert_eq!(head.status(), 404, "HEAD /imposters/{id}");
    }
    admin.shutdown().await;
}

/// Every per-imposter route answers a non-numeric or out-of-range id exactly as it answers a port
/// with no imposter on it — the status, the error code and type.
#[tokio::test]
async fn every_per_imposter_route_treats_a_bad_id_as_an_absent_port() {
    let (base, _manager, admin) = start(None).await;
    let client = reqwest::Client::new();
    let absent = free_port().to_string();
    let body = |path: &str| -> &'static str {
        if path.ends_with("/spaces/{space}/stubs") {
            r#"{"responses":[{"is":{}}]}"#
        } else if path.ends_with("/stubs") {
            r#"{"stubs":[{"responses":[{"is":{}}]}],"stub":{"responses":[{"is":{}}]}}"#
        } else if path.ends_with("/state") {
            r#"{"state":"Started"}"#
        } else {
            r#"{"responses":[{"is":{}}]}"#
        }
    };

    for entry in ADMIN_ROUTES
        .iter()
        .filter(|r| r.family == RouteFamily::Imposters && r.path.starts_with("/imposters/{port}"))
    {
        let sample = |id: &str| {
            entry
                .path
                .replace("{port}", id)
                .replace("{stubIndex}", "0")
                .replace("{stubId}", "a")
                .replace("{scenario}", "order")
                .replace("{space}", "f1")
        };
        let method = reqwest::Method::from_bytes(entry.method.as_str().as_bytes()).expect("method");
        let send = |id: &str| {
            client
                .request(method.clone(), format!("{base}{}", sample(id)))
                .header("Content-Type", "application/json")
                .body(body(entry.path))
                .send()
        };
        let expected = send(&absent).await.expect("send");
        let expected_status = expected.status();
        let expected_body = expected.text().await.expect("body");
        let expected_json: serde_json::Value =
            serde_json::from_str(&expected_body).unwrap_or_default();
        for id in ["abc", "70000"] {
            let actual = send(id).await.expect("send");
            assert_eq!(
                actual.status(),
                expected_status,
                "{} {} with id {id}",
                entry.method,
                entry.path
            );
            let actual: serde_json::Value =
                serde_json::from_str(&actual.text().await.expect("body")).unwrap_or_default();
            assert_eq!(
                actual["errors"][0]["code"], expected_json["errors"][0]["code"],
                "{} {} with id {id}",
                entry.method, entry.path
            );
            assert_eq!(
                actual["errors"][0]["type"], expected_json["errors"][0]["type"],
                "{} {} with id {id}",
                entry.method, entry.path
            );
        }
    }
    admin.shutdown().await;
}

#[tokio::test]
async fn head_and_options_go_through_the_api_key_gate() {
    let (base, _manager, admin) = start(Some("secret")).await;
    let client = reqwest::Client::new();

    let get = client
        .get(format!("{base}/imposters"))
        .send()
        .await
        .expect("GET");
    assert_eq!(get.status(), 401);
    let head = client
        .head(format!("{base}/imposters"))
        .send()
        .await
        .expect("HEAD");
    assert_eq!(head.status(), 401, "HEAD is refused like GET");
    let opts = options(&client, format!("{base}/imposters")).await;
    assert_eq!(opts.status(), 401, "OPTIONS is refused like GET");
    // Even for a path that does not exist: an unauthenticated caller learns nothing about routes.
    assert_eq!(options(&client, format!("{base}/nope")).await.status(), 401);

    let authed = client
        .head(format!("{base}/imposters"))
        .header("Authorization", "secret")
        .send()
        .await
        .expect("HEAD");
    assert_eq!(authed.status(), 200);
    let authed = client
        .request(reqwest::Method::OPTIONS, format!("{base}/imposters"))
        .header("Authorization", "secret")
        .send()
        .await
        .expect("OPTIONS");
    assert_eq!(authed.status(), 200);
    admin.shutdown().await;
}

struct Spy {
    seen: Mutex<Vec<(String, String)>>,
    deny: bool,
}

impl AdminAuthorizer for Spy {
    fn authorize(&self, req: AuthzRequest<'_>) -> AuthzDecision {
        self.seen
            .lock()
            .push((req.action.to_string(), format!("{:?}", req.port)));
        if self.deny {
            AuthzDecision::Deny { reason: "no" }
        } else {
            AuthzDecision::Allow { principal: None }
        }
    }
}

#[tokio::test]
async fn head_and_options_are_authorized_as_reads() {
    for deny in [true, false] {
        let spy = Arc::new(Spy {
            seen: Mutex::new(Vec::new()),
            deny,
        });
        let manager = Arc::new(ImposterManager::new());
        let admin = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None)
            .with_admin_authorizer(spy.clone())
            .bind()
            .await
            .expect("admin binds");
        let base = format!("http://{}", admin.local_addr());
        let client = reqwest::Client::new();

        let head = client
            .head(format!("{base}/imposters/4545"))
            .send()
            .await
            .expect("HEAD");
        let opts = options(&client, format!("{base}/imposters/4545")).await;
        if deny {
            assert_eq!(head.status(), 403);
            assert_eq!(opts.status(), 403);
        } else {
            assert_eq!(head.status(), 404);
            assert_eq!(opts.status(), 200);
        }
        assert_eq!(
            *spy.seen.lock(),
            vec![
                (actions::IMPOSTER_READ.to_string(), "Some(4545)".to_string()),
                (actions::IMPOSTER_READ.to_string(), "Some(4545)".to_string()),
            ]
        );
        admin.shutdown().await;
    }
}

/// `HEAD` on an event stream answers the stream's headers and ends: it never opens a stream that
/// would hold the connection.
#[tokio::test]
async fn head_on_an_event_stream_answers_without_streaming() {
    let (base, manager, admin) = start(None).await;
    let port = free_port();
    create(&manager, port).await;
    let client = reqwest::Client::new();

    for path in [
        "/events".to_string(),
        format!("/imposters/{port}/savedRequests/stream"),
    ] {
        let head = tokio::time::timeout(
            Duration::from_secs(5),
            client.head(format!("{base}{path}")).send(),
        )
        .await
        .unwrap_or_else(|_| panic!("HEAD {path} never answered"))
        .expect("HEAD");
        assert_eq!(head.status(), 200, "{path}");
        assert_eq!(
            header(&head, "content-type").as_deref(),
            Some("text/event-stream"),
            "{path}"
        );
        let body = tokio::time::timeout(Duration::from_secs(5), head.bytes())
            .await
            .unwrap_or_else(|_| panic!("HEAD {path} body never ended"))
            .expect("body");
        assert!(body.is_empty(), "{path}");

        let opts = options(&client, format!("{base}{path}")).await;
        assert_eq!(opts.status(), 200, "{path}");
        assert_eq!(
            header(&opts, "allow").as_deref(),
            Some("GET, HEAD"),
            "{path}"
        );
    }

    // The stream's refusal is HEAD's too.
    let absent = client
        .head(format!(
            "{base}/imposters/{}/savedRequests/stream",
            free_port()
        ))
        .send()
        .await
        .expect("HEAD");
    assert_eq!(absent.status(), 404);
    admin.shutdown().await;
}

/// `/__rift/:port/…` is data-plane traffic to an imposter: its `HEAD` and `OPTIONS` belong to the
/// imposter's stubs, not to the admin router's automatic answers.
#[tokio::test]
async fn the_gateway_passes_head_and_options_to_the_imposter() {
    let (base, manager, admin) = start(None).await;
    let port = free_port();
    create(&manager, port).await;
    let response = options(
        &reqwest::Client::new(),
        format!("{base}/__rift/{port}/imposters"),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "allow"), None);
    assert_eq!(response.text().await.expect("body"), "from the imposter");
    admin.shutdown().await;
}

/// An empty id is no imposter route: the unknown-route 404 for every method, and a `DELETE` that
/// deletes nothing. Mountebank reads `/imposters/` as the collection; that parity is #1481.
#[tokio::test]
async fn an_empty_imposter_id_is_the_unknown_route_404() {
    let (base, manager, admin) = start(None).await;
    let port = free_port();
    create(&manager, port).await;
    let client = reqwest::Client::new();

    for path in ["/imposters/", "/imposters//stubs"] {
        for method in ["GET", "DELETE", "OPTIONS", "HEAD", "PUT", "POST"] {
            let response = client
                .request(
                    reqwest::Method::from_bytes(method.as_bytes()).expect("method"),
                    format!("{base}{path}"),
                )
                .send()
                .await
                .expect("send");
            assert_eq!(response.status(), 404, "{method} {path}");
            assert_eq!(header(&response, "allow"), None, "{method} {path}");
            if method != "HEAD" {
                let body: serde_json::Value = response.json().await.expect("json");
                assert_eq!(body["errors"][0]["message"], "Not Found", "{method} {path}");
            }
        }
    }
    let still_there = client
        .get(format!("{base}/imposters/{port}"))
        .send()
        .await
        .expect("GET");
    assert_eq!(
        still_there.status(),
        200,
        "DELETE /imposters/ deleted nothing"
    );
    admin.shutdown().await;
}
