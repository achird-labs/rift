//! Issue #1400: every admin JSON response says `application/json; charset=utf-8`, as Mountebank's
//! admin API (Express `res.json`) does — success, handler error, engine error and auth refusal
//! alike. Imposter traffic through the `/__rift/` gateway keeps the bare `application/json` its
//! stub infers, because that is an imposter-served response, not the admin plane.

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;

const ADMIN_JSON: &str = "application/json; charset=utf-8";
const KEY: &str = "s3cret-token";

fn content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn every_admin_json_door_names_the_utf8_charset() {
    let manager = Arc::new(ImposterManager::new());
    let admin = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        manager.clone(),
        Some(KEY.to_string()),
    )
    .bind()
    .await
    .expect("admin binds");
    let base = format!("http://{}", admin.local_addr());
    let client = reqwest::Client::new();
    let admin_get = |path: &str| {
        client
            .get(format!("{base}{path}"))
            .header("authorization", KEY)
    };

    // `POST /imposters` 201: the handler's own `json_response`.
    let created = client
        .post(format!("{base}/imposters"))
        .header("authorization", KEY)
        .json(&serde_json::json!({
            "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "body": { "ok": true } } }] }]
        }))
        .send()
        .await
        .expect("create");
    assert_eq!(created.status(), 201);
    assert_eq!(content_type(&created), ADMIN_JSON, "POST /imposters 201");
    let port = created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("auto-assigned port");

    // `POST /imposters` 400: a body that is not an imposter.
    let refused = client
        .post(format!("{base}/imposters"))
        .header("authorization", KEY)
        .body("not json")
        .send()
        .await
        .expect("refused");
    assert_eq!(refused.status(), 400);
    assert_eq!(content_type(&refused), ADMIN_JSON, "POST /imposters 400");

    for (path, status) in [
        ("/imposters".to_string(), 200),
        (format!("/imposters/{port}"), 200),
        // The paged route builds its own header list (cursor headers).
        (format!("/imposters/{port}/savedRequests?since=0"), 200),
        // An engine `ImposterError` conversion, built by the shared core builder.
        ("/imposters/1/stubs".to_string(), 404),
    ] {
        let response = admin_get(&path).send().await.expect(&path);
        assert_eq!(response.status(), status, "GET {path}");
        assert_eq!(content_type(&response), ADMIN_JSON, "GET {path}");
    }

    // The api-key refusal is a hand-written envelope in the listener.
    let unauthorized = client
        .get(format!("{base}/imposters"))
        .send()
        .await
        .expect("unauthorized");
    assert_eq!(unauthorized.status(), 401);
    assert_eq!(content_type(&unauthorized), ADMIN_JSON, "401");

    // Imposter traffic through the gateway is not the admin plane.
    let served = client
        .get(format!("{base}/__rift/{port}/anything"))
        .send()
        .await
        .expect("gateway");
    assert_eq!(served.status(), 200);
    assert_eq!(content_type(&served), "application/json", "gateway");

    manager.delete_all().await;
    admin.shutdown().await;
}
