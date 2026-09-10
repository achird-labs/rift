//! Issue #997: a WebSocket upgrade survives being routed through the intercept tunnel.
//!
//! The intercept proxy's contract is that traffic no rule claims reaches the origin unchanged.
//! Before this, a `Connection: Upgrade` / `Upgrade: websocket` request got an ordinary HTTP
//! response and the handshake failed — so any system under test whose traffic includes a WebSocket
//! (socket.io, GraphQL subscriptions, dev-server live reload) had that connection silently broken
//! by being routed through the proxy.
//!
//! These drive real sockets and a real handshake on purpose. The defect is in how a listener wires
//! an upgrade to hyper, and the specific way it fails — `hyper::upgrade::on` never resolving
//! because the connection was served without upgrade support — produces a **hang**, not an error.
//! Nothing about getting that wrong fails to compile, and no unit test on the relay would show it.

use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rift_http_proxy::intercept::InterceptListener;
use rift_http_proxy::intercept_rules::InterceptRules;
use rift_mock_core::proxy::OutboundTls;
use rift_mock_core::proxy::intercept_ca::{CertificateAuthority, SniCertResolver};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Every assertion here is really "did this finish at all", because the failure mode is a hang.
const BUDGET: Duration = Duration::from_secs(10);

/// A self-signed cert for the origin. Self-signed means it is also its own trust anchor, so the
/// same PEM serves as the origin's certificate and as the `ca_pem` the relay is told to trust —
/// which is the point: the relay must use the *configured* policy, not system roots.
fn origin_cert() -> (String, String) {
    let c =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("generate origin cert");
    (c.cert.pem(), c.key_pair.serialize_pem())
}

fn origin_tls_acceptor(cert_pem: &str, key_pem: &str) -> tokio_rustls::TlsAcceptor {
    let certs = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .expect("parse cert");
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .expect("parse key")
        .expect("a key");
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("server config");
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

/// A `wss://` echo origin. Answers `count` connections, echoing every text frame back with an
/// `echo:` prefix so the test can tell a real round trip from an accidental reflection.
fn spawn_wss_echo_origin(port: u16, cert_pem: String, key_pem: String, count: usize) {
    tokio::spawn(async move {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind origin");
        let acceptor = origin_tls_acceptor(&cert_pem, &key_pem);
        for _ in 0..count {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else {
                    return;
                };
                while let Some(Ok(msg)) = ws.next().await {
                    if msg.is_text() {
                        let text = msg.into_text().expect("text");
                        if ws
                            .send(tokio_tungstenite::tungstenite::Message::Text(format!(
                                "echo:{text}"
                            )))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            });
        }
    });
}

async fn start_intercept(
    rules: InterceptRules,
    origin_ca_pem: String,
) -> (InterceptListener, String) {
    let ca = CertificateAuthority::generate().expect("intercept ca");
    let ca_pem = ca.ca_cert_pem().to_string();
    let resolver = Arc::new(SniCertResolver::new(Arc::new(ca)));
    let listener = InterceptListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        resolver,
        rules,
        None,
        // The whole point of threading the policy through: the relay trusts what the operator
        // configured. With `OutboundTls::default()` (system roots) this origin is untrusted and
        // every test below fails at the TLS handshake.
        OutboundTls {
            ca_pem: Some(origin_ca_pem),
            skip_verify: false,
        },
    )
    .await
    .expect("bind intercept listener");
    (listener, ca_pem)
}

/// Open a tunnel through the proxy to `authority` and hand back the decrypted stream.
///
/// The `CONNECT` is written by hand because that is what a client does: no HTTP client library
/// exposes "give me the raw tunnel afterwards", and the tunnel is exactly what the WebSocket
/// handshake then runs over.
async fn connect_through_proxy(
    proxy: SocketAddr,
    authority: &str,
    intercept_ca_pem: &str,
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut tcp = TcpStream::connect(proxy).await.expect("connect to proxy");
    let mut head = Vec::new();
    write!(
        head,
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"
    )
    .expect("format CONNECT");
    tcp.write_all(&head).await.expect("write CONNECT");

    let mut buf = [0u8; 1024];
    let n = tcp.read(&mut buf).await.expect("read CONNECT response");
    let response = String::from_utf8_lossy(&buf[..n]);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "proxy refused the tunnel: {response}"
    );

    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut intercept_ca_pem.as_bytes()) {
        roots.add(cert.expect("ca cert")).expect("add ca");
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let host = authority.split(':').next().expect("host");
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).expect("name");
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(server_name, tcp)
        .await
        .expect("TLS through the tunnel")
}

/// The headline criterion: a `wss://` conversation survives the proxy, in both directions.
#[tokio::test]
async fn a_websocket_conversation_survives_the_intercept_tunnel() {
    let (cert_pem, key_pem) = origin_cert();
    spawn_wss_echo_origin(23010, cert_pem.clone(), key_pem, 1);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (listener, ca_pem) = start_intercept(InterceptRules::new(), cert_pem).await;
    let proxy = listener.local_addr();

    let outcome = tokio::time::timeout(BUDGET, async {
        let tunnel = connect_through_proxy(proxy, "localhost:23010", &ca_pem).await;
        let (mut ws, response) =
            tokio_tungstenite::client_async("ws://localhost:23010/chat", tunnel)
                .await
                .expect("websocket handshake through the tunnel");
        assert_eq!(
            response.status(),
            101,
            "the origin's 101 must reach the client, not be swallowed by the proxy"
        );

        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            "hello".to_string(),
        ))
        .await
        .expect("client -> origin");

        let reply = ws.next().await.expect("a reply").expect("not an error");
        reply.into_text().expect("text")
    })
    .await;

    let reply = outcome.expect(
        "the conversation never completed within the budget — the classic symptom of serving the \
         tunnel without upgrade support, where `hyper::upgrade::on` simply never resolves",
    );
    assert_eq!(
        reply, "echo:hello",
        "frames must round-trip both ways, unmodified"
    );

    listener.shutdown().await;
}

/// What the origin *actually receives*.
///
/// The echo origin above cannot show this: `tokio_tungstenite::accept_async` validates the method,
/// version, `Connection`, `Upgrade` and the `Sec-WebSocket-*` headers and nothing else — so a
/// handshake missing `Host` sails through it, while a name-based virtual host (nginx, an ALB with
/// host routing, socket.io behind a reverse proxy) MUST reject it with `400` per RFC 9112 §3.2.
/// That is the majority of real origins, so asserting the forwarded head directly is the only way
/// this is covered rather than accidentally passing.
#[tokio::test]
async fn the_relayed_handshake_carries_host_and_not_the_proxy_credential() {
    let (cert_pem, key_pem) = origin_cert();
    let received = Arc::new(tokio::sync::Mutex::new(String::new()));

    // A raw TLS origin that captures the request head and then just closes.
    {
        let (cert_pem, key_pem, received) = (cert_pem.clone(), key_pem, Arc::clone(&received));
        tokio::spawn(async move {
            let listener = TcpListener::bind(("127.0.0.1", 23015))
                .await
                .expect("bind capture origin");
            let acceptor = origin_tls_acceptor(&cert_pem, &key_pem);
            if let Ok((tcp, _)) = listener.accept().await
                && let Ok(mut tls) = acceptor.accept(tcp).await
            {
                let mut buf = [0u8; 4096];
                if let Ok(n) = tls.read(&mut buf).await {
                    *received.lock().await = String::from_utf8_lossy(&buf[..n]).to_string();
                }
                let _ = tls
                    .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                    .await;
            }
        });
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (listener, ca_pem) = start_intercept(InterceptRules::new(), cert_pem).await;
    let proxy = listener.local_addr();

    let _ = tokio::time::timeout(BUDGET, async {
        let mut tunnel = connect_through_proxy(proxy, "localhost:23015", &ca_pem).await;
        // Sent by hand so the proxy credential can be included — a real client configured with
        // proxy auth may attach it to tunnelled requests too.
        tunnel
            .write_all(
                b"GET /chat HTTP/1.1\r\nHost: localhost:23015\r\n\
                  Connection: Upgrade\r\nUpgrade: websocket\r\n\
                  Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                  Proxy-Authorization: Basic c2VjcmV0OnBhc3N3b3Jk\r\n\r\n",
            )
            .await
            .expect("write handshake");
        let mut sink = [0u8; 1024];
        let _ = tunnel.read(&mut sink).await;
    })
    .await;

    let head = received.lock().await.clone();
    assert!(
        !head.is_empty(),
        "the relay never reached the origin at all"
    );
    assert!(
        head.to_lowercase().contains("host:"),
        "the relayed handshake MUST carry Host — without it any name-based virtual host answers \
         400 and the handshake fails one hop upstream, which is the very symptom #997 fixes. \
         head:\n{head}"
    );
    assert!(
        !head.to_lowercase().contains("proxy-authorization"),
        "the tunnel's own credential must never reach a third-party origin. head:\n{head}"
    );
    assert!(
        head.to_lowercase().contains("upgrade: websocket"),
        "the handshake headers that make it a handshake must survive. head:\n{head}"
    );

    listener.shutdown().await;
}

/// A rule that matches the handshake serves its stub, and NO upgrade happens. This is deliberate
/// and useful — it is how you simulate the WebSocket endpoint being down or refusing — and it is
/// also the guard that the relay did not become unconditional.
#[tokio::test]
async fn a_matching_rule_answers_the_handshake_instead_of_upgrading() {
    let (cert_pem, key_pem) = origin_cert();
    // The origin is running, so a passing test proves the rule won rather than that nothing worked.
    spawn_wss_echo_origin(23011, cert_pem.clone(), key_pem, 1);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rules = InterceptRules::new();
    let rule: rift_http_proxy::intercept_rules::InterceptRule =
        serde_json::from_value(serde_json::json!({
            "host": "localhost",
            "predicates": [{"equals": {"path": "/chat"}}],
            "action": {"serve": {"statusCode": 403, "body": "nope"}}
        }))
        .expect("parse rule");
    rules.add(rule).expect("add rule");

    let (listener, ca_pem) = start_intercept(rules, cert_pem).await;
    let proxy = listener.local_addr();

    let outcome = tokio::time::timeout(BUDGET, async {
        let tunnel = connect_through_proxy(proxy, "localhost:23011", &ca_pem).await;
        tokio_tungstenite::client_async("ws://localhost:23011/chat", tunnel).await
    })
    .await
    .expect("the rule path must answer promptly, never hang");

    let err = outcome.expect_err("a 403 is not a successful websocket handshake");
    let rendered = err.to_string();
    assert!(
        rendered.contains("403") || rendered.to_lowercase().contains("http"),
        "the client should see the rule's HTTP response, not an upgrade: {rendered}"
    );

    listener.shutdown().await;
}

/// No rule, and the origin is not there. The point is the *bound* — a relay that cannot reach the
/// origin must answer, not hang, because a hang is indistinguishable from a working tunnel that is
/// simply quiet.
#[tokio::test]
async fn an_unreachable_origin_answers_rather_than_hanging() {
    let (cert_pem, _key) = origin_cert();
    // Nothing is listening on 23012.
    let (listener, ca_pem) = start_intercept(InterceptRules::new(), cert_pem).await;
    let proxy = listener.local_addr();

    let outcome = tokio::time::timeout(BUDGET, async {
        let tunnel = connect_through_proxy(proxy, "localhost:23012", &ca_pem).await;
        tokio_tungstenite::client_async("ws://localhost:23012/chat", tunnel).await
    })
    .await
    .expect("an unreachable origin must produce an answer within the budget, not a hang");

    assert!(
        outcome.is_err(),
        "an unreachable origin cannot produce a successful handshake"
    );

    listener.shutdown().await;
}

/// A non-WebSocket `Upgrade` must take the old path exactly. `h2c` is the one that matters: the
/// tunnel pins ALPN to `http/1.1` and has a documented non-goal about h2 here, so quietly routing
/// it into the relay would change behaviour this issue never intended to touch.
#[tokio::test]
async fn a_non_websocket_upgrade_is_unaffected() {
    let (cert_pem, _key) = origin_cert();
    let (listener, ca_pem) = start_intercept(InterceptRules::new(), cert_pem).await;
    let proxy = listener.local_addr();

    let body = tokio::time::timeout(BUDGET, async {
        let mut tunnel = connect_through_proxy(proxy, "localhost:23013", &ca_pem).await;
        tunnel
            .write_all(
                b"GET /x HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\n\
                  Upgrade: h2c\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write h2c upgrade request");
        let mut out = Vec::new();
        let _ = tunnel.read_to_end(&mut out).await;
        String::from_utf8_lossy(&out).to_string()
    })
    .await
    .expect("an h2c upgrade must answer on the ordinary path, not be relayed");

    assert!(
        body.contains("rift intercepted"),
        "an `Upgrade: h2c` request must still get the fixed fall-through response, not a relay \
         attempt: {body}"
    );

    listener.shutdown().await;
}

/// Shutting the listener down must terminate a live relay. A relay that outlived its listener is
/// the leak the shutdown broadcast was arranged to prevent (#1010), and a WebSocket relay is the
/// longest-lived thing this listener can hold.
#[tokio::test]
async fn listener_shutdown_terminates_a_live_relay() {
    let (cert_pem, key_pem) = origin_cert();
    spawn_wss_echo_origin(23014, cert_pem.clone(), key_pem, 1);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (listener, ca_pem) = start_intercept(InterceptRules::new(), cert_pem).await;
    let proxy = listener.local_addr();

    let tunnel = connect_through_proxy(proxy, "localhost:23014", &ca_pem).await;
    let (mut ws, _) = tokio::time::timeout(
        BUDGET,
        tokio_tungstenite::client_async("ws://localhost:23014/chat", tunnel),
    )
    .await
    .expect("handshake within budget")
    .expect("handshake succeeds");

    // Prove the relay is actually carrying traffic before we tear it down, or "it ended" would be
    // satisfied by a relay that never started.
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        "alive".to_string(),
    ))
    .await
    .expect("send");
    let reply = tokio::time::timeout(BUDGET, ws.next())
        .await
        .expect("reply within budget")
        .expect("a reply")
        .expect("not an error");
    assert_eq!(reply.into_text().expect("text"), "echo:alive");

    listener.shutdown().await;

    // The relay must end promptly. Draining to completion is what shows it: a relay still running
    // would leave this pending until the budget expires.
    let drained = tokio::time::timeout(BUDGET, async { while ws.next().await.is_some() {} }).await;
    assert!(
        drained.is_ok(),
        "a live relay outlived the listener that owned it"
    );
}
