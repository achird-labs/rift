//! Issue #1614: the admin `replayable` / `removeProxies` flags are real query parameters with exact
//! names and case-insensitive `true`, as Mountebank reads them. The parser itself landed with #1591;
//! this pins every probe row of #1614 against a live admin server, and covers the one view that was
//! still wrong: `GET /imposters?removeProxies=true` alone is the full imposters, proxies removed.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;

async fn start_server() -> (String, rift_http_proxy::admin_api::RunningAdminApi) {
    let manager = Arc::new(ImposterManager::new());
    let addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let running = AdminApiServer::new(addr, manager, None)
        .bind()
        .await
        .expect("bind admin");
    (format!("http://{}", running.local_addr()), running)
}

/// An imposter with one `is` stub and one `proxy` stub; returns its port.
async fn seed(admin: &str, client: &reqwest::Client) -> u16 {
    let resp = client
        .post(format!("{admin}/imposters"))
        .json(&json!({
            "protocol": "http",
            "stubs": [
                {"responses": [{"is": {"statusCode": 200}}]},
                {"responses": [{"proxy": {"to": "http://127.0.0.1:9"}}]}
            ]
        }))
        .send()
        .await
        .expect("create");
    assert_eq!(resp.status().as_u16(), 201);
    resp.json::<Value>().await.expect("body")["port"]
        .as_u64()
        .expect("port") as u16
}

#[derive(Debug, PartialEq)]
enum View {
    /// `_links`, no `stubs`
    List,
    /// `_links` and `stubs`
    Full(usize),
    /// no `_links`, `stubs`
    Replayable(usize),
    /// the summary (`stubCount`), Rift's default for `GET /imposters`
    Summary,
}

fn classify(imposter: &Value) -> View {
    let links = imposter.get("_links").is_some();
    let stubs = imposter
        .get("stubs")
        .and_then(Value::as_array)
        .map(Vec::len);
    match (links, stubs) {
        (true, None) if imposter.get("stubCount").is_some() => View::Summary,
        (true, None) => View::List,
        (true, Some(n)) => View::Full(n),
        (false, Some(n)) => View::Replayable(n),
        (false, None) => panic!("unrecognised view: {imposter}"),
    }
}

async fn get_view(admin: &str, client: &reqwest::Client, path: &str, query: &str) -> View {
    let body: Value = client
        .get(format!("{admin}{path}{query}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    match body.get("imposters") {
        Some(list) => classify(&list[0]),
        None => classify(&body),
    }
}

#[tokio::test]
async fn get_imposters_flag_matrix_matches_mountebank() {
    let (admin, _running) = start_server().await;
    let client = reqwest::Client::new();
    seed(&admin, &client).await;

    use View::*;
    let cases: &[(&str, View)] = &[
        ("", Summary),
        ("?list=true", List),
        ("?replayable=true", Replayable(2)),
        ("?replayable=TRUE", Replayable(2)),
        ("?replayable=True", Replayable(2)),
        ("?notreplayable=true", Summary),
        ("?replayable=trueish", Summary),
        ("?replayable=true%20", Summary),
        ("?replayable=1", Summary),
        ("?replayable=", Summary),
        ("?replayable", Summary),
        ("?replayable[]=true", Summary),
        ("?replayable=false", Summary),
        ("?replay%61ble=true", Replayable(2)),
        ("?replayable=tru%65", Replayable(2)),
        ("?replayable=true&removeProxies=TRUE", Replayable(1)),
        ("?replayable=true&noremoveProxies=true", Replayable(2)),
        ("?replayable=true&removeProxies=true", Replayable(1)),
        ("?replayable=true&x=1", Replayable(2)),
        ("?x=1&replayable=true", Replayable(2)),
        // Last value wins (Mountebank answers 500 here; Rift takes the last).
        ("?replayable=false&replayable=true", Replayable(2)),
        ("?replayable=true&replayable=false", Summary),
        // removeProxies alone is the full imposters, proxies removed.
        ("?removeProxies=true", Full(1)),
        ("?removeProxies=TRUE", Full(1)),
        ("?noremoveProxies=true", Summary),
        ("?removeProxies=false", Summary),
    ];
    for (query, want) in cases {
        let got = get_view(&admin, &client, "/imposters", query).await;
        assert_eq!(&got, want, "GET /imposters{query}");
    }
}

#[tokio::test]
async fn get_imposter_flag_matrix_matches_mountebank() {
    let (admin, _running) = start_server().await;
    let client = reqwest::Client::new();
    let port = seed(&admin, &client).await;
    let path = format!("/imposters/{port}");

    use View::*;
    let cases: &[(&str, View)] = &[
        ("", Full(2)),
        ("?replayable=TRUE", Replayable(2)),
        ("?notreplayable=true", Full(2)),
        ("?replayable=trueish", Full(2)),
        ("?replay%61ble=true", Replayable(2)),
        ("?removeProxies=true", Full(1)),
        ("?removeProxies=TRUE", Full(1)),
        ("?noremoveProxies=true", Full(2)),
        ("?replayable=true&removeProxies=true", Replayable(1)),
    ];
    for (query, want) in cases {
        let got = get_view(&admin, &client, &path, query).await;
        assert_eq!(&got, want, "GET {path}{query}");
    }
}

#[tokio::test]
async fn delete_imposter_honours_the_flags() {
    let (admin, _running) = start_server().await;
    let client = reqwest::Client::new();

    let port = seed(&admin, &client).await;
    let resp = client
        .delete(format!(
            "{admin}/imposters/{port}?replayable=TRUE&removeProxies=true"
        ))
        .send()
        .await
        .expect("delete");
    let body: Value = resp.json().await.expect("json");
    assert_eq!(classify(&body), View::Replayable(1));

    let port = seed(&admin, &client).await;
    let body: Value = client
        .delete(format!("{admin}/imposters/{port}?notreplayable=true"))
        .send()
        .await
        .expect("delete")
        .json()
        .await
        .expect("json");
    assert_eq!(classify(&body), View::Full(2));
}

#[tokio::test]
async fn delete_all_honours_the_flags() {
    let (admin, _running) = start_server().await;
    let client = reqwest::Client::new();

    seed(&admin, &client).await;
    let body: Value = client
        .delete(format!("{admin}/imposters?removeProxies=true"))
        .send()
        .await
        .expect("delete")
        .json()
        .await
        .expect("json");
    assert_eq!(classify(&body["imposters"][0]), View::Replayable(1));

    seed(&admin, &client).await;
    let body: Value = client
        .delete(format!("{admin}/imposters?replayable=FALSE"))
        .send()
        .await
        .expect("delete")
        .json()
        .await
        .expect("json");
    assert_eq!(classify(&body["imposters"][0]), View::Full(2));
}
