//! Issue #878: the intercept listener is a TLS-MITM forward proxy that had no authentication of any
//! kind, on a host defaulting to `0.0.0.0`. Anyone who could reach the port could route traffic
//! through it and be served forged certificates from Rift's CA — which, if that CA is installed
//! anywhere (the entire point of the feature), are trusted.
//!
//! Auth is **opt-in**: no credential configured behaves exactly as before, because the listener is
//! off unless asked for and most uses are loopback test rigs. `--require-admin-auth` (#863) is what
//! makes the exposed case fail closed.
//!
//! The credential is `Proxy-Authorization`, not the admin `Authorization` header: the former is
//! hop-by-hop and consumed here, the latter is end-to-end and would forward the admin API key to
//! every intercepted origin.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Reserve a port, then release it, so a later bind attempt proves nothing else claimed it.
fn free_port() -> u16 {
    let probe = StdTcpListener::bind("0.0.0.0:0").expect("probe bind");
    let port = probe.local_addr().expect("probe addr").port();
    drop(probe);
    port
}

/// Send a raw `CONNECT` and return the status line plus headers, without ever starting TLS.
///
/// Raw TCP rather than a proxy-aware client is deliberate: it is the only way to observe what the
/// proxy answers *before* a handshake, which is the ordering AC6 turns on.
async fn raw_connect(addr: SocketAddr, extra_headers: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to proxy");
    let req = format!(
        "CONNECT cdn.example.com:443 HTTP/1.1\r\nHost: cdn.example.com:443\r\n{extra_headers}\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .await
        .expect("write CONNECT");

    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    // Read until the header terminator or EOF; the proxy closes after a 407.
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn basic(user: &str, pass: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
}

async fn start_with(args: &[&str]) -> rift_http_proxy::server::RunningServer {
    let mut argv = vec!["rift", "--port", "0", "--metrics-port", "0"];
    argv.extend_from_slice(args);
    ServerBuilder::from_cli(Cli::parse_from(argv))
        .start()
        .await
        .expect("server starts")
}

// AC2: the compatibility guarantee. With no credential configured the listener behaves exactly as
// it always has — a regression here breaks every existing intercept user for a risk they may not
// have, which is why auth is opt-in rather than default-on.
#[tokio::test]
async fn without_a_credential_the_proxy_is_open_exactly_as_before() {
    let server = start_with(&["--local-only", "--intercept-port", "0"]).await;
    let intercept = server.intercept_addr().expect("intercept bound");

    let resp = raw_connect(intercept, "").await;
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "an unauthenticated proxy must still establish the tunnel, got: {resp:?}"
    );

    server.shutdown().await;
}

// AC3 + AC6. Two assertions, and the second is the load-bearing one: TLS cannot begin before the
// `200 Connection Established`, because the client only sends a ClientHello after it. So proving the
// response is a 407 and never a 200 proves no handshake ran — hence no leaf certificate was minted
// for an arbitrary SNI at the request of an unauthenticated caller. That ordering is the whole point
// of checking before the 200 rather than after.
#[tokio::test]
async fn a_missing_credential_is_407_before_any_certificate_is_minted() {
    let server = start_with(&[
        "--local-only",
        "--intercept-port",
        "0",
        "--intercept-auth",
        "u:p",
    ])
    .await;
    let intercept = server.intercept_addr().expect("intercept bound");

    let resp = raw_connect(intercept, "").await;
    assert!(
        resp.starts_with("HTTP/1.1 407"),
        "a missing credential must be refused, got: {resp:?}"
    );
    assert!(
        resp.to_ascii_lowercase()
            .contains("proxy-authenticate: basic"),
        "the 407 must carry a Proxy-Authenticate challenge, got: {resp:?}"
    );
    assert!(
        !resp.contains("200 Connection Established"),
        "the refusal must precede the tunnel, so no certificate is ever minted: {resp:?}"
    );

    server.shutdown().await;
}

// AC4: a wrong credential is refused like a missing one — and must not leak whether the username
// existed.
#[tokio::test]
async fn a_wrong_credential_is_407() {
    let server = start_with(&[
        "--local-only",
        "--intercept-port",
        "0",
        "--intercept-auth",
        "u:p",
    ])
    .await;
    let intercept = server.intercept_addr().expect("intercept bound");

    let header = format!("Proxy-Authorization: Basic {}\r\n", basic("u", "wrong"));
    let resp = raw_connect(intercept, &header).await;
    assert!(
        resp.starts_with("HTTP/1.1 407"),
        "a wrong credential must be refused, got: {resp:?}"
    );

    server.shutdown().await;
}

// A non-Basic scheme must not be waved through — a classifier that cannot read the credential
// treats it as absent, never as valid.
#[tokio::test]
async fn a_non_basic_scheme_is_407() {
    let server = start_with(&[
        "--local-only",
        "--intercept-port",
        "0",
        "--intercept-auth",
        "u:p",
    ])
    .await;
    let intercept = server.intercept_addr().expect("intercept bound");

    let resp = raw_connect(intercept, "Proxy-Authorization: Bearer abc123\r\n").await;
    assert!(
        resp.starts_with("HTTP/1.1 407"),
        "an unsupported scheme must be refused, not accepted: {resp:?}"
    );

    server.shutdown().await;
}

// AC5: the correct credential opens the tunnel, and the intercept feature still works end to end
// (a rule installed through the admin API is served over the authenticated tunnel).
#[tokio::test]
async fn the_correct_credential_opens_the_tunnel_and_rules_still_serve() {
    let server = start_with(&[
        "--local-only",
        "--intercept-port",
        "0",
        "--intercept-auth",
        "u:p",
    ])
    .await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");

    let ca_pem = reqwest::get(format!("http://{admin}/intercept/ca.pem"))
        .await
        .expect("ca.pem")
        .text()
        .await
        .expect("ca.pem body");

    let rule =
        r#"{"host":"cdn.example.com","action":{"serve":{"statusCode":418,"body":"authed"}}}"#;
    let created = reqwest::Client::new()
        .post(format!("http://{admin}/intercept/rules"))
        .body(rule)
        .send()
        .await
        .expect("add rule");
    assert_eq!(created.status(), 201);

    let client = reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::https(format!("http://{intercept}"))
                .expect("proxy")
                .basic_auth("u", "p"),
        )
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).expect("ca"))
        .build()
        .expect("client");

    let resp = client
        .get("https://cdn.example.com/config.json")
        .send()
        .await
        .expect("intercepted through an authenticated proxy");
    assert_eq!(resp.status(), 418);
    assert_eq!(resp.text().await.expect("body"), "authed");

    server.shutdown().await;
}

// AC7: #863's strict flag must cover this listener too. Without this the flag keeps under-delivering
// — an operator who set it would get an authenticated admin plane beside an open MITM proxy. The
// metrics-port probe mirrors #863's own test: the refusal must leave nothing bound.
#[tokio::test]
async fn require_admin_auth_refuses_an_exposed_intercept_with_no_credential() {
    let metrics_port = free_port();
    let cli = Cli::parse_from([
        "rift",
        "--host",
        "0.0.0.0",
        "--port",
        "0",
        "--metrics-port",
        &metrics_port.to_string(),
        "--api-key",
        "s3cr3t",
        "--require-admin-auth",
        "--intercept-port",
        "0",
    ]);
    let err =
        ServerBuilder::from_cli(cli).start().await.err().expect(
            "an exposed keyless intercept listener must be refused under --require-admin-auth",
        );
    let msg = err.to_string();
    assert!(
        msg.contains("--intercept-auth"),
        "the refusal must name the remedy, got: {msg}"
    );

    StdTcpListener::bind(SocketAddr::from(([0, 0, 0, 0], metrics_port))).unwrap_or_else(|e| {
        panic!("the refusal must precede any bind, but port {metrics_port} is held: {e}")
    });
}

// AC8: the same configuration with a credential starts — the flag gates on authentication, not on
// the address, exactly as it does for the admin plane.
#[tokio::test]
async fn require_admin_auth_accepts_an_exposed_intercept_with_a_credential() {
    let server = start_with(&[
        "--host",
        "0.0.0.0",
        "--api-key",
        "s3cr3t",
        "--require-admin-auth",
        "--intercept-port",
        "0",
        "--intercept-auth",
        "u:p",
    ])
    .await;
    assert!(server.intercept_addr().is_some());
    server.shutdown().await;
}

// AC9: a malformed `--intercept-auth` is a startup error, not a silently-disabled gate. An operator
// who wrote the value wrong must not end up with an open proxy believing it is closed.
#[tokio::test]
async fn a_malformed_intercept_auth_is_a_startup_error() {
    let cli = Cli::parse_from([
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--intercept-port",
        "0",
        "--intercept-auth",
        "no-colon-here",
    ]);
    let err = ServerBuilder::from_cli(cli)
        .start()
        .await
        .err()
        .expect("a value with no `:` must be rejected");
    assert!(
        err.to_string().contains("--intercept-auth"),
        "the error must name the flag, got: {err}"
    );
}

// AC1: the blank-credential rejection, which had no test at all until this one — the `.trim()`
// guard could have been deleted and every other test here would still have passed. It is the #844
// failure mode one listener over: a blank secret switches the gate on and then admits everyone,
// leaving an operator believing a credential is in force.
#[tokio::test]
async fn a_blank_half_of_the_credential_is_a_startup_error() {
    for bad in ["u:", ":p", ":", "u:   ", "   :p"] {
        let cli = Cli::parse_from([
            "rift",
            "--local-only",
            "--port",
            "0",
            "--metrics-port",
            "0",
            "--intercept-port",
            "0",
            "--intercept-auth",
            bad,
        ]);
        let err = ServerBuilder::from_cli(cli)
            .start()
            .await
            .err()
            .unwrap_or_else(|| panic!("`{bad}` must be rejected: a blank half admits everyone"));
        assert!(
            err.to_string().contains("--intercept-auth"),
            "the error must name the flag, got: {err}"
        );
    }
}

// A colon is legal *inside* the password — `split_once` takes the first one. Locked in because the
// obvious "simplification" (`split(':')` with two parts expected) silently breaks it.
#[tokio::test]
async fn a_password_may_contain_a_colon() {
    let server = start_with(&[
        "--local-only",
        "--intercept-port",
        "0",
        "--intercept-auth",
        "u:pa:ss",
    ])
    .await;
    let intercept = server.intercept_addr().expect("intercept bound");

    let header = format!("Proxy-Authorization: Basic {}\r\n", basic("u", "pa:ss"));
    let resp = raw_connect(intercept, &header).await;
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "the first colon splits user from pass; the rest belongs to the password: {resp:?}"
    );

    server.shutdown().await;
}

// Blocker found in review: a credential with no listener to guard reads as protection but is not.
// The operator sets it, nothing starts at boot, and a later `POST /intercept` brings up an OPEN
// proxy. Refused for the same reason a blank secret is.
#[tokio::test]
async fn intercept_auth_without_a_port_is_a_startup_error() {
    let cli = Cli::parse_from([
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--intercept-auth",
        "u:p",
    ]);
    let err = ServerBuilder::from_cli(cli)
        .start()
        .await
        .err()
        .expect("a credential guarding nothing must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("--intercept-port"),
        "the error must say which listener is missing, got: {msg}"
    );
}

// AC7 at the door review found uncovered: the listener can be started long after boot over
// `POST /intercept`, and `--require-admin-auth` must cover that too. Checking only at startup let an
// operator who set the strict flag still end up with an open MITM proxy on every interface — the
// exact "authenticated admin plane beside an open MITM proxy" this issue exists to close.
#[tokio::test]
async fn require_admin_auth_refuses_a_runtime_started_exposed_intercept() {
    let server = start_with(&[
        "--host",
        "0.0.0.0",
        "--api-key",
        "s3cr3t",
        "--require-admin-auth",
    ])
    .await;
    let admin = server.admin_addr();

    let refused = reqwest::Client::new()
        .post(format!("http://{admin}/intercept"))
        .header("authorization", "s3cr3t")
        .json(&serde_json::json!({"host": "0.0.0.0", "port": 0}))
        .send()
        .await
        .expect("POST /intercept");
    assert_eq!(
        refused.status(),
        403,
        "a keyless off-host listener started at runtime must be refused under --require-admin-auth"
    );

    // The same request WITH a credential is allowed — the flag gates on authentication, not on the
    // address, at every door.
    let allowed = reqwest::Client::new()
        .post(format!("http://{admin}/intercept"))
        .header("authorization", "s3cr3t")
        .json(&serde_json::json!({
            "host": "0.0.0.0", "port": 0,
            "auth": {"username": "ci", "password": "s3cr3t"}
        }))
        .send()
        .await
        .expect("POST /intercept with auth");
    assert_eq!(
        allowed.status(),
        201,
        "an authenticated listener is allowed"
    );

    server.shutdown().await;
}

// The admin-API door's own blank-credential rejection — previously untested on that door, and it is
// the door a caller other than the operator can reach.
#[tokio::test]
async fn the_admin_api_rejects_a_blank_credential() {
    let server = start_with(&["--local-only"]).await;
    let admin = server.admin_addr();

    let resp = reqwest::Client::new()
        .post(format!("http://{admin}/intercept"))
        .json(&serde_json::json!({
            "port": 0, "auth": {"username": "ci", "password": "   "}
        }))
        .send()
        .await
        .expect("POST /intercept");
    assert_eq!(
        resp.status(),
        400,
        "a blank password must be refused at the admin door too"
    );

    server.shutdown().await;
}
