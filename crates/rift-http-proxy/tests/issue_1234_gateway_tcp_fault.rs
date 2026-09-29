//! Issue #1234: a TCP fault reached through the shared listeners — the admin API's `/__rift/`
//! gateway and the front door — aborts the client connection exactly as it does on the imposter's
//! own port, instead of framing the fault's placeholder `502` as an HTTP answer.
//!
//! HTTP/1: byte-for-byte the imposter port's behaviour, per fault kind.
//! HTTP/2: every kind resets that one stream (`RST_STREAM`, `INTERNAL_ERROR`); sibling streams on
//! the same connection keep working, because aborting the socket would take down every other
//! stream multiplexed on it (the reason an imposter with a TCP fault is HTTP/1-only, #295).

use arc_swap::ArcSwap;
use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::front_door::{
    CompiledRoutes, Route, RouteMatch, RouteTable, RouteTarget, RunningFrontDoor, bind_front_door,
};
use rift_http_proxy::imposter::ImposterManager;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Every carrier site: a top-level Mountebank `fault`, `_rift.fault.tcp`, and a script `reset()`.
const FAULT_PATHS: [&str; 5] = ["/reset", "/empty", "/garbage", "/malformed", "/script"];

async fn fault_imposter(manager: &ImposterManager, port: u16) {
    let config = serde_json::from_value(serde_json::json!({
        "port": port, "protocol": "http", "recordRequests": true,
        "stubs": [
            { "predicates": [{ "equals": { "path": "/reset" } }],
              "responses": [{ "fault": "CONNECTION_RESET_BY_PEER" }] },
            { "predicates": [{ "equals": { "path": "/empty" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "never-seen" },
                              "_rift": { "fault": { "tcp": "empty" } } }] },
            { "predicates": [{ "equals": { "path": "/garbage" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "never-seen" },
                              "_rift": { "fault": { "tcp": "garbage" } } }] },
            { "predicates": [{ "equals": { "path": "/malformed" } }],
              "responses": [{ "fault": "MALFORMED_RESPONSE_CHUNK" }] },
            { "predicates": [{ "equals": { "path": "/script" } }],
              "responses": [{ "_rift": { "script": {
                  "engine": "rhai", "code": "fn respond(ctx) { reset() }" } } }] },
            { "predicates": [{ "equals": { "path": "/ok" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }
        ]
    }))
    .expect("fault imposter config");
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");
    tokio::time::sleep(Duration::from_millis(200)).await;
}

async fn start_admin(manager: Arc<ImposterManager>) -> (RunningAdminApi, String) {
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .bind()
        .await
        .expect("bind admin API");
    let base = format!("http://{}", running.local_addr());
    (running, base)
}

/// A front door whose one route sends `/routed/<path>` to `port` as `/<path>`; its `/__rift/`
/// fallback reaches the same imposter by port.
async fn start_front_door(manager: Arc<ImposterManager>, port: u16) -> (RunningFrontDoor, String) {
    let table = RouteTable {
        routes: vec![Route {
            id: "routed".to_owned(),
            priority: 0,
            matches: RouteMatch {
                path_prefix: Some("/routed".to_owned()),
                ..RouteMatch::default()
            },
            target: RouteTarget {
                port,
                strip_prefix: true,
                set_host: None,
            },
            enabled: true,
        }],
    };
    let compiled = Arc::new(ArcSwap::new(Arc::new(CompiledRoutes::new(&table))));
    let running = bind_front_door("127.0.0.1:0".parse().unwrap(), manager, compiled)
        .await
        .expect("bind front door");
    let base = format!("http://{}", running.local_addr());
    (running, base)
}

/// What an HTTP/1 client observes (same probe as `admin_api_integration.rs`'s `tcp_faults`).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Observed {
    /// The request failed before a valid HTTP response (reset / empty close / bad framing).
    SendFailed,
    /// Response headers parsed (real status line) but the body read failed.
    BodyFailed,
    /// A complete, normal HTTP response: the fault did NOT fire.
    FullResponse,
}

/// A fresh HTTP/1 client per call, so one probe's aborted connection is never reused by the next.
async fn observe(url: &str) -> Observed {
    let client = reqwest::Client::builder()
        .http1_only()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    match client.get(url).send().await {
        Err(_) => Observed::SendFailed,
        Ok(resp) => match resp.bytes().await {
            Err(_) => Observed::BodyFailed,
            Ok(_) => Observed::FullResponse,
        },
    }
}

fn expected(path: &str) -> Observed {
    match path {
        // The status line of the malformed-chunk fault parses; only the body fails.
        "/malformed" => Observed::BodyFailed,
        _ => Observed::SendFailed,
    }
}

// The h1 matrix: {admin `/__rift/`, front door `/__rift/`, front door matched route} x every carrier
// site, each observed exactly as the imposter's own port observes it.
#[tokio::test]
async fn h1_fault_through_every_door_matches_the_imposter_port() {
    let port = 21340;
    let manager = Arc::new(ImposterManager::new());
    fault_imposter(&manager, port).await;
    let (admin, admin_base) = start_admin(manager.clone()).await;
    let (front, front_base) = start_front_door(manager.clone(), port).await;

    for path in FAULT_PATHS {
        let direct = observe(&format!("http://127.0.0.1:{port}{path}")).await;
        assert_eq!(direct, expected(path), "imposter port, {path}");
        for (door, url) in [
            (
                "admin /__rift/",
                format!("{admin_base}/__rift/{port}{path}"),
            ),
            (
                "front door /__rift/",
                format!("{front_base}/__rift/{port}{path}"),
            ),
            ("front door route", format!("{front_base}/routed{path}")),
        ] {
            assert_eq!(
                observe(&url).await,
                direct,
                "{door} {path} must fail the way the imposter port does, not serve the 502 placeholder"
            );
        }
    }

    // An ordinary stub on the same imposter still answers through every door.
    for url in [
        format!("{admin_base}/__rift/{port}/ok"),
        format!("{front_base}/__rift/{port}/ok"),
        format!("{front_base}/routed/ok"),
    ] {
        let resp = reqwest::get(&url).await.expect("ok request");
        assert_eq!(resp.status(), 200, "{url}");
        assert_eq!(resp.text().await.unwrap(), "ok", "{url}");
    }

    admin.shutdown().await;
    front.shutdown().await;
    manager.delete_all().await;
}

async fn assert_h2_stream_reset(base: &str, port: u16, label: &str) {
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .timeout(Duration::from_secs(3))
        .build()
        .expect("h2 client");
    let ok = |resp: reqwest::Response| async move {
        assert_eq!(resp.version(), reqwest::Version::HTTP_2, "{label}");
        assert_eq!(resp.status(), 200, "{label}");
        assert_eq!(resp.text().await.unwrap(), "ok", "{label}");
    };

    ok(client
        .get(format!("{base}/__rift/{port}/ok"))
        .send()
        .await
        .expect("h2 ok before the fault"))
    .await;
    for path in FAULT_PATHS {
        let err = client
            .get(format!("{base}/__rift/{port}{path}"))
            .send()
            .await
            .expect_err(&format!(
                "{label} {path}: an h2 fault must reset the stream"
            ));
        let chain = format!("{err:?}");
        assert!(
            chain.contains("INTERNAL_ERROR"),
            "{label} {path}: expected RST_STREAM(INTERNAL_ERROR), got {chain}"
        );
        // A sibling stream on the same client still works: only that one stream was reset.
        ok(client
            .get(format!("{base}/__rift/{port}/ok"))
            .send()
            .await
            .expect("h2 ok after the fault"))
        .await;
    }
}

#[tokio::test]
async fn h2_fault_resets_only_its_stream_on_both_doors() {
    let port = 21341;
    let manager = Arc::new(ImposterManager::new());
    fault_imposter(&manager, port).await;
    let (admin, admin_base) = start_admin(manager.clone()).await;
    let (front, front_base) = start_front_door(manager.clone(), port).await;

    assert_h2_stream_reset(&admin_base, port, "admin").await;
    assert_h2_stream_reset(&front_base, port, "front door").await;

    admin.shutdown().await;
    front.shutdown().await;
    manager.delete_all().await;
}

// h1 keep-alive: ok, fault, ok on one client. The fault kills that connection; the listener and the
// client's next request (on a new connection) are unaffected.
#[tokio::test]
async fn h1_keep_alive_ok_fault_ok() {
    let port = 21342;
    let manager = Arc::new(ImposterManager::new());
    fault_imposter(&manager, port).await;
    let (admin, admin_base) = start_admin(manager.clone()).await;
    let (front, front_base) = start_front_door(manager.clone(), port).await;

    for base in [
        format!("{admin_base}/__rift/{port}"),
        format!("{front_base}/__rift/{port}"),
        format!("{front_base}/routed"),
    ] {
        let client = reqwest::Client::builder()
            .http1_only()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let first = client.get(format!("{base}/ok")).send().await.expect("ok");
        assert_eq!(first.text().await.unwrap(), "ok");
        assert!(
            client.get(format!("{base}/reset")).send().await.is_err(),
            "{base}/reset must abort, not answer"
        );
        let last = client.get(format!("{base}/ok")).send().await.expect("ok");
        assert_eq!(last.text().await.unwrap(), "ok");
    }

    admin.shutdown().await;
    front.shutdown().await;
    manager.delete_all().await;
}

// Pin (green before and after): the journal already records a gateway fault without a `status`
// (#1227) — aborting the connection must not change that.
#[tokio::test]
async fn gateway_fault_is_journaled_without_status() {
    let port = 21343;
    let manager = Arc::new(ImposterManager::new());
    fault_imposter(&manager, port).await;
    let (admin, admin_base) = start_admin(manager.clone()).await;

    let _ = observe(&format!("{admin_base}/__rift/{port}/reset")).await;
    let _ = observe(&format!("{admin_base}/__rift/{port}/ok")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = manager
        .get_imposter(port)
        .expect("imposter")
        .get_recorded_requests();
    let fault = recorded
        .iter()
        .find(|r| r.path == "/reset")
        .expect("fault request journaled");
    assert_eq!(
        fault.status, None,
        "a TCP fault is not an answer: {fault:?}"
    );
    let ok = recorded
        .iter()
        .find(|r| r.path == "/ok")
        .expect("ok request journaled");
    assert_eq!(ok.status, Some(200));

    admin.shutdown().await;
    manager.delete_all().await;
}

// ---------------------------------------------------------------------------------------------
// Logging: an injected fault is the configured behaviour, not a server error. The front door used
// to write one `ERROR Front door connection error` line per fault.
// ---------------------------------------------------------------------------------------------

type Captured = Arc<Mutex<Vec<(tracing::Level, String)>>>;

struct Capture(Captured);

struct MessageVisitor<'a>(&'a mut String);

impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.push_str(&format!("{value:?}"));
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        if meta.target().starts_with("rift") && *meta.level() <= tracing::Level::DEBUG {
            let mut message = String::new();
            event.record(&mut MessageVisitor(&mut message));
            self.0.lock().unwrap().push((*meta.level(), message));
        }
    }
}

/// Process-wide capture: listener tasks run on runtime worker threads, which a thread-local
/// default would not see.
fn captured() -> Captured {
    static CAPTURED: OnceLock<Captured> = OnceLock::new();
    CAPTURED
        .get_or_init(|| {
            use tracing_subscriber::layer::SubscriberExt;
            let captured = Captured::default();
            let subscriber = tracing_subscriber::registry().with(Capture(captured.clone()));
            tracing::subscriber::set_global_default(subscriber).expect("install log capture");
            captured
        })
        .clone()
}

#[tokio::test]
async fn injected_faults_are_not_logged_as_errors() {
    let captured = captured();
    let port = 21344;
    let manager = Arc::new(ImposterManager::new());
    fault_imposter(&manager, port).await;
    let (admin, admin_base) = start_admin(manager.clone()).await;
    let (front, front_base) = start_front_door(manager.clone(), port).await;

    for path in FAULT_PATHS {
        let _ = observe(&format!("{front_base}/__rift/{port}{path}")).await;
        let _ = observe(&format!("{front_base}/routed{path}")).await;
        let _ = observe(&format!("{admin_base}/__rift/{port}{path}")).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let lines = captured.lock().unwrap().clone();
    let errors: Vec<_> = lines
        .iter()
        .filter(|(level, msg)| *level == tracing::Level::ERROR && msg.contains("connection"))
        .collect();
    assert!(
        errors.is_empty(),
        "an injected fault must not log a connection ERROR: {errors:?}"
    );
    assert!(
        lines
            .iter()
            .any(|(level, msg)| *level == tracing::Level::DEBUG
                && msg.contains("injected TCP fault")),
        "the front door still records the abort, at debug: {lines:?}"
    );

    admin.shutdown().await;
    front.shutdown().await;
    manager.delete_all().await;
}
