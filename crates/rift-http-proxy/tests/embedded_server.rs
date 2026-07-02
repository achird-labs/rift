//! Issue #317: the server is embeddable — `ServerBuilder`, `run_metrics_server`, and
//! `dispatch_to_port` are library functions a custom binary can compose around its own
//! `ImposterManager`.

use clap::Parser;
use rift_http_proxy::gateway::dispatch_to_port;
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use rift_http_proxy::server::{Cli, ServerBuilder, run_metrics_server};
use std::sync::Arc;

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

// AC2: run_metrics_server on an explicit loopback SocketAddr serves /metrics (and 404s
// elsewhere). Fixed port rather than :0 — the function never returns, so a :0 bind gives
// the caller no way to learn the assigned port.
#[tokio::test]
async fn run_metrics_server_serves_metrics() {
    let addr: std::net::SocketAddr = "127.0.0.1:19482".parse().expect("addr");
    tokio::spawn(run_metrics_server(addr));
    wait_for_http("http://127.0.0.1:19482/metrics").await;

    let resp = reqwest::get("http://127.0.0.1:19482/metrics")
        .await
        .expect("metrics reachable");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("rift_"),
        "expected rift metrics in body, got: {body:.200}"
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
