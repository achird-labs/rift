//! CLI wiring for the intercept listener (epic #394, issue #406): `--intercept-port` starts the
//! forward-proxy from the standalone binary and shares its rule store + CA with the admin API.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};

#[tokio::test]
async fn intercept_port_flag_starts_listener_wired_to_admin() {
    // Start the server the way the `rift` binary does, but on ephemeral ports.
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
        .expect("server starts with intercept enabled");

    let admin = server.admin_addr();
    let intercept = server
        .intercept_addr()
        .expect("intercept listener bound when --intercept-port is set");

    // The admin API exposes the CA (proving the shared InterceptState is wired in).
    let ca_pem = reqwest::get(format!("http://{admin}/intercept/ca.pem"))
        .await
        .expect("ca.pem request")
        .text()
        .await
        .unwrap();
    assert!(ca_pem.starts_with("-----BEGIN CERTIFICATE-----"));

    // Configure a rule through the admin API (same store the listener matches against).
    let rule =
        r#"{"host":"cdn.example.com","action":{"serve":{"statusCode":418,"body":"cli-brew"}}}"#;
    let created = reqwest::Client::new()
        .post(format!("http://{admin}/intercept/rules"))
        .body(rule)
        .send()
        .await
        .expect("add rule");
    assert_eq!(created.status(), 201);

    // A client trusting the CA and proxying HTTPS through the intercept port gets the stub.
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(format!("http://{intercept}")).unwrap())
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
        .build()
        .unwrap();
    let resp = client
        .get("https://cdn.example.com/config.json")
        .send()
        .await
        .expect("intercepted via CLI-started listener");
    assert_eq!(resp.status(), 418);
    assert_eq!(resp.text().await.unwrap(), "cli-brew");

    server.shutdown().await;
}

#[tokio::test]
async fn no_intercept_flag_leaves_listener_off() {
    let cli = Cli::parse_from(["rift", "--local-only", "--port", "0", "--metrics-port", "0"]);
    let server = ServerBuilder::from_cli(cli).start().await.expect("start");
    assert!(
        server.intercept_addr().is_none(),
        "intercept listener must stay off unless --intercept-port is set"
    );
    server.shutdown().await;
}
