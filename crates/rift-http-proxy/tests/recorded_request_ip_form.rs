//! Issue #1602: a recorded request carries `ip` (and `form` for a urlencoded body), as Mountebank's
//! `httpRequest.js` records them. Both are derived when the entry is printed.

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

#[tokio::test]
async fn recorded_requests_print_ip_and_form_on_every_door() {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
        .bind()
        .await
        .expect("admin API binds");
    let admin = format!("http://{}", running.local_addr());
    let client = reqwest::Client::new();

    let created = client
        .post(format!("{admin}/imposters"))
        .json(&json!({"protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"statusCode": 200}}]}]}))
        .send()
        .await
        .expect("POST");
    assert_eq!(created.status().as_u16(), 201);
    let port = created.json::<Value>().await.expect("json")["port"]
        .as_u64()
        .expect("assigned port");

    // An imposter with no `host` listens dual-stack, so an IPv4 client is `::ffff:127.0.0.1`,
    // as in Mountebank.
    let url = format!("http://127.0.0.1:{port}/f");
    client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("a=1&a=2&b=x")
        .send()
        .await
        .expect("form POST");
    client.get(&url).send().await.expect("GET");

    for path in [
        format!("{admin}/imposters/{port}"),
        format!("{admin}/imposters/{port}?replayable=false"),
    ] {
        let printed: Value = client
            .get(path)
            .send()
            .await
            .expect("GET imposter")
            .json()
            .await
            .expect("json");
        let requests = printed["requests"].as_array().expect("requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["ip"], json!("::ffff:127.0.0.1"));
        assert_eq!(requests[0]["form"], json!({"a": ["1", "2"], "b": "x"}));
        assert_eq!(requests[1]["ip"], json!("::ffff:127.0.0.1"));
        assert!(requests[1].get("form").is_none());
    }

    let _ = manager.delete_imposter(port as u16).await;
}
