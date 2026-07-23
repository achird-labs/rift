//! Front-door listener integration tests (issue #19 / U-11).

use arc_swap::ArcSwap;
use rift_http_proxy::front_door::{
    CompiledRoutes, RouteMatch, RouteTable, RouteTarget, bind_front_door,
};
use rift_http_proxy::imposter::ImposterManager;
use std::sync::Arc;

/// A route with everything defaulted except what the test cares about.
fn route(
    id: &str,
    matches: RouteMatch,
    port: u16,
    strip_prefix: bool,
) -> rift_http_proxy::front_door::Route {
    rift_http_proxy::front_door::Route {
        id: id.to_owned(),
        priority: 0,
        matches,
        target: RouteTarget {
            port,
            strip_prefix,
            set_host: None,
        },
        enabled: true,
    }
}

fn host_match(host: &str) -> RouteMatch {
    RouteMatch {
        host: Some(host.to_owned()),
        ..RouteMatch::default()
    }
}

fn prefix_match(prefix: &str) -> RouteMatch {
    RouteMatch {
        path_prefix: Some(prefix.to_owned()),
        ..RouteMatch::default()
    }
}

/// Create an imposter on `port` whose stub matches only an exact path (+ optional query),
/// responding with `body` — used to assert the imposter saw exactly that request target.
async fn exact_path_imposter(manager: &ImposterManager, port: u16, path: &str, body: &str) {
    manager
        .create_imposter(
            serde_json::from_value(serde_json::json!({
                "port": port, "protocol": "http",
                "stubs": [{
                    "predicates": [{"equals": {"path": path}}],
                    "responses": [{"is": {"statusCode": 200, "body": body}}]
                }]
            }))
            .unwrap(),
        )
        .await
        .expect("create imposter");
}

/// Bind a front door on an OS-assigned port serving `table`, returning the running handle and its
/// base URL.
async fn start_front_door(
    manager: Arc<ImposterManager>,
    table: RouteTable,
) -> (rift_http_proxy::front_door::RunningFrontDoor, String) {
    let compiled = Arc::new(ArcSwap::new(Arc::new(CompiledRoutes::new(&table))));
    let running = bind_front_door("127.0.0.1:0".parse().unwrap(), manager, compiled)
        .await
        .expect("bind front door");
    let base = format!("http://{}", running.local_addr());
    (running, base)
}

#[tokio::test]
async fn host_routes_reach_different_imposters() {
    let manager = Arc::new(ImposterManager::new());
    exact_path_imposter(&manager, 19900, "/", "from-a").await;
    exact_path_imposter(&manager, 19901, "/", "from-b").await;

    let table = RouteTable {
        routes: vec![
            route("a", host_match("a.test"), 19900, false),
            route("b", host_match("b.test"), 19901, false),
        ],
    };
    let (running, base) = start_front_door(manager.clone(), table).await;
    let client = reqwest::Client::new();

    let a = client
        .get(&base)
        .header(reqwest::header::HOST, "a.test")
        .send()
        .await
        .unwrap();
    assert_eq!(a.status(), 200);
    assert_eq!(a.text().await.unwrap(), "from-a");

    let b = client
        .get(&base)
        .header(reqwest::header::HOST, "b.test")
        .send()
        .await
        .unwrap();
    assert_eq!(b.status(), 200);
    assert_eq!(b.text().await.unwrap(), "from-b");

    running.shutdown().await;
}

#[tokio::test]
async fn strip_prefix_true_strips_the_path_the_imposter_sees() {
    let manager = Arc::new(ImposterManager::new());
    // The imposter only has a stub for the STRIPPED path; if the front door failed to strip,
    // this predicate would never match and the imposter's Mountebank-style 200-with-no-match
    // fallback (or a 4xx) would show up instead of "stripped".
    exact_path_imposter(&manager, 19902, "/x", "stripped").await;

    let table = RouteTable {
        routes: vec![route("p", prefix_match("/api"), 19902, true)],
    };
    let (running, base) = start_front_door(manager.clone(), table).await;

    let resp = reqwest::get(format!("{base}/api/x")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "stripped");

    running.shutdown().await;
}

#[tokio::test]
async fn strip_prefix_false_leaves_the_true_path_intact() {
    let manager = Arc::new(ImposterManager::new());
    // Without stripping, the imposter must see the untouched `/api/x`.
    exact_path_imposter(&manager, 19903, "/api/x", "unstripped").await;

    let table = RouteTable {
        routes: vec![route("p", prefix_match("/api"), 19903, false)],
    };
    let (running, base) = start_front_door(manager.clone(), table).await;

    let resp = reqwest::get(format!("{base}/api/x")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "unstripped");

    running.shutdown().await;
}

#[tokio::test]
async fn strip_prefix_of_the_prefix_itself_yields_root_not_empty() {
    let manager = Arc::new(ImposterManager::new());
    exact_path_imposter(&manager, 19904, "/", "root").await;

    let table = RouteTable {
        routes: vec![route("p", prefix_match("/api"), 19904, true)],
    };
    let (running, base) = start_front_door(manager.clone(), table).await;

    // `/api` exactly, no trailing segment: stripping must produce `/`, never `""`.
    let resp = reqwest::get(format!("{base}/api")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "root");

    running.shutdown().await;
}

#[tokio::test]
async fn query_strings_survive_stripping_and_passthrough() {
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(
            serde_json::from_value(serde_json::json!({
                "port": 19905, "protocol": "http",
                "stubs": [{
                    "predicates": [{"equals": {"path": "/x", "query": {"q": "1"}}}],
                    "responses": [{"is": {"statusCode": 200, "body": "stripped-query"}}]
                }]
            }))
            .unwrap(),
        )
        .await
        .expect("create imposter");
    manager
        .create_imposter(
            serde_json::from_value(serde_json::json!({
                "port": 19906, "protocol": "http",
                "stubs": [{
                    "predicates": [{"equals": {"path": "/api/x", "query": {"q": "1"}}}],
                    "responses": [{"is": {"statusCode": 200, "body": "unstripped-query"}}]
                }]
            }))
            .unwrap(),
        )
        .await
        .expect("create imposter");

    let table = RouteTable {
        routes: vec![
            route("strip", prefix_match("/api"), 19905, true),
            route("pass", host_match("pass.test"), 19906, false),
        ],
    };
    let (running, base) = start_front_door(manager.clone(), table).await;
    let client = reqwest::Client::new();

    let stripped = reqwest::get(format!("{base}/api/x?q=1")).await.unwrap();
    assert_eq!(stripped.status(), 200);
    assert_eq!(stripped.text().await.unwrap(), "stripped-query");

    let passthrough = client
        .get(format!("{base}/api/x?q=1"))
        .header(reqwest::header::HOST, "pass.test")
        .send()
        .await
        .unwrap();
    assert_eq!(passthrough.status(), 200);
    assert_eq!(passthrough.text().await.unwrap(), "unstripped-query");

    running.shutdown().await;
}

#[tokio::test]
async fn unmatched_request_is_404_with_the_no_route_header() {
    let manager = Arc::new(ImposterManager::new());
    let table = RouteTable {
        routes: vec![route("only", host_match("only.test"), 19907, false)],
    };
    let (running, base) = start_front_door(manager.clone(), table).await;

    let resp = reqwest::get(format!("{base}/nope")).await.unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(
        resp.headers()
            .get("x-rift-front-door")
            .map(|v| v.to_str().unwrap()),
        Some("no-route"),
        "an unmatched request must carry the no-route marker"
    );

    running.shutdown().await;
}

#[tokio::test]
async fn matched_route_to_a_dead_port_is_404_without_the_no_route_header() {
    let manager = Arc::new(ImposterManager::new());
    // No imposter is ever created on 19908 — the route matches, its target does not exist.
    let table = RouteTable {
        routes: vec![route("dead", host_match("dead.test"), 19908, false)],
    };
    let (running, base) = start_front_door(manager.clone(), table).await;

    let resp = reqwest::Client::new()
        .get(&base)
        .header(reqwest::header::HOST, "dead.test")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(
        resp.headers().get("x-rift-front-door").is_none(),
        "a matched route with no imposter is a different failure than no-route, so it must not \
         carry the no-route marker"
    );

    running.shutdown().await;
}

#[tokio::test]
async fn gateway_addressing_still_works_through_the_front_door_with_an_empty_table() {
    let manager = Arc::new(ImposterManager::new());
    exact_path_imposter(&manager, 19909, "/y", "gatewayed").await;

    let (running, base) = start_front_door(manager.clone(), RouteTable::default()).await;

    let resp = reqwest::get(format!("{base}/__rift/19909/y"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "gatewayed");

    running.shutdown().await;
}
