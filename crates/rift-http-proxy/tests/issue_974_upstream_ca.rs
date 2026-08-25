//! Issue #974: a `proxy` stub must be able to reach a privately-issued origin.
//!
//! The gap this pins had no test because nothing in the suite could reach an origin behind a
//! private CA: every fixture is plain HTTP, localhost, or public-CA. So the first test here is
//! deliberately a *negative* one — it proves the default policy still refuses an untrusted chain,
//! which is what makes the two positive cases meaningful rather than vacuous.

use std::sync::Arc;
use std::time::Duration;

use rift_mock_core::imposter::{ImposterManager, build_upstream_client};
use rift_mock_core::proxy::OutboundTls;
use rift_mock_core::proxy::intercept_ca::{CertificateAuthority, SniCertResolver};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

const ORIGIN_BODY: &str = "private-origin-ok";

/// An HTTPS origin whose certificate chains to `ca` and to nothing else — the shape of a corporate
/// API gateway. Returns the port it is listening on.
async fn spawn_private_origin(ca: Arc<CertificateAuthority>) -> u16 {
    spawn_private_origin_serving(ca, ORIGIN_BODY.to_string()).await
}

/// As above, but serving `body` — used by the config-source test, which needs a JSON document.
async fn spawn_private_origin_serving(ca: Arc<CertificateAuthority>, body: String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the private origin");
    let port = listener.local_addr().expect("origin addr").port();

    let resolver = Arc::new(SniCertResolver::new(ca));
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("origin TLS versions")
    .with_no_client_auth()
    .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                // Read just the request head; the body is irrelevant to what this test asserts.
                let mut buf = [0u8; 2048];
                let _ = tls.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(response.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    port
}

fn proxy_imposter(port: u16, origin_port: u16) -> serde_json::Value {
    serde_json::json!({
        "port": port,
        "protocol": "http",
        "stubs": [{
            "responses": [{
                "proxy": { "to": format!("https://localhost:{origin_port}"), "mode": "proxyOnce" }
            }]
        }]
    })
}

/// Drive one request through an imposter created on `manager`, returning (status, body, headers).
async fn proxy_once(
    manager: &Arc<ImposterManager>,
    imposter_port: u16,
    origin_port: u16,
) -> (u16, String, reqwest::header::HeaderMap) {
    manager
        .create_imposter(
            serde_json::from_value(proxy_imposter(imposter_port, origin_port))
                .expect("imposter config"),
        )
        .await
        .expect("create the proxying imposter");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("test client")
        .get(format!("http://127.0.0.1:{imposter_port}/token"))
        .send()
        .await
        .expect("the imposter itself must always answer");

    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.text().await.unwrap_or_default();
    (status, body, headers)
}

#[tokio::test]
async fn proxy_without_a_trusted_ca_is_refused_as_an_upstream_failure() {
    rift_http_proxy::install_default_crypto_provider();
    let ca = Arc::new(CertificateAuthority::generate().expect("generate a private CA"));
    let origin_port = spawn_private_origin(Arc::clone(&ca)).await;

    // The default policy trusts the OS store, which cannot contain this just-generated CA.
    let manager = Arc::new(ImposterManager::new());
    let (status, body, headers) = proxy_once(&manager, 19955, origin_port).await;

    assert_eq!(status, 502, "an untrusted chain is an upstream failure");
    assert!(
        headers.contains_key("x-rift-proxy-error"),
        "the failure must be marked as a proxy error, got headers: {headers:?}"
    );
    // The rustls cause (`UnknownIssuer`) is deliberately log-only: `upstream_error_response`
    // formats `{e}` for the client and `{e:#}` for the log, so the chain is never leaked to a
    // caller. What the client is owed is which upstream failed, and that the proxy hop is what
    // failed — assert that, rather than a string the contract does not promise.
    assert!(
        body.contains(&format!("https://localhost:{origin_port}")),
        "the client must be told which upstream failed, got: {body}"
    );
    assert!(
        body.contains("upstream failure"),
        "the envelope must classify it as an upstream failure, got: {body}"
    );
    let _ = manager.delete_imposter(19955).await;
}

#[tokio::test]
async fn proxy_with_the_ca_pem_reaches_the_private_origin() {
    rift_http_proxy::install_default_crypto_provider();
    let ca = Arc::new(CertificateAuthority::generate().expect("generate a private CA"));
    let origin_port = spawn_private_origin(Arc::clone(&ca)).await;

    let policy = OutboundTls {
        ca_pem: Some(ca.ca_cert_pem().to_string()),
        skip_verify: false,
    };
    let manager = Arc::new(
        ImposterManager::new()
            .with_upstream_client(build_upstream_client(&policy).expect("client under the CA")),
    );
    let (status, body, headers) = proxy_once(&manager, 19956, origin_port).await;

    assert_eq!(
        status, 200,
        "the supplied anchor must make the origin reachable"
    );
    assert_eq!(
        body, ORIGIN_BODY,
        "the origin's body must be relayed verbatim"
    );
    assert!(
        headers.contains_key("x-rift-proxy"),
        "a proxied response is marked as one"
    );
    let _ = manager.delete_imposter(19956).await;
}

#[tokio::test]
async fn proxy_with_skip_verify_reaches_the_private_origin() {
    rift_http_proxy::install_default_crypto_provider();
    let ca = Arc::new(CertificateAuthority::generate().expect("generate a private CA"));
    let origin_port = spawn_private_origin(Arc::clone(&ca)).await;

    let policy = OutboundTls {
        ca_pem: None,
        skip_verify: true,
    };
    let manager = Arc::new(
        ImposterManager::new()
            .with_upstream_client(build_upstream_client(&policy).expect("skip-verify client")),
    );
    let (status, body, _) = proxy_once(&manager, 19957, origin_port).await;

    assert_eq!(status, 200, "skip-verify must accept the private chain");
    assert_eq!(body, ORIGIN_BODY);
    let _ = manager.delete_imposter(19957).await;
}

/// The C-ABI sets its policy after the manager exists (`rift_start` precedes `rift_serve_admin`),
/// so the post-construction setter must be equivalent to having built with it.
#[tokio::test]
async fn set_upstream_client_applies_to_imposters_created_afterwards() {
    rift_http_proxy::install_default_crypto_provider();
    let ca = Arc::new(CertificateAuthority::generate().expect("generate a private CA"));
    let origin_port = spawn_private_origin(Arc::clone(&ca)).await;

    let manager = Arc::new(ImposterManager::new());
    let policy = OutboundTls {
        ca_pem: Some(ca.ca_cert_pem().to_string()),
        skip_verify: false,
    };
    manager.set_upstream_client(build_upstream_client(&policy).expect("client under the CA"));

    let (status, body, _) = proxy_once(&manager, 19958, origin_port).await;
    assert_eq!(
        status, 200,
        "a client set after construction must still apply"
    );
    assert_eq!(body, ORIGIN_BODY);
    let _ = manager.delete_imposter(19958).await;
}

/// The flags exist under both spellings the docs advertise. `verify-docs-coverage.sh` gates that
/// they are documented; this gates that they parse onto the right fields.
mod cli_surface {
    use clap::Parser;
    use rift_http_proxy::server::Cli;

    #[test]
    fn upstream_tls_flags_parse_from_the_command_line() {
        let cli = Cli::try_parse_from([
            "rift",
            "--upstream-ca-file",
            "/etc/rift/corp-ca.pem",
            "--upstream-tls-skip-verify",
        ])
        .expect("both flags parse");
        assert_eq!(
            cli.upstream_ca_file.as_deref(),
            Some(std::path::Path::new("/etc/rift/corp-ca.pem"))
        );
        assert!(cli.upstream_tls_skip_verify);
    }

    #[test]
    fn upstream_tls_flags_default_to_the_verifying_policy() {
        let cli = Cli::try_parse_from(["rift"]).expect("no flags parse");
        assert!(cli.upstream_ca_file.is_none());
        assert!(
            !cli.upstream_tls_skip_verify,
            "verification must be on unless explicitly disabled"
        );
    }
}

/// Criterion 6: `--configfile https://…` fetches through the same policy. Without it, an operator
/// who fixes their proxying with `--upstream-ca-file` would still find their config URL failing
/// with the identical `UnknownIssuer`.
mod config_source {
    use super::*;
    use rift_http_proxy::sources::{HttpSource, ImposterSource, SourceRef};

    fn imposter_doc() -> String {
        serde_json::json!({
            "imposters": [{ "port": 19959, "protocol": "http", "stubs": [] }]
        })
        .to_string()
    }

    #[tokio::test]
    async fn http_source_under_the_default_policy_refuses_a_private_origin() {
        rift_http_proxy::install_default_crypto_provider();
        let ca = Arc::new(CertificateAuthority::generate().expect("generate a private CA"));
        let port = spawn_private_origin_serving(Arc::clone(&ca), imposter_doc()).await;

        let source = HttpSource::with_policy(&OutboundTls::default()).expect("default source");
        let err = source
            .fetch(&SourceRef::new(format!(
                "https://localhost:{port}/imposters.json"
            )))
            .await
            .expect_err("an untrusted config origin must not be fetched");
        assert!(
            format!("{err:#}").contains("UnknownIssuer"),
            "the cause chain must name the trust failure, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn http_source_with_the_ca_pem_fetches_from_a_private_origin() {
        rift_http_proxy::install_default_crypto_provider();
        let ca = Arc::new(CertificateAuthority::generate().expect("generate a private CA"));
        let port = spawn_private_origin_serving(Arc::clone(&ca), imposter_doc()).await;

        let policy = OutboundTls {
            ca_pem: Some(ca.ca_cert_pem().to_string()),
            skip_verify: false,
        };
        let source = HttpSource::with_policy(&policy).expect("source under the CA");
        let fetched = source
            .fetch(&SourceRef::new(format!(
                "https://localhost:{port}/imposters.json"
            )))
            .await
            .expect("the supplied anchor must make the config document reachable");
        assert_eq!(
            fetched.configs.len(),
            1,
            "the document must be parsed, not merely transferred"
        );
    }
}

/// The CLI half of "a bad anchor fails at startup". The FFI half is covered by
/// `ffi_unreadable_upstream_ca_file_fails_the_serve`; the file-read lives in both entry points.
mod cli_startup {
    use clap::Parser;
    use rift_http_proxy::server::{Cli, ServerBuilder};

    #[tokio::test]
    async fn run_fails_when_the_upstream_ca_file_is_unreadable() {
        let cli = Cli::try_parse_from([
            "rift",
            "--host",
            "127.0.0.1",
            "--port",
            "12641",
            "--metrics-port",
            "19488",
            "--upstream-ca-file",
            "/nonexistent/rift-974/corp-ca.pem",
        ])
        .expect("cli parse: the flag surface must not touch the file");

        let result = ServerBuilder::from_cli(cli).run().await;
        let err = result.expect_err("an unreadable --upstream-ca-file must fail run()");
        assert!(
            format!("{err:#}").contains("upstream-ca-file"),
            "the error must name the flag that caused it, got: {err:#}"
        );
    }
}
