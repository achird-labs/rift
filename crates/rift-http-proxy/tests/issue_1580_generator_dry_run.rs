//! Issue #1580: a `predicateGenerators` `inject` is dry-run when its stub is created, as in
//! Mountebank's `dryRunValidator`. One that throws or returns a non-array over the synthetic
//! `GET /` is refused at every admin door with `400 invalid injection`, and a `--configfile`
//! holding one fails to start. A script guarded on `config.request.isDryRun` is admitted.

use clap::Parser;
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use serde_json::{Value, json};

const THROWS: &str = "function (config) { throw new Error('boom'); }";
const GUARDED: &str = "function (config) { if (config.request.isDryRun) { return []; } \
                       return [{ equals: JSON.parse(config.request.body) }]; }";

fn proxy_stub(inject: &str) -> Value {
    json!({ "responses": [{ "proxy": { "to": "http://127.0.0.1:1", "mode": "proxyOnce",
            "predicateGenerators": [{ "inject": inject }] } }] })
}

async fn start(allow_injection: bool) -> (RunningServer, String) {
    let mut argv = vec![
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ];
    if allow_injection {
        argv.push("--allow-injection");
    }
    let server = ServerBuilder::from_cli(Cli::try_parse_from(argv).expect("cli parse"))
        .start()
        .await
        .expect("server starts");
    let admin = format!("http://{}", server.admin_addr());
    (server, admin)
}

async fn send(method: reqwest::Method, url: &str, body: &Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .request(method, url)
        .json(body)
        .send()
        .await
        .expect("admin request");
    let status = resp.status().as_u16();
    let text = resp.text().await.expect("body");
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn clean_imposter(admin: &str) -> u16 {
    let (status, body) = send(
        reqwest::Method::POST,
        &format!("{admin}/imposters"),
        &json!({ "protocol": "http", "stubs": [] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    u16::try_from(body["port"].as_u64().expect("port")).expect("u16 port")
}

/// The four doors the issue names, as (label, method, path with `{port}`, body).
fn doors(stub: &Value) -> Vec<(&'static str, reqwest::Method, &'static str, Value)> {
    use reqwest::Method;
    vec![
        (
            "POST /imposters",
            Method::POST,
            "/imposters",
            json!({ "protocol": "http", "stubs": [stub] }),
        ),
        (
            "PUT /imposters",
            Method::PUT,
            "/imposters",
            json!({ "imposters": [{ "protocol": "http", "stubs": [stub] }] }),
        ),
        (
            "POST /imposters/{port}/stubs",
            Method::POST,
            "/imposters/{port}/stubs",
            json!({ "stub": stub }),
        ),
        (
            "PUT /imposters/{port}/stubs",
            Method::PUT,
            "/imposters/{port}/stubs",
            json!({ "stubs": [stub] }),
        ),
    ]
}

#[tokio::test]
async fn a_throwing_generator_inject_is_refused_at_every_door_with_its_source_and_message() {
    let (server, admin) = start(true).await;
    for (door, method, path, body) in doors(&proxy_stub(THROWS)) {
        let port = clean_imposter(&admin).await;
        let url = format!("{admin}{}", path.replace("{port}", &port.to_string()));
        let (status, resp) = send(method, &url, &body).await;
        let error = &resp["errors"][0];
        assert_eq!(status, 400, "{door}: {resp}");
        assert_eq!(error["code"], "invalid injection", "{door}: {resp}");
        assert_eq!(
            error["message"], "invalid predicateGenerator injection",
            "{door}: {resp}"
        );
        assert_eq!(
            error["source"],
            format!("({THROWS})(config);"),
            "{door}: {resp}"
        );
        assert!(
            error["data"].as_str().is_some_and(|d| d.contains("boom")),
            "{door}: {resp}"
        );
    }
    server.shutdown().await;
}

#[tokio::test]
async fn a_generator_inject_guarded_on_is_dry_run_is_admitted_at_every_door() {
    let (server, admin) = start(true).await;
    for (door, method, path, body) in doors(&proxy_stub(GUARDED)) {
        let port = clean_imposter(&admin).await;
        let url = format!("{admin}{}", path.replace("{port}", &port.to_string()));
        let (status, resp) = send(method, &url, &body).await;
        assert!(status == 200 || status == 201, "{door}: {status} {resp}");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn without_the_flag_the_injection_gate_answers_not_the_dry_run() {
    let (server, admin) = start(false).await;
    let (status, resp) = send(
        reqwest::Method::POST,
        &format!("{admin}/imposters"),
        &json!({ "protocol": "http", "stubs": [proxy_stub(THROWS)] }),
    )
    .await;
    assert_eq!(status, 400, "{resp}");
    let message = resp["errors"][0]["message"].as_str().unwrap_or_default();
    assert!(message.contains("--allowInjection"), "{resp}");
    assert!(resp["errors"][0].get("source").is_none(), "{resp}");
    server.shutdown().await;
}

#[tokio::test]
async fn a_configfile_with_a_throwing_generator_inject_fails_to_start_naming_the_port() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let config = json!({ "imposters": [{ "port": 45801, "protocol": "http",
                                         "stubs": [proxy_stub(THROWS)] }] });
    std::fs::write(&path, config.to_string()).expect("write config");
    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--allow-injection",
        "--configfile",
        path.to_str().expect("utf8 path"),
    ])
    .expect("cli parse");
    let err = ServerBuilder::from_cli(cli)
        .start()
        .await
        .err()
        .expect("a throwing generator inject must abort startup");
    let message = format!("{err:#}");
    assert!(message.contains("port 45801"), "{message}");
    assert!(
        message.contains("invalid predicateGenerator injection"),
        "{message}"
    );
    assert!(message.contains("boom"), "{message}");
}
