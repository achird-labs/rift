//! Issue #1273: a `forward` rule can name the imposter's `host` and `scheme`, so it reaches an
//! imposter outside the engine process (another container, a TLS-only vendor mock) — not only
//! `http://127.0.0.1:{port}`.

use std::sync::Arc;

use clap::Parser;
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use rift_mock_core::proxy::intercept_ca::{CertificateAuthority, SniCertResolver};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

async fn start(extra: &[&str]) -> RunningServer {
    let mut args = vec![
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--intercept-port",
        "0",
    ];
    args.extend_from_slice(extra);
    ServerBuilder::from_cli(Cli::parse_from(args))
        .start()
        .await
        .expect("server starts")
}

/// Serve one canned response per connection, echoing the request's `Host` header in the body so a
/// test can see both that the request arrived and what it carried. `tls` makes it an HTTPS origin
/// whose certificate chains to that CA only.
async fn spawn_upstream(tls: Option<Arc<CertificateAuthority>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let port = listener.local_addr().expect("addr").port();
    let acceptor = tls.map(|ca| {
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(SniCertResolver::new(ca)));
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        TlsAcceptor::from(Arc::new(config))
    });
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                match acceptor {
                    Some(acceptor) => {
                        if let Ok(mut s) = acceptor.accept(stream).await {
                            answer(&mut s, "https").await;
                        }
                    }
                    None => {
                        let mut s = stream;
                        answer(&mut s, "http").await;
                    }
                }
            });
        }
    });
    port
}

async fn answer<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(s: &mut S, scheme: &str) {
    let mut buf = vec![0u8; 4096];
    let n = s.read(&mut buf).await.unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    let host = head
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_default();
    let body = format!("{scheme} upstream saw host={host}");
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(response.as_bytes()).await;
    let _ = s.shutdown().await;
}

async fn ca_pem(server: &RunningServer) -> String {
    reqwest::get(format!("http://{}/intercept/ca.pem", server.admin_addr()))
        .await
        .expect("ca.pem")
        .text()
        .await
        .expect("body")
}

fn sut(server: &RunningServer, ca: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::https(format!(
                "http://{}",
                server.intercept_addr().expect("listener")
            ))
            .unwrap(),
        )
        .add_root_certificate(reqwest::Certificate::from_pem(ca.as_bytes()).unwrap())
        .build()
        .unwrap()
}

async fn post_rule(server: &RunningServer, rule: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{}/intercept/rules", server.admin_addr()))
        .body(rule.to_string())
        .send()
        .await
        .expect("post rule")
}

#[tokio::test]
async fn forward_to_a_named_host_reaches_the_upstream() {
    let server = start(&[]).await;
    let upstream = spawn_upstream(None).await;
    let rule = format!(
        r#"{{"host":"cdn.example.com","action":{{"forward":{{"host":"localhost","port":{upstream}}}}}}}"#
    );
    assert_eq!(post_rule(&server, &rule).await.status(), 201);
    let ca = ca_pem(&server).await;
    let body = sut(&server, &ca)
        .get("https://cdn.example.com/x")
        .send()
        .await
        .expect("intercepted")
        .text()
        .await
        .unwrap();
    assert_eq!(
        body, "http upstream saw host=cdn.example.com",
        "reached the named host, carrying the SUT's Host (#1278)"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn forward_over_https_trusts_the_upstream_ca() {
    let origin_ca = Arc::new(CertificateAuthority::generate().expect("origin CA"));
    let dir = tempfile::tempdir().unwrap();
    let ca_file = dir.path().join("origin-ca.pem");
    std::fs::write(&ca_file, origin_ca.ca_cert_pem()).unwrap();
    let upstream = spawn_upstream(Some(origin_ca)).await;

    let server = start(&["--upstream-ca-file", ca_file.to_str().unwrap()]).await;
    let rule = format!(
        r#"{{"host":"cdn.example.com","action":{{"forward":{{"host":"localhost","port":{upstream},"scheme":"https"}}}}}}"#
    );
    assert_eq!(post_rule(&server, &rule).await.status(), 201);
    let ca = ca_pem(&server).await;
    let response = sut(&server, &ca)
        .get("https://cdn.example.com/x")
        .send()
        .await
        .expect("intercepted");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text().await.unwrap(),
        "https upstream saw host=cdn.example.com"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn forward_over_https_without_trust_is_502() {
    let origin_ca = Arc::new(CertificateAuthority::generate().expect("origin CA"));
    let upstream = spawn_upstream(Some(origin_ca)).await;
    let server = start(&[]).await;
    let rule = format!(
        r#"{{"host":"cdn.example.com","action":{{"forward":{{"host":"localhost","port":{upstream},"scheme":"https"}}}}}}"#
    );
    assert_eq!(post_rule(&server, &rule).await.status(), 201);
    let ca = ca_pem(&server).await;
    let response = sut(&server, &ca)
        .get("https://cdn.example.com/x")
        .send()
        .await
        .expect("intercepted");
    assert_eq!(
        response.status(),
        502,
        "an untrusted upstream is a failed forward"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn forward_targets_that_cannot_be_reached_are_refused_at_admission() {
    let server = start(&[]).await;
    for (forward, why) in [
        (
            r#"{"port":4600,"hots":"x"}"#,
            "an unknown key (a typo) is refused, not ignored",
        ),
        (r#"{"port":0}"#, "port 0"),
        (r#"{"port":4600,"host":""}"#, "an empty host"),
        (
            r#"{"port":4600,"host":"http://mock-svc"}"#,
            "a scheme inside host",
        ),
        (
            r#"{"port":4600,"host":"mock-svc:4600"}"#,
            "a port inside host",
        ),
        (
            r#"{"port":4600,"host":"mock-svc/path"}"#,
            "a path inside host",
        ),
        (
            r#"{"port":4600,"host":"::1"}"#,
            "an unbracketed IPv6 literal",
        ),
        (r#"{"port":4600,"scheme":"ftp"}"#, "an unknown scheme"),
        (
            r#"{"port":4600,"host":"999.0.0.1"}"#,
            "an IPv4 literal no URL can carry",
        ),
        (
            r#"{"port":4600,"host":"1.2.3"}"#,
            "a short IPv4 form the URL parser rewrites",
        ),
    ] {
        let rule = format!(r#"{{"host":"a.test","action":{{"forward":{forward}}}}}"#);
        let response = post_rule(&server, &rule).await;
        assert_eq!(response.status(), 400, "{why}: {forward}");
    }
    let listed = reqwest::get(format!("http://{}/intercept/rules", server.admin_addr()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(listed.trim(), "[]", "nothing refused was stored");

    for forward in [
        r#"{"port":4600,"host":"mock-svc"}"#,
        r#"{"port":4600,"host":"10.0.0.7","scheme":"http"}"#,
        r#"{"port":4600,"host":"[::1]","scheme":"https"}"#,
    ] {
        let rule = format!(r#"{{"host":"a.test","action":{{"forward":{forward}}}}}"#);
        assert_eq!(post_rule(&server, &rule).await.status(), 201, "{forward}");
    }
    server.shutdown().await;
}

/// A rule written before this change reads back exactly as it was written: no `host`/`scheme`
/// keys appear, so SDK read-back comparisons keep passing.
#[tokio::test]
async fn a_legacy_forward_rule_round_trips_without_host_or_scheme_keys() {
    let server = start(&[]).await;
    assert_eq!(
        post_rule(
            &server,
            r#"{"host":"a.test","action":{"forward":{"port":4600}}}"#
        )
        .await
        .status(),
        201
    );
    let listed: serde_json::Value =
        reqwest::get(format!("http://{}/intercept/rules", server.admin_addr()))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(
        listed[0]["action"],
        serde_json::json!({"forward": {"port": 4600}})
    );
    server.shutdown().await;
}

#[tokio::test]
async fn a_config_file_with_an_unreachable_forward_target_fails_startup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        r#"{ "imposters": [], "intercept": { "port": 0, "rules": [
            { "host": "a.test", "action": { "forward": { "port": 4600, "host": "http://x" } } } ] } }"#,
    )
    .unwrap();
    let cli = Cli::parse_from([
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--configfile",
        path.to_str().unwrap(),
    ]);
    match ServerBuilder::from_cli(cli).start().await {
        Ok(server) => {
            server.shutdown().await;
            panic!("a bad forward host must refuse the start");
        }
        Err(e) => {
            let msg = format!("{e:#}");
            assert!(
                msg.contains("http://x"),
                "the refusal names the bad host: {msg}"
            );
        }
    }
}

/// The discriminating case: an upstream listening only on IPv6 loopback, which a forward that
/// ignored `host` (and dialled `127.0.0.1`) cannot reach. Skipped where the host has no `::1`.
#[tokio::test]
async fn forward_to_an_ipv6_host_reaches_an_upstream_not_on_ipv4_loopback() {
    let Ok(listener) = TcpListener::bind("[::1]:0").await else {
        eprintln!("skipping: no IPv6 loopback on this host");
        return;
    };
    let upstream = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move { answer(&mut s, "v6").await });
        }
    });
    let server = start(&[]).await;
    let rule = format!(
        r#"{{"host":"cdn.example.com","action":{{"forward":{{"host":"[::1]","port":{upstream}}}}}}}"#
    );
    assert_eq!(post_rule(&server, &rule).await.status(), 201);
    let ca = ca_pem(&server).await;
    let response = sut(&server, &ca)
        .get("https://cdn.example.com/x")
        .send()
        .await
        .expect("intercepted");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text().await.unwrap(),
        "v6 upstream saw host=cdn.example.com"
    );
    server.shutdown().await;
}
