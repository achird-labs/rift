//! Issue #317: the server is embeddable — `ServerBuilder`, `run_metrics_server`, and
//! `dispatch_to_port` are library functions a custom binary can compose around its own
//! `ImposterManager`.

use clap::Parser;
use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::gateway::dispatch_to_port;
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use rift_http_proxy::server::{Cli, ServerBuilder, bind_metrics_server, run_metrics_server};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

fn imposter_cfg(port: u16, body: &str) -> ImposterConfig {
    serde_json::from_value(serde_json::json!({
        "protocol": "http",
        "port": port,
        "stubs": [{
            "predicates": [{"equals": {"path": "/ping"}}],
            "responses": [{"is": {"statusCode": 200, "body": body}}]
        }]
    }))
    .expect("test imposter config")
}

async fn wait_for_http(url: &str) {
    for _ in 0..50 {
        if reqwest::get(url).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("server at {url} did not come up");
}

// AC1: a ServerBuilder with an injected pre-built manager serves the admin API — the
// embedding seam: the admin API operates on OUR manager, not an internally built one.
#[tokio::test]
async fn server_builder_with_injected_manager_serves_admin_api() {
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(imposter_cfg(19480, "pong"))
        .await
        .expect("create imposter");

    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "12610",
        "--metrics-port",
        "19481",
    ])
    .expect("cli parse");

    tokio::spawn(ServerBuilder::from_cli(cli).manager(manager.clone()).run());
    wait_for_http("http://127.0.0.1:12610/health").await;

    let imposters: serde_json::Value = reqwest::get("http://127.0.0.1:12610/imposters")
        .await
        .expect("admin api reachable")
        .json()
        .await
        .expect("json");
    let ports: Vec<u64> = imposters["imposters"]
        .as_array()
        .expect("imposters array")
        .iter()
        .filter_map(|i| i["port"].as_u64())
        .collect();
    assert_eq!(
        ports,
        vec![19480],
        "admin API must serve the injected manager's imposters"
    );

    // The pre-built imposter also serves traffic (it was created before run()).
    let served = reqwest::get("http://127.0.0.1:19480/ping")
        .await
        .expect("imposter reachable")
        .text()
        .await
        .expect("body");
    assert_eq!(served, "pong");

    manager.delete_all().await;
}

// Issue #853: the manager `ServerBuilder` builds ITSELF must register the shipped flow-state
// backends. Deliberately no `.manager(...)` here — injecting one would test the wrong object.
//
// Why this guard exists: `default_flow_store_backends()` is unit-tested, and the admin API is
// tested against a manager the test builds by hand, so if a refactor dropped
// `.with_flow_store_backends(...)` from `ServerBuilder`, everything would still compile, clippy
// would pass, `verify-feature-propagation.sh` would pass (it checks the Cargo feature chain, not
// the registration call) — and every `rift` binary user with `backend: "redis"` would get
// "no such backend is registered". That is the #777 shape moved from the manifest to the code.
//
// The probe needs no Redis server: an UNREACHABLE url separates the two outcomes. Registered ⇒ the
// factory runs and fails to connect ("failed to build the Redis flow store"); not registered ⇒ the
// registry lookup misses first ("no such backend is registered"). Both are 400, so the body is what
// discriminates.
#[cfg(feature = "redis-backend")]
#[tokio::test]
async fn server_builder_registers_the_shipped_flow_store_backends() {
    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "12611",
        "--metrics-port",
        "19483",
    ])
    .expect("cli parse");

    tokio::spawn(ServerBuilder::from_cli(cli).run());
    wait_for_http("http://127.0.0.1:12611/health").await;

    let response = reqwest::Client::new()
        .post("http://127.0.0.1:12611/imposters")
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "port": 19482, "protocol": "http", "stubs": [],
                "_rift": { "flowState": {
                    "backend": "redis",
                    "redis": { "url": "redis://127.0.0.1:1" }
                }}
            })
            .to_string(),
        )
        .send()
        .await
        .expect("post imposter");

    assert_eq!(
        response.status(),
        400,
        "an unreachable redis must fail imposter creation, not downgrade to NoOp"
    );
    let body = response.text().await.expect("body");
    assert!(
        body.contains("failed to build the Redis flow store"),
        "the redis backend must be REGISTERED on the manager ServerBuilder builds — got {body:?}, \
         which means ServerBuilder no longer calls default_flow_store_backends()"
    );
    assert!(
        !body.contains("no such backend is registered"),
        "redis resolved as an unknown backend, so it was never registered: {body}"
    );
}

// AC2: run_metrics_server on an explicit loopback SocketAddr serves /metrics (and 404s
// elsewhere). Fixed port rather than :0 — the function never returns, so a :0 bind gives
// the caller no way to learn the assigned port.
#[tokio::test]
async fn run_metrics_server_serves_metrics() {
    // Issue #617: record the metric this test asserts on. The `rift_*` families are lazily
    // registered on first touch, so in a process running only this test the registry is empty
    // and /metrics returns an empty body. Without this the test passes only on a sibling test
    // having already recorded one — order-dependent, and it fails under `--exact`.
    rift_http_proxy::extensions::metrics::record_request("GET", 200);

    let addr: std::net::SocketAddr = "127.0.0.1:19482".parse().expect("addr");
    tokio::spawn(run_metrics_server(addr));
    wait_for_http("http://127.0.0.1:19482/metrics").await;

    let resp = reqwest::get("http://127.0.0.1:19482/metrics")
        .await
        .expect("metrics reachable");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("rift_requests_total"),
        "expected the rift_requests_total this test recorded, got: {body:.200}"
    );

    let not_found = reqwest::get("http://127.0.0.1:19482/other")
        .await
        .expect("request");
    assert_eq!(not_found.status(), 404);
}

// The internal-construction path: no injected manager, --configfile drives imposter
// creation, and the config source is wired through so POST /admin/reload re-reads it.
#[tokio::test]
async fn server_builder_internal_manager_loads_configfile_and_reloads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let cfg = |body: &str| {
        format!(
            r#"{{"imposters":[{{"port":19485,"protocol":"http","stubs":[
                {{"predicates":[{{"equals":{{"path":"/ping"}}}}],
                 "responses":[{{"is":{{"statusCode":200,"body":"{body}"}}}}]}}]}}]}}"#
        )
    };
    std::fs::write(&path, cfg("v1")).expect("write config");

    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "12612",
        "--metrics-port",
        "19486",
        "--configfile",
        path.to_str().expect("utf8 path"),
    ])
    .expect("cli parse");

    tokio::spawn(ServerBuilder::from_cli(cli).run());
    wait_for_http("http://127.0.0.1:12612/health").await;

    let served = reqwest::get("http://127.0.0.1:19485/ping")
        .await
        .expect("imposter reachable")
        .text()
        .await
        .expect("body");
    assert_eq!(served, "v1", "configfile imposter loaded by run()");

    std::fs::write(&path, cfg("v2")).expect("rewrite config");
    let resp = reqwest::Client::new()
        .post("http://127.0.0.1:12612/admin/reload")
        .send()
        .await
        .expect("reload");
    assert_eq!(resp.status(), 200, "config source is wired for reload");
    let served = reqwest::get("http://127.0.0.1:19485/ping")
        .await
        .expect("imposter reachable")
        .text()
        .await
        .expect("body");
    assert_eq!(served, "v2", "reload re-read the configfile");
}

// The documented timing contract: a missing TLS-defaults file fails at run(), as an Err —
// not in from_cli, and not a panic.
#[tokio::test]
async fn server_builder_run_fails_on_missing_tls_cert_file() {
    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "12613",
        "--metrics-port",
        "19487",
        "--default-tls-cert",
        "/nonexistent/cert.pem",
        "--default-tls-key",
        "/nonexistent/key.pem",
    ])
    .expect("cli parse: from_cli surface must accept the flags without touching the files");

    let result = ServerBuilder::from_cli(cli).run().await;
    assert!(
        result.is_err(),
        "missing TLS default cert file must surface as a run() error"
    );
}

/// A bare hyper listener that forwards every request in-process to `target_port` via
/// `dispatch_to_port` — the "callable from any listener" shape from the issue.
fn spawn_dispatch_listener(
    listener: tokio::net::TcpListener,
    manager: Arc<ImposterManager>,
    target_port: u16,
) {
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let mgr = manager.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let mgr = mgr.clone();
                    async move {
                        Ok::<_, std::convert::Infallible>(
                            dispatch_to_port(&mgr, target_port, req).await,
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
}

// AC3: dispatch_to_port is the #212 gateway core, callable from any listener — here a
// bare hyper listener that forwards every request in-process to the imposter on 19483.
#[tokio::test]
async fn dispatch_to_port_routes_in_process() {
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(imposter_cfg(19483, "dispatched"))
        .await
        .expect("create imposter");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:12611")
        .await
        .expect("bind listener");
    spawn_dispatch_listener(listener, manager.clone(), 19483);
    wait_for_http("http://127.0.0.1:12611/ping").await;

    let served = reqwest::get("http://127.0.0.1:12611/ping")
        .await
        .expect("listener reachable")
        .text()
        .await
        .expect("body");
    assert_eq!(
        served, "dispatched",
        "request routed to the imposter in-process"
    );

    // A port with no imposter is a 404 error response, not a hang or panic.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:19484")
        .await
        .expect("bind listener");
    spawn_dispatch_listener(listener, manager.clone(), 1);
    let resp = reqwest::get("http://127.0.0.1:19484/ping")
        .await
        .expect("request");
    assert_eq!(resp.status(), 404, "no imposter on the target port");

    manager.delete_all().await;
}

// ===========================================================================
// Issue #342: bindable admin/metrics servers — bound-addr reporting + shutdown
// ===========================================================================

/// True once `addr` refuses TCP connections (listener gone), polled up to ~2s.
async fn connect_refused(addr: SocketAddr) -> bool {
    for _ in 0..40 {
        match tokio::time::timeout(
            Duration::from_millis(200),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        {
            Ok(Err(_)) => return true, // connection refused — listener is down
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    false
}

// AC1: bind on 127.0.0.1:0 → local_addr() reports the OS-assigned (nonzero) port and the
// server serves /health there. Fixes the #317 "a :0 bind gives the caller no way to learn
// the assigned port" gap.
#[tokio::test]
async fn admin_bind_zero_port_reports_addr_and_serves_health() {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .bind()
        .await
        .expect("bind admin on :0");

    let addr = running.local_addr();
    assert_ne!(addr.port(), 0, "local_addr must report the assigned port");

    wait_for_http(&format!("http://{addr}/health")).await;
    let resp = reqwest::get(format!("http://{addr}/health"))
        .await
        .expect("health reachable");
    assert_eq!(resp.status(), 200);

    running.shutdown().await;
}

// AC2: shutdown() stops accepting (subsequent connect refused) and returns within the grace
// bound; a second shutdown() is a no-op (idempotent).
#[tokio::test]
async fn admin_shutdown_stops_accepting_and_is_idempotent() {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .bind()
        .await
        .expect("bind admin on :0");
    let addr = running.local_addr();
    wait_for_http(&format!("http://{addr}/health")).await;

    // shutdown returns within a bounded grace (the API promises ~500ms; allow slack).
    tokio::time::timeout(Duration::from_secs(2), running.shutdown())
        .await
        .expect("shutdown returns within the grace bound");

    assert!(
        connect_refused(addr).await,
        "after shutdown the admin port must refuse connections"
    );

    // Double shutdown is a no-op — must not panic or hang.
    tokio::time::timeout(Duration::from_secs(2), running.shutdown())
        .await
        .expect("second shutdown is a no-op");
}

// AC3: join() returns once the accept loop has exited (driven here by a prior shutdown).
#[tokio::test]
async fn admin_join_returns_after_accept_loop_exits() {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .bind()
        .await
        .expect("bind admin on :0");
    let addr = running.local_addr();
    wait_for_http(&format!("http://{addr}/health")).await;

    running.shutdown().await; // stops the accept loop
    let joined = tokio::time::timeout(Duration::from_secs(2), running.join())
        .await
        .expect("join returns after the accept loop has exited");
    assert!(joined.is_ok(), "join reports the accept loop's Ok result");
}

// AC4: bind_metrics_server on :0 serves /metrics at the reported addr; shutdown stops it.
#[tokio::test]
async fn metrics_bind_zero_port_serves_and_shuts_down() {
    let running = bind_metrics_server("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind metrics on :0");
    let addr = running.local_addr();
    assert_ne!(
        addr.port(),
        0,
        "metrics local_addr must report the assigned port"
    );

    wait_for_http(&format!("http://{addr}/metrics")).await;
    // AC4 is "serves /metrics at the reported addr" — assert the HTTP contract (200 at the
    // :0-assigned port). Metric *content* ("rift_") depends on global-registry state and is
    // covered by `run_metrics_server_serves_metrics`.
    let resp = reqwest::get(format!("http://{addr}/metrics"))
        .await
        .expect("metrics reachable at the :0-assigned port");
    assert_eq!(resp.status(), 200);

    tokio::time::timeout(Duration::from_secs(2), running.shutdown())
        .await
        .expect("metrics shutdown within grace");
    assert!(
        connect_refused(addr).await,
        "after shutdown the metrics port must refuse connections"
    );
}

// AC5: ServerBuilder::start() returns once bound, reports both addrs (nonzero via :0), and
// its shutdown() stops BOTH listeners.
#[tokio::test]
async fn server_builder_start_reports_addrs_and_shuts_down_both() {
    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ])
    .expect("cli parse");

    let running = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("start returns once bound");

    let admin_addr = running.admin_addr();
    // Metrics binds 0.0.0.0; reach it over loopback for a portable client connection.
    let metrics_addr = SocketAddr::new(
        "127.0.0.1".parse().unwrap(),
        running.metrics_addr().expect("metrics addr present").port(),
    );
    assert_ne!(admin_addr.port(), 0);
    assert_ne!(metrics_addr.port(), 0);

    wait_for_http(&format!("http://{admin_addr}/health")).await;
    wait_for_http(&format!("http://{metrics_addr}/metrics")).await;

    tokio::time::timeout(Duration::from_secs(3), running.shutdown())
        .await
        .expect("shutdown both within grace");

    assert!(connect_refused(admin_addr).await, "admin listener stopped");
    assert!(
        connect_refused(metrics_addr).await,
        "metrics listener stopped"
    );
}

// Issue #19 / U-11: `--front-door` binds the front-door listener from `ServerBuilder::start`,
// seeded from the config file's `routes` block exactly like `--intercept-port`/the `intercept`
// block seed the intercept listener. A request through the front door's own bound address (never
// a hardcoded port — resolved via `front_door_addr()`) reaches the imposter the route names.
#[tokio::test]
async fn server_builder_start_binds_front_door_and_routes_to_imposter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    std::fs::write(
        &path,
        r#"{"imposters":[{"port":19490,"protocol":"http","stubs":[
            {"predicates":[{"equals":{"path":"/ping"}}],
             "responses":[{"is":{"statusCode":200,"body":"front-door-pong"}}]}]}],
           "routes":{"routes":[
             {"id":"pay","match":{"host":"payments.test"},"target":{"port":19490}}
           ]}}"#,
    )
    .expect("write config");

    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--configfile",
        path.to_str().expect("utf8 path"),
        "--front-door",
        "127.0.0.1:0",
    ])
    .expect("cli parse");

    let running = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("start binds the front door alongside admin/metrics");

    let front_door_addr = running
        .front_door_addr()
        .expect("front door bound when --front-door is set");
    assert_ne!(
        front_door_addr.port(),
        0,
        "front_door_addr() resolves the :0 request to the real bound port"
    );

    wait_for_http(&format!("http://{}/health", running.admin_addr())).await;
    wait_for_http(&format!("http://{front_door_addr}/ping")).await;

    let resp = reqwest::Client::new()
        .get(format!("http://{front_door_addr}/ping"))
        .header("Host", "payments.test")
        .send()
        .await
        .expect("front door request");
    assert_eq!(resp.status(), 200, "the routed imposter answered");
    assert_eq!(
        resp.text().await.expect("body"),
        "front-door-pong",
        "the request reached imposter 19490 via the route, not the gateway fallback"
    );

    running.shutdown().await;
}

// Without `--front-door`, nothing about startup changes: the listener stays off.
#[tokio::test]
async fn server_builder_start_leaves_front_door_off_without_the_flag() {
    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ])
    .expect("cli parse");
    let running = ServerBuilder::from_cli(cli).start().await.expect("start");
    assert!(
        running.front_door_addr().is_none(),
        "front door must stay off unless --front-door is set"
    );
    running.shutdown().await;
}

// AC7 end-to-end: `--allow-injection` must survive the whole thread (ServerBuilder::start →
// with_allow_injection → accept_loop → route_request → route_by_path → handle_config) and be
// reported by GET /config. A hardcoded flag anywhere on that path would fail here while the
// unit test still passed.
#[tokio::test]
async fn server_builder_start_threads_allow_injection_to_config() {
    async fn allow_injection_reported(extra: &[&str]) -> bool {
        let mut args = vec![
            "rift",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--metrics-port",
            "0",
        ];
        args.extend_from_slice(extra);
        let cli = Cli::try_parse_from(args).expect("cli parse");
        let running = ServerBuilder::from_cli(cli).start().await.expect("start");
        let admin = running.admin_addr();
        wait_for_http(&format!("http://{admin}/health")).await;
        let cfg: serde_json::Value = reqwest::get(format!("http://{admin}/config"))
            .await
            .expect("config reachable")
            .json()
            .await
            .expect("json");
        running.shutdown().await;
        cfg["options"]["allowInjection"]
            .as_bool()
            .expect("allowInjection bool")
    }

    assert!(
        allow_injection_reported(&["--allow-injection"]).await,
        "--allow-injection must reach GET /config"
    );
    assert!(
        !allow_injection_reported(&[]).await,
        "without the flag, /config must report allowInjection=false"
    );
}

// AC5 degradation: a metrics-port bind failure is non-fatal — the admin plane still comes up,
// metrics_addr() is None, and shutdown() handles the None branch without panicking.
#[tokio::test]
async fn server_builder_start_survives_metrics_bind_failure() {
    // Occupy 0.0.0.0:<port> — the same wildcard address the metrics server binds — so the
    // metrics bind deterministically collides (EADDRINUSE) on every platform.
    let occupied = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("occupy a port");
    let busy_port = occupied.local_addr().expect("addr").port();

    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        &busy_port.to_string(),
    ])
    .expect("cli parse");

    let running = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("admin plane still starts despite a metrics bind failure");

    assert_ne!(running.admin_addr().port(), 0);
    assert!(
        running.metrics_addr().is_none(),
        "a failed metrics bind degrades to None, not a hard error"
    );
    wait_for_http(&format!("http://{}/health", running.admin_addr())).await;

    // shutdown must not panic on the None metrics branch.
    tokio::time::timeout(Duration::from_secs(2), running.shutdown())
        .await
        .expect("shutdown handles the None metrics branch");
}

// Issue #806: `join`/`shutdown` both consumed `self`, so an embedder could await the server OR
// await a shutdown signal, never race them. `wait(&self)` is the seam; these cover both arms of
// that race plus the abort path, which is what makes the seam safe rather than merely present.

// AC1: wait() returns once the server is shut down from a *different* task — the borrow, not the
// value, is what wait() holds, so the shutdown side is still reachable while wait() is in flight.
#[tokio::test]
async fn running_server_wait_completes_when_shutdown_from_another_task() {
    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ])
    .expect("cli parse");
    let running = Arc::new(
        ServerBuilder::from_cli(cli)
            .start()
            .await
            .expect("start returns once bound"),
    );
    wait_for_http(&format!("http://{}/health", running.admin_addr())).await;

    let stopper = Arc::clone(&running);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stopper.shutdown().await;
    });

    let waited = tokio::time::timeout(Duration::from_secs(5), running.wait())
        .await
        .expect("wait returns once the admin accept loop exits");
    assert!(waited.is_ok(), "a shutdown-driven exit is not an error");
}

// AC2: the rift-enterprise pattern — race wait() against a termination signal. The signal wins,
// and the arm that wins can still call shutdown(): proof the select! borrow has ended.
#[tokio::test]
async fn running_server_wait_loses_race_to_signal_then_shuts_down() {
    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ])
    .expect("cli parse");
    let running = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("start returns once bound");
    let addr = running.admin_addr();
    wait_for_http(&format!("http://{addr}/health")).await;

    let (signal_tx, signal_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = signal_tx.send(());
    });

    let admin_died = tokio::select! {
        result = running.wait() => Some(result),
        _ = signal_rx => None,
    };
    assert!(
        admin_died.is_none(),
        "the signal must win this race; the admin plane was healthy"
    );

    // The whole point of the issue: after racing wait(), the server is still owned and shutdownable.
    tokio::time::timeout(Duration::from_secs(5), running.shutdown())
        .await
        .expect("shutdown is still callable after wait() was raced");
    assert!(
        connect_refused(addr).await,
        "after shutdown the admin port must refuse connections"
    );
}

// AC3: the other arm of the same race — the admin plane exits first, so wait() wins and the
// embedder learns the server is gone instead of hanging on a signal that never comes.
#[tokio::test]
async fn running_server_wait_wins_race_when_admin_plane_exits() {
    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ])
    .expect("cli parse");
    let running = Arc::new(
        ServerBuilder::from_cli(cli)
            .start()
            .await
            .expect("start returns once bound"),
    );
    wait_for_http(&format!("http://{}/health", running.admin_addr())).await;

    let stopper = Arc::clone(&running);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stopper.shutdown().await;
    });

    // A signal that never fires: only the admin plane's exit can resolve this select.
    let (_signal_tx, signal_rx) = tokio::sync::oneshot::channel::<()>();
    let admin_result = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = running.wait() => Some(result),
            _ = signal_rx => None,
        }
    })
    .await
    .expect("the admin-plane arm must resolve the race");
    assert!(
        admin_result.is_some_and(|r| r.is_ok()),
        "wait() wins the race and reports the accept loop's outcome"
    );
}

// AC4: wait() is safe to call after the server has already stopped — it returns immediately rather
// than blocking on a task that will never complete. (The abort-on-grace-timeout and panicking-loop
// interleavings need a wedged loop, so they live in the white-box `wait_seam_tests` module beside
// the implementation.)
#[tokio::test]
async fn running_server_wait_after_shutdown_returns_immediately() {
    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ])
    .expect("cli parse");
    let running = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("start returns once bound");
    wait_for_http(&format!("http://{}/health", running.admin_addr())).await;

    running.shutdown().await;

    for _ in 0..2 {
        let waited = tokio::time::timeout(Duration::from_secs(2), running.wait())
            .await
            .expect("wait after shutdown returns immediately");
        assert!(waited.is_ok(), "a post-shutdown wait reports Ok");
    }
}

// AC5: the same seam one layer down, where the accept loop actually lives.
#[tokio::test]
async fn admin_wait_returns_after_shutdown() {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .bind()
        .await
        .expect("bind admin on :0");
    let addr = running.local_addr();
    wait_for_http(&format!("http://{addr}/health")).await;

    running.shutdown().await;
    let waited = tokio::time::timeout(Duration::from_secs(2), running.wait())
        .await
        .expect("admin wait returns after the accept loop exits");
    assert!(waited.is_ok(), "wait reports the accept loop's Ok result");
}

// Issue #844: `--api-key ""` used to switch the auth gate ON and then authenticate every anonymous
// request, so the admin plane reported as protected while being fully open. It must now be a loud
// startup failure instead.
//
// Driven through `ServerBuilder::start` rather than the validator directly, because the property
// that matters is that a real startup refuses: an empty value arrives here from ordinary config
// plumbing (a Helm value that renders empty, `MB_APIKEY=` unset-but-defaulted, an SDK passing
// `getProperty("rift.apikey", "")`), not from someone typing `--api-key ""` on purpose.
#[tokio::test]
async fn server_builder_rejects_a_blank_api_key() {
    for blank in ["", "   "] {
        let cli = Cli::try_parse_from([
            "rift",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--api-key",
            blank,
        ])
        .expect("cli parse");

        let err = match ServerBuilder::from_cli(cli).start().await {
            Ok(_) => {
                panic!("a blank --api-key must fail startup, not silently authenticate everyone")
            }
            Err(e) => format!("{e:#}"),
        };
        assert!(
            err.contains("--api-key"),
            "the failure must name the flag an operator would fix, got: {err}"
        );
    }
}

// Issue #844 AC4: the same path with a real key still starts and still enforces the key — the fix
// must reject blank keys without weakening the working configuration.
#[tokio::test]
async fn server_builder_still_accepts_and_enforces_a_real_api_key() {
    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "12614",
        "--metrics-port",
        "19491",
        "--api-key",
        "s3cret-token",
    ])
    .expect("cli parse");

    tokio::spawn(ServerBuilder::from_cli(cli).run());
    // Readiness only: `wait_for_http` needs *any* HTTP response, and with a key configured this
    // probe gets a 401 — `/health` is behind the auth gate like everything except the `/__rift/`
    // gateway. That 401 is itself evidence the gate is on; the assertions below are what test it.
    wait_for_http("http://127.0.0.1:12614/health").await;

    let client = reqwest::Client::new();
    let anonymous = client
        .get("http://127.0.0.1:12614/imposters")
        .send()
        .await
        .expect("request");
    assert_eq!(
        anonymous.status(),
        401,
        "an anonymous request must still be rejected when a real key is configured"
    );

    let authorized = client
        .get("http://127.0.0.1:12614/imposters")
        .header("authorization", "s3cret-token")
        .send()
        .await
        .expect("request");
    assert_eq!(
        authorized.status(),
        200,
        "the configured key must still authenticate"
    );
}

// Issue #844: the third door onto the admin server — an embedder constructing `AdminApiServer`
// directly, as documented in docs/embedding/server.md. Without a check here a blank key is caught
// only by `api_key_matches` failing closed: safe, but the embedder gets a server that 401s
// everything while the startup log says authentication is enabled. `bind()` is where all three
// doors converge, so the diagnosis is loud wherever the key came from.
#[tokio::test]
async fn admin_api_server_bind_rejects_a_blank_api_key() {
    let manager = Arc::new(ImposterManager::new());
    let addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");

    let err = match AdminApiServer::new(addr, manager.clone(), Some(String::new()))
        .bind()
        .await
    {
        Ok(_) => panic!("a blank api key must fail bind, not serve a deny-all admin plane"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        err.contains("api-key"),
        "the failure must name the option to fix, got: {err}"
    );

    // AC4 at this door too: a real key still binds and still serves.
    let running = AdminApiServer::new(addr, manager, Some("s3cret-token".to_string()))
        .bind()
        .await
        .expect("a real key must bind");
    running.shutdown().await;
}
