//! Issue #1552: `PUT`/`DELETE`/`GET /imposters/:port/stubs/:index` route any segment, and every
//! index that is not the canonical decimal of an existing position (`abc`, `-1`, `1.0`, `01`,
//! `+1`, out of range, overflowing) answers Mountebank's `404` `bad data` — before a `PUT` body is
//! read. A non-integer used to answer the HTML "Cannot PUT" page, `01` and `+1` were read as `1`,
//! an out-of-range index answered `no such resource`, and a `PUT` checked its body first.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;

const STUB_INDEX_MESSAGE: &str = "'stubIndex' must be a valid integer, representing the array index position of the stub to replace";

/// Indexes that name no current position of a two-stub imposter.
const BAD_INDEXES: [&str; 10] = [
    "abc",
    "-1",
    "1.0",
    "-0",
    "01",
    "00",
    "+1",
    "2",
    "5",
    "18446744073709551616",
];

async fn start_server() -> (String, rift_http_proxy::admin_api::RunningAdminApi) {
    let manager = Arc::new(ImposterManager::new());
    let addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let running = AdminApiServer::new(addr, manager, None)
        .bind()
        .await
        .expect("bind admin");
    (format!("http://{}", running.local_addr()), running)
}

async fn create_two_stubs(admin: &str, client: &reqwest::Client) -> u16 {
    let resp = client
        .post(format!("{admin}/imposters"))
        .json(&json!({
            "port": 0, "protocol": "http",
            "stubs": [
                {"responses": [{"is": {"body": "a"}}]},
                {"responses": [{"is": {"body": "b"}}]}
            ]
        }))
        .send()
        .await
        .expect("create");
    assert_eq!(resp.status().as_u16(), 201);
    let body: Value = resp.json().await.expect("JSON body");
    u16::try_from(body["port"].as_u64().expect("assigned port")).expect("port fits u16")
}

async fn stub_bodies(admin: &str, client: &reqwest::Client, port: u16) -> Vec<Value> {
    let body: Value = client
        .get(format!("{admin}/imposters/{port}"))
        .send()
        .await
        .expect("GET imposter")
        .json()
        .await
        .expect("JSON body");
    body["stubs"]
        .as_array()
        .expect("stubs array")
        .iter()
        .map(|stub| stub["responses"][0]["is"]["body"].clone())
        .collect()
}

async fn assert_bad_index(resp: reqwest::Response, what: &str) {
    assert_eq!(resp.status().as_u16(), 404, "{what}");
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "{what}: {content_type}"
    );
    let body: Value = resp.json().await.expect("error JSON");
    assert_eq!(body["errors"][0]["type"], "bad data", "{what}: {body}");
    assert_eq!(
        body["errors"][0]["message"], STUB_INDEX_MESSAGE,
        "{what}: {body}"
    );
}

#[tokio::test]
async fn put_and_delete_refuse_an_index_that_is_not_a_current_position() {
    let (admin, _running) = start_server().await;
    let client = reqwest::Client::new();
    let port = create_two_stubs(&admin, &client).await;

    for index in BAD_INDEXES {
        let url = format!("{admin}/imposters/{port}/stubs/{index}");
        let put = client
            .put(&url)
            .json(&json!({"responses": [{"is": {"body": "new"}}]}))
            .send()
            .await
            .expect("PUT");
        assert_bad_index(put, &format!("PUT {index}")).await;
        let delete = client.delete(&url).send().await.expect("DELETE");
        assert_bad_index(delete, &format!("DELETE {index}")).await;
        assert_eq!(
            stub_bodies(&admin, &client, port).await,
            vec![json!("a"), json!("b")],
            "{index} changed the stubs"
        );
    }

    // A canonical position still works.
    let put = client
        .put(format!("{admin}/imposters/{port}/stubs/1"))
        .json(&json!({"responses": [{"is": {"body": "new"}}]}))
        .send()
        .await
        .expect("PUT 1");
    assert_eq!(put.status().as_u16(), 200);
    assert_eq!(
        stub_bodies(&admin, &client, port).await,
        vec![json!("a"), json!("new")]
    );
}

#[tokio::test]
async fn put_checks_the_index_before_the_body() {
    let (admin, _running) = start_server().await;
    let client = reqwest::Client::new();
    let port = create_two_stubs(&admin, &client).await;

    let put = client
        .put(format!("{admin}/imposters/{port}/stubs/5"))
        .json(&json!({"responses": [{"is": false}]}))
        .send()
        .await
        .expect("PUT");
    assert_bad_index(put, "PUT 5 with an invalid body").await;

    let put = client
        .put(format!("{admin}/imposters/{port}/stubs/abc"))
        .body("not json")
        .send()
        .await
        .expect("PUT");
    assert_bad_index(put, "PUT abc with a non-JSON body").await;
}

#[tokio::test]
async fn an_absent_imposter_is_answered_before_the_index() {
    let (admin, _running) = start_server().await;
    let client = reqwest::Client::new();
    // No imposter is created: port 1 is absent.
    for index in ["abc", "01", "0"] {
        let url = format!("{admin}/imposters/1/stubs/{index}");
        let put = client
            .put(&url)
            .json(&json!({"responses": [{"is": {}}]}))
            .send()
            .await
            .expect("PUT");
        let delete = client.delete(&url).send().await.expect("DELETE");
        let get = client.get(&url).send().await.expect("GET");
        for (method, resp) in [("PUT", put), ("DELETE", delete), ("GET", get)] {
            assert_eq!(resp.status().as_u16(), 404, "{method} {index}");
            let body: Value = resp.json().await.expect("error JSON");
            assert_eq!(
                body["errors"][0]["type"], "no such resource",
                "{method} {index}: {body}"
            );
        }
    }
}

#[tokio::test]
async fn options_on_a_non_integer_index_reports_the_route() {
    let (admin, _running) = start_server().await;
    let client = reqwest::Client::new();
    let port = create_two_stubs(&admin, &client).await;

    let resp = client
        .request(
            reqwest::Method::OPTIONS,
            format!("{admin}/imposters/{port}/stubs/abc"),
        )
        .send()
        .await
        .expect("OPTIONS");
    assert_eq!(resp.status().as_u16(), 200);
    let allow = resp
        .headers()
        .get("allow")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(allow, "DELETE, GET, HEAD, PUT");
}

#[tokio::test]
async fn the_by_id_route_is_not_read_as_an_index() {
    let (admin, _running) = start_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{admin}/imposters"))
        .json(&json!({
            "port": 0, "protocol": "http",
            "stubs": [{"id": "s1", "responses": [{"is": {"body": "a"}}]}]
        }))
        .send()
        .await
        .expect("create");
    assert_eq!(resp.status().as_u16(), 201);
    let body: Value = resp.json().await.expect("JSON body");
    let port = body["port"].as_u64().expect("assigned port");

    let resp = client
        .get(format!("{admin}/imposters/{port}/stubs/by-id/s1"))
        .send()
        .await
        .expect("GET by id");
    assert_eq!(resp.status().as_u16(), 200);
    let resp = client
        .get(format!("{admin}/imposters/{port}/stubs/by-id/missing"))
        .send()
        .await
        .expect("GET by missing id");
    assert_eq!(resp.status().as_u16(), 404);
    let body: Value = resp.json().await.expect("error JSON");
    assert_eq!(body["errors"][0]["type"], "no such resource", "{body}");
}
