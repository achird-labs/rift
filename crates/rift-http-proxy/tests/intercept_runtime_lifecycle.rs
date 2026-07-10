//! Runtime intercept lifecycle over the admin API (issue #493): `POST`/`GET`/`DELETE /intercept`
//! start, report, and stop the TLS-MITM listener at runtime — so the connect transport can enable
//! intercept without the server having been started with `--intercept-port`.
//!
//! Covers acceptance criteria 1–7. AC8 (FFI parity) lives in `crates/rift-ffi/tests/round_trip.rs`.

use clap::Parser;
use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use rift_http_proxy::intercept_control::InterceptControl;
use rift_http_proxy::server::{Cli, ServerBuilder};
use std::sync::Arc;

/// Bind an admin server with the intercept surface wired to a fresh (empty) control — the connect
/// transport's "server started without `--intercept-port`" case. Returns the base URL + the handle.
async fn admin_without_intercept_flag() -> (String, rift_http_proxy::admin_api::RunningAdminApi) {
    let manager = Arc::new(ImposterManager::new());
    let admin = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .with_intercept(InterceptControl::default())
        .bind()
        .await
        .expect("admin binds");
    let base = format!("http://{}", admin.local_addr());
    (base, admin)
}

/// A reqwest client that proxies HTTPS through `intercept_url` and trusts only `ca_pem` — the SUT's
/// view of the intercept proxy.
fn trusting_client(intercept_url: &str, ca_pem: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(intercept_url).unwrap())
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
        .build()
        .unwrap()
}

/// AC1–4, 6: full lifecycle on a server started WITHOUT `--intercept-port`.
#[tokio::test]
async fn lifecycle_on_server_started_without_flag() {
    let (base, admin) = admin_without_intercept_flag().await;
    let c = reqwest::Client::new();

    // AC1: GET before start → 404.
    assert_eq!(
        c.get(format!("{base}/intercept"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    // Rule/CA routes 404 while no listener runs.
    assert_eq!(
        c.get(format!("{base}/intercept/ca.pem"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // AC1: POST empty body → 201 with an OS-assigned port.
    let started = c.post(format!("{base}/intercept")).send().await.unwrap();
    assert_eq!(started.status(), 201);
    let body: serde_json::Value = started.json().await.unwrap();
    let port = body["interceptPort"].as_u64().expect("interceptPort") as u16;
    assert!(port > 0);
    let intercept_url = body["interceptUrl"].as_str().unwrap().to_string();
    assert_eq!(intercept_url, format!("http://127.0.0.1:{port}"));

    // AC1: GET now → 200, same body.
    let got = c.get(format!("{base}/intercept")).send().await.unwrap();
    assert_eq!(got.status(), 200);
    let got_body: serde_json::Value = got.json().await.unwrap();
    assert_eq!(got_body["interceptPort"].as_u64().unwrap() as u16, port);

    // AC1: /intercept/rules + /intercept/ca.pem now work, and interception is end-to-end.
    let rule =
        r#"{"host":"cdn.example.com","action":{"serve":{"statusCode":418,"body":"runtime-brew"}}}"#;
    assert_eq!(
        c.post(format!("{base}/intercept/rules"))
            .body(rule)
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    let ca_resp = c
        .get(format!("{base}/intercept/ca.pem"))
        .send()
        .await
        .unwrap();
    assert_eq!(ca_resp.status(), 200);
    let ca_pem = ca_resp.text().await.unwrap();
    assert!(ca_pem.starts_with("-----BEGIN CERTIFICATE-----"));

    let sut = trusting_client(&intercept_url, &ca_pem);
    let intercepted = sut
        .get("https://cdn.example.com/x")
        .send()
        .await
        .expect("intercepted");
    assert_eq!(intercepted.status(), 418);
    assert_eq!(intercepted.text().await.unwrap(), "runtime-brew");

    // AC2: POST while running → 409 with the standard error envelope.
    let conflict = c.post(format!("{base}/intercept")).send().await.unwrap();
    assert_eq!(conflict.status(), 409);
    let env: serde_json::Value = conflict.json().await.unwrap();
    assert!(
        env["errors"][0]["message"].is_string(),
        "standard error envelope"
    );

    // AC6: unknown option field / half CA pair → 400.
    assert_eq!(
        c.post(format!("{base}/intercept"))
            .body(r#"{"nope":1}"#)
            .send()
            .await
            .unwrap()
            .status(),
        400,
        "unknown field is a 400 (a fresh POST after the 409 above still sees a running listener; \
         but a malformed body is rejected before the already-running check)"
    );

    // AC3: DELETE → 204; afterwards GET → 404, rules → 404, and the port is released.
    assert_eq!(
        c.delete(format!("{base}/intercept"))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(
        c.get(format!("{base}/intercept"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let rules_404 = c
        .get(format!("{base}/intercept/rules"))
        .send()
        .await
        .unwrap();
    assert_eq!(rules_404.status(), 404);
    // Finding: the sub-route gives an actionable body, not a bare "Not Found".
    let rules_404_env: serde_json::Value = rules_404.json().await.unwrap();
    assert_eq!(
        rules_404_env["errors"][0]["message"].as_str(),
        Some("intercept listener not running")
    );

    // AC3: a new POST pinning the just-freed port succeeds (port actually released).
    let repin = c
        .post(format!("{base}/intercept"))
        .body(format!(r#"{{"port":{port}}}"#))
        .send()
        .await
        .unwrap();
    assert_eq!(repin.status(), 201, "the released port can be rebound");
    let repin_body: serde_json::Value = repin.json().await.unwrap();
    assert_eq!(repin_body["interceptPort"].as_u64().unwrap() as u16, port);

    // AC4: DELETE, then DELETE again with nothing running → both 204 (idempotent).
    assert_eq!(
        c.delete(format!("{base}/intercept"))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(
        c.delete(format!("{base}/intercept"))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );

    admin.shutdown().await;
}

/// AC6: a half-supplied CA pair is a 400 (validated before binding).
#[tokio::test]
async fn start_with_half_ca_pair_is_bad_request() {
    let (base, admin) = admin_without_intercept_flag().await;
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/intercept"))
        .body(r#"{"caCertPath":"only-cert.pem"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    // No listener was left behind.
    assert_eq!(
        c.get(format!("{base}/intercept"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    admin.shutdown().await;
}

/// AC6: a bind failure (port already in use) is a 400 through the admin handler, not a 500 or a
/// half-started listener.
#[tokio::test]
async fn start_on_occupied_port_is_bad_request() {
    let (base, admin) = admin_without_intercept_flag().await;
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = occupied.local_addr().unwrap().port();
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/intercept"))
        .body(format!(r#"{{"port":{port}}}"#))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "occupied port → 400");
    // Nothing was installed.
    assert_eq!(
        c.get(format!("{base}/intercept"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    admin.shutdown().await;
}

/// AC5: on a server started WITH `--intercept-port`, the lifecycle endpoints see and manage it, and
/// shutdown afterwards is clean (no double-stop panic).
#[tokio::test]
async fn lifecycle_on_server_started_with_flag() {
    let cli = Cli::parse_from([
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--intercept-port",
        "0",
    ]);
    let server = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    let base = format!("http://{admin}");
    let flag_port = server.intercept_addr().expect("flag listener bound").port();
    let c = reqwest::Client::new();

    // GET reports the flag-started listener (the shared slot's whole point).
    let got = c.get(format!("{base}/intercept")).send().await.unwrap();
    assert_eq!(got.status(), 200);
    let got_body: serde_json::Value = got.json().await.unwrap();
    assert_eq!(
        got_body["interceptPort"].as_u64().unwrap() as u16,
        flag_port
    );

    // POST → 409 (already running).
    assert_eq!(
        c.post(format!("{base}/intercept"))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );

    // DELETE stops it; GET → 404.
    assert_eq!(
        c.delete(format!("{base}/intercept"))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(
        c.get(format!("{base}/intercept"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // shutdown() must not double-stop the (already-stopped) listener.
    server.shutdown().await;
}

/// AC7: with `--apikey`, all three lifecycle verbs reject a missing/wrong Authorization with 401.
#[tokio::test]
async fn lifecycle_endpoints_are_apikey_gated() {
    let manager = Arc::new(ImposterManager::new());
    let admin = AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager,
        Some("s3cret".to_string()),
    )
    .with_intercept(InterceptControl::default())
    .bind()
    .await
    .expect("admin binds");
    let base = format!("http://{}", admin.local_addr());
    let c = reqwest::Client::new();

    for (method, url) in [
        ("POST", format!("{base}/intercept")),
        ("GET", format!("{base}/intercept")),
        ("DELETE", format!("{base}/intercept")),
    ] {
        let req = |auth: Option<&str>| {
            let mut r = match method {
                "POST" => c.post(&url),
                "GET" => c.get(&url),
                _ => c.delete(&url),
            };
            if let Some(a) = auth {
                r = r.header("Authorization", a);
            }
            r
        };
        assert_eq!(
            req(None).send().await.unwrap().status(),
            401,
            "{method} {url} without a key must be 401"
        );
        assert_eq!(
            req(Some("wrong")).send().await.unwrap().status(),
            401,
            "{method} {url} with a wrong key must be 401"
        );
    }

    // The correct key gets through (POST → 201, then a clean DELETE).
    assert_eq!(
        c.post(format!("{base}/intercept"))
            .header("Authorization", "s3cret")
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    assert_eq!(
        c.delete(format!("{base}/intercept"))
            .header("Authorization", "s3cret")
            .send()
            .await
            .unwrap()
            .status(),
        204
    );

    admin.shutdown().await;
}
