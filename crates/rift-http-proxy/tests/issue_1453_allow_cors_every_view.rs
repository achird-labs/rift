//! Issue #1453: since Mountebank 2.9.2 `allowCORS` is printed in every imposter view whenever it
//! is truthy, echoed exactly as configured, and any JSON value is accepted by JavaScript
//! truthiness. Ports are auto-assigned (#1461).

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

async fn start() -> (reqwest::Client, String, Arc<ImposterManager>) {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
        .bind()
        .await
        .expect("admin API binds");
    (
        reqwest::Client::new(),
        format!("http://{}", running.local_addr()),
        manager,
    )
}

/// Creates an imposter with `allowCORS: value` on an auto-assigned port; returns the create
/// response body and the port.
async fn create(client: &reqwest::Client, admin: &str, value: &Value) -> (Value, u16) {
    let created = client
        .post(format!("{admin}/imposters"))
        .json(&json!({"protocol": "http", "allowCORS": value}))
        .send()
        .await
        .expect("POST /imposters");
    let status = created.status().as_u16();
    let body: Value = created.json().await.expect("create body is JSON");
    assert_eq!(status, 201, "allowCORS {value}: {body}");
    let port = body["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .expect("create body carries the port");
    (body, port)
}

async fn get_json(client: &reqwest::Client, url: &str) -> Value {
    let response = client.get(url).send().await.expect("GET");
    assert_eq!(response.status().as_u16(), 200, "{url}");
    response.json().await.expect("JSON")
}

fn list_entry(list: &Value, port: u16) -> Value {
    list["imposters"]
        .as_array()
        .expect("imposters array")
        .iter()
        .find(|i| i["port"] == json!(port))
        .cloned()
        .unwrap_or_else(|| panic!("port {port} listed: {list}"))
}

/// Every view of the imposter: create response, detail, replayable, list, `?list=true`.
async fn views(client: &reqwest::Client, admin: &str, value: &Value) -> Vec<(&'static str, Value)> {
    let (created, port) = create(client, admin, value).await;
    let detail = get_json(client, &format!("{admin}/imposters/{port}")).await;
    let replayable = get_json(client, &format!("{admin}/imposters/{port}?replayable=true")).await;
    let list = get_json(client, &format!("{admin}/imposters")).await;
    let short = get_json(client, &format!("{admin}/imposters?list=true")).await;
    vec![
        ("create", created),
        ("detail", detail),
        ("replayable", replayable),
        ("list", list_entry(&list, port)),
        ("list=true", list_entry(&short, port)),
    ]
}

#[tokio::test]
async fn a_truthy_allow_cors_is_printed_verbatim_in_every_view() {
    let (client, admin, manager) = start().await;
    for (view, printed) in views(&client, &admin, &json!("yes")).await {
        assert_eq!(
            printed.get("allowCORS"),
            Some(&json!("yes")),
            "{view}: {printed}"
        );
    }
    manager.delete_all().await;
}

#[tokio::test]
async fn a_false_allow_cors_is_printed_in_no_view() {
    let (client, admin, manager) = start().await;
    for (view, printed) in views(&client, &admin, &json!(false)).await {
        assert_eq!(printed.get("allowCORS"), None, "{view}: {printed}");
        assert_eq!(printed.get("allowCors"), None, "{view}: {printed}");
    }
    manager.delete_all().await;
}

/// Mirrors `test_cors_options_preflight` (rift-mock-core): `1` is as truthy as `true`.
#[tokio::test]
async fn a_numeric_allow_cors_answers_a_preflight_like_true() {
    let (client, admin, manager) = start().await;
    for value in [json!(true), json!(1)] {
        let (_, port) = create(&client, &admin, &value).await;
        let response = client
            .request(
                reqwest::Method::OPTIONS,
                format!("http://127.0.0.1:{port}/any/path"),
            )
            .header("Origin", "http://example.com")
            .header("Access-Control-Request-Method", "POST")
            .send()
            .await
            .expect("OPTIONS");
        assert_eq!(response.status().as_u16(), 200, "allowCORS {value}");
        for name in [
            "access-control-allow-origin",
            "access-control-allow-headers",
            "access-control-allow-methods",
        ] {
            assert_eq!(
                response.headers().get(name).and_then(|v| v.to_str().ok()),
                Some("*"),
                "allowCORS {value}: {name}"
            );
        }
    }
    manager.delete_all().await;
}
