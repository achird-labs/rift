//! Front-door listener integration tests (issue #19 / U-11).

use arc_swap::ArcSwap;
use rift_http_proxy::front_door::{
    CompiledRoutes, RouteMatch, RouteObserver, RouteTable, RouteTarget, bind_front_door,
    bind_front_door_at, bind_front_door_with_observer,
};
use rift_http_proxy::imposter::ImposterManager;
use rift_http_proxy::server::{ListenAddr, parse_front_door_addr};
use std::sync::{Arc, Mutex};

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
async fn exact_path_imposter(manager: &ImposterManager, path: &str, body: &str) -> u16 {
    manager
        .create_imposter(
            serde_json::from_value(serde_json::json!({
                "protocol": "http",
                "stubs": [{
                    "predicates": [{"equals": {"path": path}}],
                    "responses": [{"is": {"statusCode": 200, "body": body}}]
                }]
            }))
            .unwrap(),
        )
        .await
        .expect("create imposter")
}

/// A port with no imposter on it: bound to learn it, then released.
fn unused_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
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

/// A [`RouteObserver`] that records every id it is told about, in call order — including
/// duplicates, since the whole point is to count dispatches, not to de-dupe them.
#[derive(Default)]
struct RecordingObserver {
    seen: Mutex<Vec<String>>,
}

impl RecordingObserver {
    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("recording observer mutex").clone()
    }
}

impl RouteObserver for RecordingObserver {
    fn note_dispatch(&self, route_id: &str) {
        self.seen
            .lock()
            .expect("recording observer mutex")
            .push(route_id.to_owned());
    }
}

/// Like [`start_front_door`], but wired to a [`RecordingObserver`] so a test can assert exactly
/// which routes were observed as dispatched.
async fn start_front_door_with_observer(
    manager: Arc<ImposterManager>,
    table: RouteTable,
) -> (
    rift_http_proxy::front_door::RunningFrontDoor,
    String,
    Arc<RecordingObserver>,
) {
    let compiled = Arc::new(ArcSwap::new(Arc::new(CompiledRoutes::new(&table))));
    let observer = Arc::new(RecordingObserver::default());
    let running = bind_front_door_with_observer(
        "127.0.0.1:0".parse().unwrap(),
        manager,
        compiled,
        Some(observer.clone() as Arc<dyn RouteObserver>),
    )
    .await
    .expect("bind front door with observer");
    let base = format!("http://{}", running.local_addr());
    (running, base, observer)
}

#[tokio::test]
async fn host_routes_reach_different_imposters() {
    let manager = Arc::new(ImposterManager::new());
    let port_a = exact_path_imposter(&manager, "/", "from-a").await;
    let port_b = exact_path_imposter(&manager, "/", "from-b").await;

    let table = RouteTable {
        routes: vec![
            route("a", host_match("a.test"), port_a, false),
            route("b", host_match("b.test"), port_b, false),
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
    let port = exact_path_imposter(&manager, "/x", "stripped").await;

    let table = RouteTable {
        routes: vec![route("p", prefix_match("/api"), port, true)],
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
    let port = exact_path_imposter(&manager, "/api/x", "unstripped").await;

    let table = RouteTable {
        routes: vec![route("p", prefix_match("/api"), port, false)],
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
    let port = exact_path_imposter(&manager, "/", "root").await;

    let table = RouteTable {
        routes: vec![route("p", prefix_match("/api"), port, true)],
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
    let strip_port = manager
        .create_imposter(
            serde_json::from_value(serde_json::json!({
                "protocol": "http",
                "stubs": [{
                    "predicates": [{"equals": {"path": "/x", "query": {"q": "1"}}}],
                    "responses": [{"is": {"statusCode": 200, "body": "stripped-query"}}]
                }]
            }))
            .unwrap(),
        )
        .await
        .expect("create imposter");
    let pass_port = manager
        .create_imposter(
            serde_json::from_value(serde_json::json!({
                "protocol": "http",
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
            route("strip", prefix_match("/api"), strip_port, true),
            route("pass", host_match("pass.test"), pass_port, false),
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
    let dead_port = unused_port();
    let table = RouteTable {
        routes: vec![route("only", host_match("only.test"), dead_port, false)],
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
    let dead_port = unused_port();
    // No imposter is ever created on the unused port — the route matches, its target does not exist.
    let table = RouteTable {
        routes: vec![route("dead", host_match("dead.test"), dead_port, false)],
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
    let port = exact_path_imposter(&manager, "/y", "gatewayed").await;

    let (running, base) = start_front_door(manager.clone(), RouteTable::default()).await;

    let resp = reqwest::get(format!("{base}/__rift/{port}/y"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "gatewayed");

    running.shutdown().await;
}

// --- RouteObserver (issue #368) ---------------------------------------------------------------

#[tokio::test]
async fn a_matched_dispatch_is_observed_exactly_once_with_its_route_id() {
    let manager = Arc::new(ImposterManager::new());
    let port = exact_path_imposter(&manager, "/", "from-a").await;

    let table = RouteTable {
        routes: vec![route("a", host_match("a.test"), port, false)],
    };
    let (running, base, observer) = start_front_door_with_observer(manager.clone(), table).await;

    let resp = reqwest::Client::new()
        .get(&base)
        .header(reqwest::header::HOST, "a.test")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(observer.seen(), vec!["a".to_owned()]);

    running.shutdown().await;
}

#[tokio::test]
async fn two_requests_to_the_same_route_produce_two_observations() {
    let manager = Arc::new(ImposterManager::new());
    let port = exact_path_imposter(&manager, "/", "from-a").await;

    let table = RouteTable {
        routes: vec![route("a", host_match("a.test"), port, false)],
    };
    let (running, base, observer) = start_front_door_with_observer(manager.clone(), table).await;
    let client = reqwest::Client::new();

    for _ in 0..2 {
        let resp = client
            .get(&base)
            .header(reqwest::header::HOST, "a.test")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Two dispatches to the same route are two observations, not one — the observer counts,
    // it does not de-dupe.
    assert_eq!(observer.seen(), vec!["a".to_owned(), "a".to_owned()]);

    running.shutdown().await;
}

#[tokio::test]
async fn requests_to_different_routes_are_attributed_to_the_right_id() {
    let manager = Arc::new(ImposterManager::new());
    let port_a = exact_path_imposter(&manager, "/", "from-a").await;
    let port_b = exact_path_imposter(&manager, "/", "from-b").await;

    let table = RouteTable {
        routes: vec![
            route("a", host_match("a.test"), port_a, false),
            route("b", host_match("b.test"), port_b, false),
        ],
    };
    let (running, base, observer) = start_front_door_with_observer(manager.clone(), table).await;
    let client = reqwest::Client::new();

    client
        .get(&base)
        .header(reqwest::header::HOST, "b.test")
        .send()
        .await
        .unwrap();
    client
        .get(&base)
        .header(reqwest::header::HOST, "a.test")
        .send()
        .await
        .unwrap();

    assert_eq!(observer.seen(), vec!["b".to_owned(), "a".to_owned()]);

    running.shutdown().await;
}

#[tokio::test]
async fn a_route_whose_imposter_is_gone_is_still_observed() {
    let manager = Arc::new(ImposterManager::new());
    let dead_port = unused_port();
    // No imposter is ever created on the unused port — the route matches, its target does not exist, and
    // the response is a 404. The route still claimed the request: this is the case a HITS column
    // exists to reveal (a route that only ever 404s), so it must be observed exactly like a
    // healthy dispatch.
    let table = RouteTable {
        routes: vec![route("dead", host_match("dead.test"), dead_port, false)],
    };
    let (running, base, observer) = start_front_door_with_observer(manager.clone(), table).await;

    let resp = reqwest::Client::new()
        .get(&base)
        .header(reqwest::header::HOST, "dead.test")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(observer.seen(), vec!["dead".to_owned()]);

    running.shutdown().await;
}

#[tokio::test]
async fn gateway_addressing_is_not_observed() {
    let manager = Arc::new(ImposterManager::new());
    let port = exact_path_imposter(&manager, "/y", "gatewayed").await;

    // Empty route table: nothing can claim this request via the route table, so it falls through
    // to the single-port gateway's own `/__rift/:port` addressing — which never claimed a route
    // and must not be observed.
    let (running, base, observer) =
        start_front_door_with_observer(manager.clone(), RouteTable::default()).await;

    let resp = reqwest::get(format!("{base}/__rift/{port}/y"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(observer.seen().is_empty());

    running.shutdown().await;
}

#[tokio::test]
async fn an_unmatched_request_is_not_observed() {
    let manager = Arc::new(ImposterManager::new());
    let dead_port = unused_port();
    let table = RouteTable {
        routes: vec![route("only", host_match("only.test"), dead_port, false)],
    };
    let (running, base, observer) = start_front_door_with_observer(manager.clone(), table).await;

    let resp = reqwest::get(format!("{base}/nope")).await.unwrap();
    assert_eq!(resp.status(), 404);
    assert!(observer.seen().is_empty());

    running.shutdown().await;
}

/// Whether this host can listen on `[::1]` at all; CI runners without IPv6 skip the v6 half.
fn has_ipv6_loopback() -> bool {
    std::net::TcpListener::bind("[::1]:0").is_ok()
}

/// Issue #1561: an embedder reaches the CLI's bare-port rule, `ListenAddr::Any`, through the public
/// `bind_front_door_at`, and the port answers on IPv4 and (where bindable) IPv6.
#[tokio::test]
async fn bind_front_door_at_any_listens_on_both_families() {
    let manager = Arc::new(ImposterManager::new());
    let compiled = Arc::new(ArcSwap::new(Arc::new(CompiledRoutes::new(
        &RouteTable::default(),
    ))));
    let running = bind_front_door_at(ListenAddr::Any(0), manager, compiled, None)
        .await
        .expect("bind front door");
    let port = running.local_addr().port();

    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("IPv4 loopback must accept");
    if has_ipv6_loopback() {
        tokio::net::TcpStream::connect(("::1", port))
            .await
            .expect("IPv6 loopback must accept");
    }
    running.shutdown().await;
}

/// Issue #1561: the public parser applies the CLI's rule and its error message.
#[test]
fn parse_front_door_addr_is_public_and_applies_the_cli_rule() {
    assert_eq!(
        parse_front_door_addr("8080").expect("port"),
        ListenAddr::Any(8080)
    );
    assert_eq!(
        parse_front_door_addr("127.0.0.1:8080").expect("addr"),
        ListenAddr::Exact("127.0.0.1:8080".parse().expect("addr"))
    );
    let err = parse_front_door_addr("nonsense").expect_err("neither port nor HOST:PORT");
    assert!(err.to_string().contains("'nonsense'"), "{err}");
}
