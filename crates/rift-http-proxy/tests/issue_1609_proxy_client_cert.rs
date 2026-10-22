//! Issue #1609: a `proxy` response presents `cert`/`key` (and decrypts `key` with `passphrase`) to
//! an upstream that requires a client certificate, as Mountebank's proxy hands them to Node's
//! `https.request`. Rift used to drop the three keys on load, so every request to an mTLS origin
//! failed the handshake with `CertificateRequired`.
//!
//! These run the real binary, so the `--upstream-ca-file` wiring (`server.rs`) is covered too: the
//! identity client must trust what the operator's plain proxy client trusts. Every origin and proxy
//! is a Rift imposter on an assigned port (no fixed test port, see `test_port_uniqueness`).

mod support;

use std::path::Path;
use std::time::Duration;

const CA: &str = include_str!("fixtures/client-identity/ca.crt");
const CLIENT_CERT: &str = include_str!("fixtures/client-identity/client.crt");
const CLIENT_KEY: &str = include_str!("fixtures/client-identity/client.key");
const CLIENT_KEY_PBES2: &str = include_str!("fixtures/client-identity/client.pbes2.key");
const PASSPHRASE: &str = "rift-test";
// Issue #1599's X.509 v1 client pair: rustls' own `keys_match` refuses v1.
const V1_CLIENT_CERT: &str = include_str!("fixtures/x509-v1/client.crt");
const V1_CLIENT_KEY: &str = include_str!("fixtures/x509-v1/client.key");

struct Origin {
    cert: String,
    key: String,
}

/// A server certificate for `localhost`, which the proxying server is told to trust.
fn origin_identity() -> Origin {
    let c = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("origin certificate");
    Origin {
        cert: c.cert.pem(),
        key: c.signing_key.serialize_pem(),
    }
}

async fn start(dir: &Path, origin: &Origin, extra: &[&str]) -> (tokio::process::Child, String) {
    let ca_file = dir.join("origin-ca.pem");
    std::fs::write(&ca_file, &origin.cert).expect("write the upstream CA file");
    let mut args = vec![
        "--host",
        "127.0.0.1",
        "--metrics-port",
        "0",
        "--upstream-ca-file",
        ca_file.to_str().expect("utf-8"),
    ];
    args.extend_from_slice(extra);
    let (child, admin_port) = support::spawn_server(&args).await;
    (child, format!("http://127.0.0.1:{admin_port}"))
}

async fn create(admin: &str, imposter: serde_json::Value) -> (u16, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!("{admin}/imposters"))
        .json(&imposter)
        .send()
        .await
        .expect("POST /imposters");
    let status = response.status().as_u16();
    let body: serde_json::Value = response.json().await.expect("json body");
    (status, body)
}

async fn create_ok(admin: &str, imposter: serde_json::Value) -> u16 {
    let (status, body) = create(admin, imposter).await;
    assert_eq!(status, 201, "imposter creation must succeed: {body}");
    body["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .expect("assigned port")
}

/// An https origin answering `origin`; `client_auth` is its `mutualAuth`/`rejectUnauthorized`/`ca`.
async fn origin_imposter(admin: &str, origin: &Origin, client_auth: serde_json::Value) -> u16 {
    let mut imposter = serde_json::json!({
        "protocol": "https",
        "key": origin.key,
        "cert": origin.cert,
        "stubs": [{"responses": [{"is": {"body": "origin"}}]}],
    });
    for (k, v) in client_auth.as_object().expect("object") {
        imposter[k] = v.clone();
    }
    create_ok(admin, imposter).await
}

/// The strict origin: it asks for a client certificate and verifies it against [`CA`].
async fn strict_origin(admin: &str, origin: &Origin) -> u16 {
    origin_imposter(
        admin,
        origin,
        serde_json::json!({"mutualAuth": true, "rejectUnauthorized": true, "ca": [CA]}),
    )
    .await
}

fn proxy_imposter(origin_port: u16, identity: &serde_json::Value) -> serde_json::Value {
    let mut proxy = serde_json::json!({
        "to": format!("https://localhost:{origin_port}"),
        "mode": "proxyAlways",
    });
    for (k, v) in identity.as_object().expect("object") {
        proxy[k] = v.clone();
    }
    serde_json::json!({"protocol": "http", "stubs": [{"responses": [{"proxy": proxy}]}]})
}

async fn through(port: u16) -> (u16, String) {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
        .get(format!("http://127.0.0.1:{port}/x"))
        .send()
        .await
        .expect("the proxy imposter answers");
    let status = response.status().as_u16();
    (status, response.text().await.unwrap_or_default())
}

fn identity() -> serde_json::Value {
    serde_json::json!({"cert": CLIENT_CERT, "key": CLIENT_KEY})
}

#[tokio::test]
async fn a_proxy_presents_its_client_certificate_to_an_mtls_origin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let origin = origin_identity();
    let (_server, admin) = start(dir.path(), &origin, &[]).await;
    let origin_port = strict_origin(&admin, &origin).await;

    let port = create_ok(&admin, proxy_imposter(origin_port, &identity())).await;
    assert_eq!(through(port).await, (200, "origin".to_string()));
}

#[tokio::test]
async fn without_an_identity_the_mtls_origin_refuses_the_proxy() {
    // Pins that the default stays "no client certificate": the 200 above is the identity's doing.
    let dir = tempfile::tempdir().expect("tempdir");
    let origin = origin_identity();
    let (_server, admin) = start(dir.path(), &origin, &[]).await;
    let origin_port = strict_origin(&admin, &origin).await;

    let port = create_ok(&admin, proxy_imposter(origin_port, &serde_json::json!({}))).await;
    assert_eq!(through(port).await.0, 502);
}

#[tokio::test]
async fn an_identity_the_origin_does_not_trust_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let origin = origin_identity();
    let (_server, admin) = start(dir.path(), &origin, &[]).await;
    let origin_port = strict_origin(&admin, &origin).await;

    let stranger = rcgen::generate_simple_self_signed(vec!["stranger".to_string()])
        .expect("an unrelated client certificate");
    let port = create_ok(
        &admin,
        proxy_imposter(
            origin_port,
            &serde_json::json!({"cert": stranger.cert.pem(), "key": stranger.signing_key.serialize_pem()}),
        ),
    )
    .await;
    assert_eq!(through(port).await.0, 502);
}

#[tokio::test]
async fn an_encrypted_key_is_decrypted_with_its_passphrase() {
    let dir = tempfile::tempdir().expect("tempdir");
    let origin = origin_identity();
    let (_server, admin) = start(dir.path(), &origin, &[]).await;
    let origin_port = strict_origin(&admin, &origin).await;

    let encrypted = serde_json::json!({
        "cert": CLIENT_CERT, "key": CLIENT_KEY_PBES2, "passphrase": PASSPHRASE,
    });
    let port = create_ok(&admin, proxy_imposter(origin_port, &encrypted)).await;
    assert_eq!(through(port).await, (200, "origin".to_string()));
}

#[tokio::test]
async fn an_x509_v1_client_certificate_is_presented() {
    // The mbTest fixture shape (#1599): a v1 certificate, to an origin with bare `mutualAuth`.
    let dir = tempfile::tempdir().expect("tempdir");
    let origin = origin_identity();
    let (_server, admin) = start(dir.path(), &origin, &[]).await;
    let origin_port =
        origin_imposter(&admin, &origin, serde_json::json!({"mutualAuth": true})).await;

    let v1 = serde_json::json!({"cert": V1_CLIENT_CERT, "key": V1_CLIENT_KEY});
    let port = create_ok(&admin, proxy_imposter(origin_port, &v1)).await;
    assert_eq!(through(port).await, (200, "origin".to_string()));
}

#[tokio::test]
async fn get_echoes_the_identity_as_mountebank_does() {
    let dir = tempfile::tempdir().expect("tempdir");
    let origin = origin_identity();
    let (_server, admin) = start(dir.path(), &origin, &[]).await;
    let encrypted = serde_json::json!({
        "cert": CLIENT_CERT, "key": CLIENT_KEY_PBES2, "passphrase": PASSPHRASE,
    });
    let port = create_ok(&admin, proxy_imposter(1, &encrypted)).await;

    let shown: serde_json::Value = reqwest::get(format!("{admin}/imposters/{port}"))
        .await
        .expect("GET imposter")
        .json()
        .await
        .expect("json");
    let proxy = &shown["stubs"][0]["responses"][0]["proxy"];
    assert_eq!(proxy["cert"], CLIENT_CERT, "{shown}");
    assert_eq!(proxy["key"], CLIENT_KEY_PBES2, "{shown}");
    assert_eq!(proxy["passphrase"], PASSPHRASE, "{shown}");
}

#[tokio::test]
async fn a_bad_identity_is_refused_at_creation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let origin = origin_identity();
    let (_server, admin) = start(dir.path(), &origin, &[]).await;
    for (bad, expected) in [
        (serde_json::json!({"cert": CLIENT_CERT}), "proxy.key"),
        (serde_json::json!({"key": CLIENT_KEY}), "proxy.cert"),
        (
            serde_json::json!({"cert": CLIENT_CERT, "key": CLIENT_KEY_PBES2, "passphrase": "wrong"}),
            "does not decrypt",
        ),
        (
            serde_json::json!({"cert": "not a pem", "key": CLIENT_KEY}),
            "no certificate",
        ),
    ] {
        let (status, body) = create(&admin, proxy_imposter(1, &bad)).await;
        assert_eq!(status, 400, "{bad} must be refused: {body}");
        let text = body.to_string();
        assert!(text.contains(expected), "{bad}: {text}");
        assert!(
            !text.contains("wrong\""),
            "the passphrase must not be echoed: {text}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_datadir_restart_keeps_the_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let datadir = dir.path().join("data");
    let datadir = datadir.to_str().expect("utf-8").to_string();
    let origin = origin_identity();

    let (mut server, admin) = start(dir.path(), &origin, &["--datadir", &datadir]).await;
    let origin_port = strict_origin(&admin, &origin).await;
    let port = create_ok(&admin, proxy_imposter(origin_port, &identity())).await;
    assert_eq!(through(port).await.0, 200);
    server.kill().await.expect("stop the first server");
    let _ = server.wait().await;

    let (_server, _admin) = start(dir.path(), &origin, &["--datadir", &datadir]).await;
    // The origin and the proxy come back on their saved ports; the identity must come back too.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let answered = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client")
            .get(format!("http://127.0.0.1:{port}/x"))
            .send()
            .await;
        if let Ok(response) = answered {
            assert_eq!(
                response.status().as_u16(),
                200,
                "the identity survived the restart"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the proxy imposter came back"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
