//! Issue #991: the decrypted CONNECT tunnel is served by hyper instead of hand-rolled HTTP/1.1.
//!
//! These pin the behaviour the swap must preserve (AC3) plus the edge cases the swap newly
//! exposes — above all that the tunnel still closes after one request. Keep-alive is #993 and is
//! explicitly out of scope here, so hyper's *default* keep-alive silently shipping it would be a
//! regression these tests exist to catch.
//!
//! It also carries issue #995's body-cap battery. #991 is what *made* the cap a refusal rather
//! than a silent truncation, so the two belong in one file and share its tunnel harness; #995
//! then pins that refusal on every action path and both framings, which #991 only did for the
//! forward path.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The intercept listener's own buffered-body cap (`MAX_BODY_BYTES` in `intercept.rs`).
const MAX_BODY_BYTES: usize = 1024 * 1024;

async fn start_intercept() -> rift_http_proxy::server::RunningServer {
    ServerBuilder::from_cli(Cli::parse_from([
        "rift",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--local-only",
        "--intercept-port",
        "0",
    ]))
    .start()
    .await
    .expect("server starts")
}

async fn ca_pem(admin: SocketAddr) -> String {
    reqwest::get(format!("http://{admin}/intercept/ca.pem"))
        .await
        .expect("ca.pem")
        .text()
        .await
        .expect("ca.pem body")
}

async fn add_rule(admin: SocketAddr, rule: &str) {
    let created = reqwest::Client::new()
        .post(format!("http://{admin}/intercept/rules"))
        .body(rule.to_string())
        .send()
        .await
        .expect("add rule");
    assert_eq!(created.status(), 201, "rule accepted");
}

fn proxy_client(intercept: SocketAddr, ca: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(format!("http://{intercept}")).expect("proxy"))
        .add_root_certificate(reqwest::Certificate::from_pem(ca.as_bytes()).expect("ca"))
        .build()
        .expect("client")
}

/// Drive one request through the tunnel over a raw TLS stream and return the response bytes
/// exactly as they went over the wire, plus whether the server closed the connection afterwards.
///
/// A proxy-aware `reqwest` cannot answer either question: hyper's client strips `connection` from
/// the headers it surfaces, and its pool hides whether the socket was reused. Both are precisely
/// what AC3 and #993's out-of-scope-ness turn on, so this reads the socket directly.
async fn raw_tunnel_request(
    intercept: SocketAddr,
    ca: &str,
    host: &str,
    request: &str,
) -> (String, bool) {
    let mut tls = open_tls_tunnel(intercept, ca, host).await;
    tls.write_all(request.as_bytes()).await.expect("write");

    // Read to EOF. Reaching EOF at all is the evidence the server closed after one response.
    let mut out = Vec::new();
    let closed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tls.read_to_end(&mut out),
    )
    .await
    .is_ok();
    (String::from_utf8_lossy(&out).into_owned(), closed)
}

/// Complete the `CONNECT` handshake and the TLS handshake, and hand back the decrypted stream so a
/// test can drive the tunnel byte by byte — which is what the timeout and half-close tests need
/// and no proxy-aware client can express.
async fn open_tls_tunnel(
    intercept: SocketAddr,
    ca: &str,
    host: &str,
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut stream = TcpStream::connect(intercept).await.expect("connect");
    stream
        .write_all(format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n").as_bytes())
        .await
        .expect("write CONNECT");

    let mut established = Vec::new();
    let mut byte = [0u8; 1];
    while !established.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).await.expect("read CONNECT reply");
        assert!(n != 0, "proxy closed before answering CONNECT");
        established.push(byte[0]);
    }
    assert!(
        String::from_utf8_lossy(&established).starts_with("HTTP/1.1 200"),
        "CONNECT established: {}",
        String::from_utf8_lossy(&established)
    );

    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca.as_bytes()) {
        roots.add(cert.expect("pem cert")).expect("add root");
    }
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("client protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).expect("sni");
    connector
        .connect(server_name, stream)
        .await
        .expect("tls handshake")
}

// ===== AC4: a slow client cannot park a connection task =====
//
// `header_read_timeout` is wired from `HttpTuning`, but wiring is not behaviour: this drives a
// client that completes CONNECT and the TLS handshake and then dribbles a request head forever,
// and asserts the server cuts it. Without this, AC4 rested on reading the builder chain.
//
// `RIFT_HTTP_HEADER_TIMEOUT` is read by `HttpTuning::from_env()` at `bind()`, so it must be set
// before the server starts — hence `serial`, since env is process-global.
#[tokio::test]
#[serial_test::serial]
async fn a_client_that_dribbles_the_request_head_is_cut_off() {
    unsafe { std::env::set_var("RIFT_HTTP_HEADER_TIMEOUT", "1") };
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;

    let mut tls = open_tls_tunnel(intercept, &ca, "cdn.example.com").await;
    // A request head that is never terminated — no second CRLF, ever.
    tls.write_all(b"GET /slow HTTP/1.1\r\nHost: cdn.example.com\r\n")
        .await
        .expect("write partial head");

    let started = std::time::Instant::now();
    let mut out = Vec::new();
    let cut = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tls.read_to_end(&mut out),
    )
    .await;
    let elapsed = started.elapsed();

    assert!(
        cut.is_ok(),
        "the connection must be closed by header_read_timeout, not parked until the test's own \
         deadline; read {} byte(s)",
        out.len()
    );
    // Timing, not merely "the socket ended": a connection that died instantly would satisfy the
    // assertion above for a reason that has nothing to do with the timeout being wired. The
    // window's lower bound is what ties the close to `RIFT_HTTP_HEADER_TIMEOUT=1`, and its upper
    // bound is what rules out the 30s default.
    assert!(
        elapsed >= std::time::Duration::from_millis(500)
            && elapsed < std::time::Duration::from_secs(10),
        "expected the 1s header_read_timeout to close it, but it closed after {elapsed:?}"
    );
    unsafe { std::env::remove_var("RIFT_HTTP_HEADER_TIMEOUT") };
    server.shutdown().await;
}

// ===== E2 (live): a client that disconnects mid-body is NOT told its body was too large =====
//
// The unit test drives a real `Limited` overflow for the TooLarge half, but its disconnect half is
// a hand-built error. This is the half where being wrong is the actual bug — answering 413 to a
// client that merely went away — so it is pinned over a real socket.
#[tokio::test]
async fn a_mid_body_disconnect_is_not_reported_as_too_large() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","action":{"serve":{"statusCode":200,"body":"ok"}}}"#,
    )
    .await;

    let mut tls = open_tls_tunnel(intercept, &ca, "cdn.example.com").await;
    // Promise 100 bytes, send 10, then half-close so the server reads EOF mid-body while we can
    // still read whatever it answers.
    tls.write_all(
        b"POST /upload HTTP/1.1\r\nHost: cdn.example.com\r\ncontent-length: 100\r\n\r\n0123456789",
    )
    .await
    .expect("write partial body");
    tls.shutdown().await.expect("half-close");

    let mut out = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tls.read_to_end(&mut out),
    )
    .await;
    let response = String::from_utf8_lossy(&out);

    assert!(
        !response.contains("413"),
        "a client that went away mid-body was never over the cap: {response}"
    );
    // It either answers 400 (the body could not be read) or nothing at all if the socket died
    // first; both are honest, 413 is not.
    assert!(
        response.is_empty() || response.starts_with("HTTP/1.1 400"),
        "expected 400 or a dead socket, got: {response}"
    );
    server.shutdown().await;
}

// ===== E14: chunked request bodies now decode, where they used to be treated as empty =====
//
// This is issue #992's payoff falling out of hyper's decoding, exactly as #991 predicted. It is
// pinned here because it is a user-visible behaviour change riding along with the refactor, and
// because the documentation asserting the opposite is corrected in this same PR.
#[tokio::test]
async fn a_chunked_request_body_is_decoded_and_matched() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    // The rule matches on the DECODED body, so it can only fire if chunked framing was decoded.
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","predicates":[{"equals":{"body":"hello world"}}],"action":{"serve":{"statusCode":200,"body":"matched-chunked"}}}"#,
    )
    .await;

    let (response, _) = raw_tunnel_request(
        intercept,
        &ca,
        "cdn.example.com",
        "POST /upload HTTP/1.1\r\nHost: cdn.example.com\r\ntransfer-encoding: chunked\r\n\r\n\
         6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n",
    )
    .await;

    assert!(
        response.contains("matched-chunked"),
        "the chunked body reached the matcher decoded; before #991 it was treated as empty and \
         this rule could not fire: {response}"
    );
    server.shutdown().await;
}

// ===== E3 / AC3: the tunnel still carries exactly one request =====
//
// Keep-alive across the tunnel is #993 and out of scope. hyper defaults to keep-alive on
// HTTP/1.1, so shipping this refactor without explicitly disabling it would deliver #993 by
// accident — and #995's comment records why that ordering is dangerous, not merely premature.
#[tokio::test]
async fn every_response_still_closes_the_connection() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","action":{"serve":{"statusCode":200,"body":"ok"}}}"#,
    )
    .await;

    let (response, closed) = raw_tunnel_request(
        intercept,
        &ca,
        "cdn.example.com",
        "GET /a HTTP/1.1\r\nHost: cdn.example.com\r\n\r\n",
    )
    .await;

    assert!(
        response.to_ascii_lowercase().contains("connection: close"),
        "every response still announces close: {response}"
    );
    assert!(
        closed,
        "the server closes the tunnel after one response; keep-alive is #993: {response}"
    );
    server.shutdown().await;
}

// ===== E11 / AC3: the no-rule-match fallback is byte-identical =====
#[tokio::test]
async fn no_rule_match_still_answers_the_slice3_200() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;

    let resp = proxy_client(intercept, &ca)
        .get("https://cdn.example.com/config.json")
        .send()
        .await
        .expect("intercepted");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/plain"),
    );
    assert_eq!(
        resp.text().await.expect("body"),
        "rift intercepted GET /config.json for cdn.example.com\n",
        "the slice-3 fallback body is unchanged"
    );
    server.shutdown().await;
}

// ===== E12 / AC3: a forward to a dead upstream is still 502 with an empty body =====
#[tokio::test]
async fn a_failed_forward_is_still_502() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    // Port 1 is privileged and outside the ephemeral range, so nothing can ever be listening
    // (the #859 convention).
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","action":{"forward":{"port":1}}}"#,
    )
    .await;

    let resp = proxy_client(intercept, &ca)
        .get("https://cdn.example.com/gone")
        .send()
        .await
        .expect("intercepted");
    assert_eq!(resp.status(), 502);
    // E12 names the header, not just the empty body: an implementation that dropped
    // `content-length` and leaned on `connection: close` for framing would pass a body-only
    // assertion while changing what the client sees.
    assert_eq!(
        resp.headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok()),
        Some("0"),
        "502 still declares an empty body rather than relying on close-framing"
    );
    assert_eq!(resp.text().await.expect("body"), "", "502 carries no body");
    server.shutdown().await;
}

// ===== E1 / user decision: an oversize body is refused with 413, never matched, never forwarded =====
//
// Today this silently truncates at 1 MiB and forwards the fragment as if complete (#995). The
// swap to `http_body_util::Limited` makes that impossible to express, and the refusal is what
// removes the unread-remainder smuggling primitive before #993 can make the tunnel keep-alive.
#[tokio::test]
async fn an_oversize_request_body_is_refused_with_413() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    // A forward rule, so a 413 also proves nothing reached the (dead) upstream: a forwarded
    // request would have answered 502 instead.
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","action":{"forward":{"port":1}}}"#,
    )
    .await;

    let resp = proxy_client(intercept, &ca)
        .post("https://cdn.example.com/upload")
        .body("x".repeat(MAX_BODY_BYTES + 1))
        .send()
        .await
        .expect("intercepted");
    assert_eq!(
        resp.status(),
        413,
        "an oversize body is refused, not truncated and forwarded"
    );
    server.shutdown().await;
}

// The boundary the refusal must not over-reach: a body exactly at the cap is still served.
#[tokio::test]
async fn a_body_at_the_cap_is_still_served() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","action":{"serve":{"statusCode":200,"body":"accepted"}}}"#,
    )
    .await;

    let resp = proxy_client(intercept, &ca)
        .post("https://cdn.example.com/upload")
        .body("x".repeat(MAX_BODY_BYTES))
        .send()
        .await
        .expect("intercepted");
    assert_eq!(resp.status(), 200, "a body exactly at the cap is accepted");
    assert_eq!(resp.text().await.expect("body"), "accepted");
    server.shutdown().await;
}

// ===== E8: a bodyless request still matches a rule with no body predicate =====
//
// The parity trap in the swap: hyper hands back an empty body where the hand-rolled reader handed
// back `None`. Matching an empty body as `Some("")` rather than `None` changes what every body
// predicate does, so this pins the mapping.
#[tokio::test]
async fn a_request_with_no_body_still_matches() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","action":{"serve":{"statusCode":204}}}"#,
    )
    .await;

    let resp = proxy_client(intercept, &ca)
        .get("https://cdn.example.com/ping")
        .send()
        .await
        .expect("intercepted");
    assert_eq!(resp.status(), 204);
    server.shutdown().await;
}

// ===== E13 / AC3: the pre-TLS CONNECT paths are untouched by the swap =====
#[tokio::test]
async fn a_non_connect_head_is_still_405() {
    let server = start_intercept().await;
    let intercept = server.intercept_addr().expect("intercept bound");

    let mut stream = TcpStream::connect(intercept).await.expect("connect");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .expect("write");
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.expect("read");
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.starts_with("HTTP/1.1 405 Method Not Allowed"),
        "a non-CONNECT head is still refused before any TLS: {text}"
    );
    server.shutdown().await;
}

// ===== AC3: a served stub still renders status, headers and body through the tunnel =====
#[tokio::test]
async fn a_served_stub_still_renders_status_headers_and_body() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","action":{"serve":{"statusCode":418,"headers":{"x-flag":["on"]},"body":{"featureX":"ON"}}}}"#,
    )
    .await;

    let resp = proxy_client(intercept, &ca)
        .get("https://cdn.example.com/flags")
        .send()
        .await
        .expect("intercepted");
    assert_eq!(resp.status(), 418);
    assert_eq!(
        resp.headers().get("x-flag").and_then(|v| v.to_str().ok()),
        Some("on")
    );
    assert_eq!(resp.text().await.expect("body"), r#"{"featureX":"ON"}"#);
    server.shutdown().await;
}

// ===== Issue #995: the 1 MiB cap refuses on every path and both framings =====
//
// #991 shipped the refusal itself (`read_limited_body` reads through `http_body_util::Limited`
// and maps `LengthLimitError` to 413), and pinned it on the *forward* path only — see
// `an_oversize_request_body_is_refused_with_413` above. That single test cannot tell a cap
// enforced in the reader from a cap enforced where the body happens to be *used*: the serve and
// no-rule-match paths never look at the body, so an implementation that checked the size only
// before forwarding would answer 200 to a 2 MiB request and still pass. These close that gap.

/// Like [`raw_tunnel_request`], but tolerates the write failing part-way.
///
/// An over-cap request is precisely the case where the server answers and closes while the client
/// is still sending, so `write_all` can return `BrokenPipe` for a request that was nonetheless
/// answered correctly. Panicking on that (as `raw_tunnel_request` does) would turn the deliverable
/// refusal the production code goes out of its way to guarantee into a spurious test failure.
async fn raw_tunnel_request_oversize(
    intercept: SocketAddr,
    ca: &str,
    host: &str,
    request: &str,
) -> (String, bool) {
    let mut tls = open_tls_tunnel(intercept, ca, host).await;
    // Deliberately unchecked: a short write here is an expected outcome, not a failure. What the
    // caller asserts on is the response that comes back regardless.
    let _ = tls.write_all(request.as_bytes()).await;

    let mut out = Vec::new();
    // The read outcome is returned rather than discarded purely so a failing caller can tell the
    // two ways an empty response happens apart: a server that hung to the deadline, and one that
    // answered and closed. Both render as the same empty string otherwise.
    let read_completed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tls.read_to_end(&mut out),
    )
    .await
    .is_ok();
    (String::from_utf8_lossy(&out).into_owned(), read_completed)
}

/// Build a chunked request body carrying exactly `decoded_len` bytes, in one chunk.
///
/// The cap applies to the *decoded* length, so the encoded frame is deliberately larger — that
/// difference is what distinguishes a limit applied to the wire bytes from one applied to the
/// body, and E4 below turns on it.
fn chunked_request(host: &str, decoded_len: usize) -> String {
    let body = "x".repeat(decoded_len);
    format!(
        "POST /upload HTTP/1.1\r\nHost: {host}\r\ntransfer-encoding: chunked\r\n\r\n\
         {decoded_len:x}\r\n{body}\r\n0\r\n\r\n"
    )
}

// AC1 (serve) / E1 / E6. The serve action never reads the request body, so this is the path a
// use-site cap would leak on.
#[tokio::test]
async fn an_oversize_body_on_a_serve_rule_is_refused_with_413() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","action":{"serve":{"statusCode":200,"body":"served"}}}"#,
    )
    .await;

    let resp = proxy_client(intercept, &ca)
        .post("https://cdn.example.com/upload")
        .body("x".repeat(MAX_BODY_BYTES + 1))
        .send()
        .await
        .expect("intercepted");

    assert_eq!(
        resp.status(),
        413,
        "the cap is enforced in the reader, so a serve rule that never looks at the body is \
         refused just the same"
    );
    // E6: the same framing discipline the 502 path is held to — declare an empty body rather than
    // leaning on close-framing, so a client reading content-length is not left waiting.
    assert_eq!(
        resp.headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok()),
        Some("0"),
        "413 declares an empty body"
    );
    assert_eq!(resp.text().await.expect("body"), "", "413 carries no body");
    server.shutdown().await;
}

// AC1 (no-rule-match) / E2. With no rule at all the listener answers its default 200; the cap must
// still bite first.
#[tokio::test]
async fn an_oversize_body_with_no_matching_rule_is_refused_with_413() {
    let server = start_intercept().await;
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(server.admin_addr()).await;
    // No rule added on purpose: this is the default path, which
    // `no_rule_match_still_answers_the_slice3_200` pins at 200 for an ordinary request.

    let resp = proxy_client(intercept, &ca)
        .post("https://cdn.example.com/upload")
        .body("x".repeat(MAX_BODY_BYTES + 1))
        .send()
        .await
        .expect("intercepted");

    assert_eq!(
        resp.status(),
        413,
        "the default no-rule-match path refuses an oversize body rather than answering its 200"
    );
    server.shutdown().await;
}

// AC2. The forward-path test above uses a dead port, so it proves only that no *successful*
// forward happened — a 502 and a refusal are distinguishable there, but "the imposter recorded
// nothing" is not something a dead port can attest to. This forwards at a live recording imposter
// and reads its request log back.
#[tokio::test]
async fn an_oversize_body_reaches_no_imposter() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    let http = reqwest::Client::new();

    // Port omitted so the manager auto-assigns a free one — a hardcoded port makes this test
    // collide with any other test binary running concurrently.
    let created: serde_json::Value = http
        .post(format!("http://{admin}/imposters"))
        .json(&serde_json::json!({
            "protocol": "http",
            "recordRequests": true,
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "from-imposter"}}]}]
        }))
        .send()
        .await
        .expect("create imposter")
        .json()
        .await
        .expect("imposter json");
    let imposter_port = created["port"].as_u64().expect("assigned port");

    add_rule(
        admin,
        &format!(
            r#"{{"host":"cdn.example.com","action":{{"forward":{{"port":{imposter_port}}}}}}}"#
        ),
    )
    .await;

    // Control: an ordinary request does reach the imposter, so a zero count below means the cap
    // stopped this one — not that the rule never worked.
    let ok = proxy_client(intercept, &ca)
        .post("https://cdn.example.com/upload")
        .body("small")
        .send()
        .await
        .expect("intercepted");
    assert_eq!(ok.status(), 200, "the forward rule reaches the imposter");
    assert_eq!(ok.text().await.expect("body"), "from-imposter");

    let oversize = proxy_client(intercept, &ca)
        .post("https://cdn.example.com/upload")
        .body("x".repeat(MAX_BODY_BYTES + 1))
        .send()
        .await
        .expect("intercepted");
    assert_eq!(oversize.status(), 413);

    let recorded: serde_json::Value = http
        .get(format!("http://{admin}/imposters/{imposter_port}"))
        .send()
        .await
        .expect("read imposter")
        .json()
        .await
        .expect("imposter json");
    let requests = recorded["requests"]
        .as_array()
        .expect("recordRequests is on, so the log is present");
    assert_eq!(
        requests.len(),
        1,
        "only the small request was forwarded; the oversize one never reached the imposter, \
         got {requests:?}"
    );
    assert_eq!(
        requests[0]["body"], "small",
        "and the one that did arrive is the small one, not a 1 MiB truncation of the other"
    );
    server.shutdown().await;
}

// AC3 / E3. Chunked framing declares no length up front, so a `content-length` pre-check cannot
// catch this — only counting bytes as they decode can.
#[tokio::test]
async fn an_oversize_chunked_body_is_refused_with_413() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","action":{"serve":{"statusCode":200,"body":"served"}}}"#,
    )
    .await;

    let (response, read_completed) = raw_tunnel_request_oversize(
        intercept,
        &ca,
        "cdn.example.com",
        &chunked_request("cdn.example.com", MAX_BODY_BYTES + 1),
    )
    .await;

    assert!(
        response.starts_with("HTTP/1.1 413"),
        "a chunked body over the cap is refused, not truncated (read to EOF: {read_completed}): {}",
        response.chars().take(120).collect::<String>()
    );
    server.shutdown().await;
}

// E4. The chunked counterpart of `a_body_at_the_cap_is_still_served`: the cap counts decoded
// bytes, so a frame whose encoding pushes it over the limit must still be accepted. At the cap the
// encoded frame is 1048589 bytes — over 1 MiB — so a limit applied to wire bytes really would
// refuse this, which is what makes the test discriminating rather than a duplicate of the
// content-length boundary test.
//
// This one drives the STRICT helper on purpose. `raw_tunnel_request_oversize` exists because an
// over-cap request is answered while the client is still writing, so a short write is expected
// there; at the cap the server consumes the whole body and the write must succeed. Tolerating a
// write failure here would trade a real assertion for nothing — an early close would show up as a
// read timeout rather than as the write error it is.
#[tokio::test]
async fn a_chunked_body_at_the_cap_is_still_served() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","action":{"serve":{"statusCode":200,"body":"accepted"}}}"#,
    )
    .await;

    let (response, _) = raw_tunnel_request(
        intercept,
        &ca,
        "cdn.example.com",
        &chunked_request("cdn.example.com", MAX_BODY_BYTES),
    )
    .await;

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "a chunked body of exactly the cap is accepted — the limit counts decoded bytes, not the \
         larger encoded frame: {}",
        response.chars().take(120).collect::<String>()
    );
    assert!(
        response.contains("accepted"),
        "and the stub was served: {}",
        response.chars().take(200).collect::<String>()
    );
    server.shutdown().await;
}

// E5. The direct anti-regression for "truncate, then match": a predicate that would match the
// first MiB must not fire, because the request is refused before the matcher is reached.
#[tokio::test]
async fn an_oversize_body_is_never_matched_against_a_body_predicate() {
    let server = start_intercept().await;
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("intercept bound");
    let ca = ca_pem(admin).await;
    // `contains` over a run of 'x' is satisfied by any 1 MiB prefix of the oversize body below, so
    // this rule fires for exactly the truncated-then-matched behaviour #995 was filed about.
    add_rule(
        admin,
        r#"{"host":"cdn.example.com","predicates":[{"contains":{"body":"xxxxx"}}],"action":{"serve":{"statusCode":200,"body":"matched-a-truncated-body"}}}"#,
    )
    .await;

    let resp = proxy_client(intercept, &ca)
        .post("https://cdn.example.com/upload")
        .body("x".repeat(MAX_BODY_BYTES + 1))
        .send()
        .await
        .expect("intercepted");

    // Status alone settles it: the stub answers 200 with its own marker body, so a 413 is only
    // reachable if the refusal happened before the matcher ran. A second assertion on the body
    // would read as an independent check but could never fail once this one holds — `413` is
    // built by `status_response`, which always carries an empty body.
    assert_eq!(
        resp.status(),
        413,
        "the matcher is never reached for an over-cap body, so the rule that would have matched \
         its first MiB never fires"
    );
    server.shutdown().await;
}
