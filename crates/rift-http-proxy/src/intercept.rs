//! Inbound forward-proxy intercept listener (epic #394, slice 3/5 + slice 4/5).
//!
//! An opt-in listener a SUT points at via `https.proxyHost`/`proxyPort`. It accepts HTTP
//! `CONNECT`, TLS-terminates the tunnel using the per-SNI cert resolver from slice 1
//! ([`SniCertResolver`]), and matches the decrypted request against an [`InterceptRules`] store
//! (slice 4): a matching rule either serves an inline stub or forwards to a named imposter port.
//! With no matching rule (including an empty store), the handler falls back to a fixed 200 that
//! echoes the intercepted host, so slice-3 behavior is unchanged by default.
//!
//! It is entirely opt-in: nothing runs until [`InterceptListener::bind`] is called, so the
//! default imposter-on-a-port model is unchanged.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::intercept_rules::{InterceptAction, InterceptRules, ServeStub};
use rift_mock_core::proxy::intercept_ca::SniCertResolver;
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Upper bound on an intercepted request body we will buffer before forwarding/matching. Bounds
/// memory use for a misbehaving or malicious `content-length`.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Per-stage deadline for the CONNECT read, TLS handshake, and request read. Bounds a slow or
/// silent client so its connection task cannot park indefinitely (slowloris).
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Backoff after a listener `accept()` error so a persistent failure (e.g. FD exhaustion) does
/// not spin the accept loop hot.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// The `host:port` a client asked to reach via `CONNECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectTarget {
    host: String,
    port: u16,
}

impl std::fmt::Display for ConnectTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// A running intercept listener. Dropping the handle does not stop it; call
/// [`InterceptListener::shutdown`] for a clean stop.
pub struct InterceptListener {
    local_addr: SocketAddr,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl InterceptListener {
    /// Bind an intercept listener on `addr` and start accepting connections. Use `127.0.0.1:0`
    /// to get an OS-assigned port (read it back via [`local_addr`](Self::local_addr)).
    ///
    /// `rules` is matched against every intercepted request (issue #398); an empty store falls
    /// back to the fixed slice-3 200 response.
    pub async fn bind(
        addr: SocketAddr,
        resolver: Arc<SniCertResolver>,
        rules: InterceptRules,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        let tls = build_tls_acceptor(resolver)?;
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, peer)) => {
                            let tls = tls.clone();
                            let rules = rules.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, tls, rules).await {
                                    tracing::debug!(%peer, error = %e, "intercept connection ended");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "intercept listener accept failed");
                            tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        }
                    },
                }
            }
        });

        Ok(Self {
            local_addr,
            shutdown_tx,
            handle,
        })
    }

    /// The address the listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Signal the accept loop to stop and wait for it to finish.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        log_accept_loop_exit(self.handle.await);
    }
}

/// Surface an abnormal exit of the accept-loop task (issue #522). The loop returns `()` and is
/// never aborted, so a `JoinError` here means it panicked — log it instead of silently discarding
/// the join result, which would let `shutdown`/`stop` report success over a crashed listener.
fn log_accept_loop_exit(result: Result<(), tokio::task::JoinError>) {
    if let Err(e) = result {
        tracing::warn!(error = %e, "intercept listener accept loop ended abnormally");
    }
}

fn build_tls_acceptor(resolver: Arc<SniCertResolver>) -> anyhow::Result<TlsAcceptor> {
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| anyhow::anyhow!("intercept TLS config: {e}"))?
            .with_no_client_auth()
            .with_cert_resolver(resolver);
    // Only HTTP/1.1 for now (non-goal: h2/websocket, see #394).
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn handle_connection(
    mut stream: TcpStream,
    tls: TlsAcceptor,
    rules: InterceptRules,
) -> anyhow::Result<()> {
    let head = timeout(IO_TIMEOUT, read_connect_head(&mut stream))
        .await
        .map_err(|_| anyhow::anyhow!("timed out reading CONNECT head"))??;
    let Some(target) = parse_connect(&head) else {
        stream
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nconnection: close\r\n\r\n")
            .await?;
        return Ok(());
    };

    stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;

    let mut tls_stream = match timeout(IO_TIMEOUT, tls.accept(stream)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            // A broken cert resolver (e.g. misconfigured intercept CA) fails EVERY handshake, so
            // log at warn — distinct from a client that simply closed early.
            tracing::warn!(%target, error = %e, "intercept TLS handshake failed");
            return Ok(());
        }
        Err(_) => {
            tracing::warn!(%target, "intercept TLS handshake timed out");
            return Ok(());
        }
    };

    let (head_bytes, leftover) = timeout(IO_TIMEOUT, read_request_message(&mut tls_stream))
        .await
        .map_err(|_| anyhow::anyhow!("timed out reading intercepted request head"))??;
    let (method, path, query, headers) = parse_request_head(&head_bytes);

    let content_length = headers
        .get("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok());
    // Diagnose the cases where a body silently won't be matched/forwarded (this slice reads only
    // content-length-framed bodies; chunked/streamed bodies are not decoded — see #394).
    if content_length.is_none() {
        if let Some(raw) = headers.get("content-length") {
            tracing::warn!(%target, content_length = %raw, "malformed content-length; treating request as bodyless");
        } else if headers.contains_key("transfer-encoding") {
            tracing::warn!(%target, "intercepted request uses transfer-encoding (e.g. chunked); body is not decoded and is treated as empty");
        }
    }
    let body_bytes = match content_length {
        Some(len) => {
            if len > MAX_BODY_BYTES {
                tracing::warn!(%target, len, cap = MAX_BODY_BYTES, "intercepted request body exceeds cap; truncating for match/forward");
            }
            Some(
                timeout(IO_TIMEOUT, read_body(&mut tls_stream, leftover, len))
                    .await
                    .map_err(|_| anyhow::anyhow!("timed out reading intercepted request body"))??,
            )
        }
        None => None,
    };
    let body = body_bytes
        .as_deref()
        .map(|b| String::from_utf8_lossy(b).into_owned());

    let action = rules.match_request(
        &target.host,
        &method,
        &path,
        query.as_deref(),
        &headers,
        body.as_deref(),
    );

    match action {
        Some(InterceptAction::Serve(stub)) => {
            if let Err(e) = write_stub_response(&mut tls_stream, &stub).await {
                tracing::warn!(%target, error = %e, "failed to render intercept stub response");
            }
        }
        Some(InterceptAction::Forward(forward)) => {
            let forward_result = forward_and_relay(
                &mut tls_stream,
                &method,
                &path,
                query.as_deref(),
                &headers,
                body_bytes.as_deref(),
                forward.port,
            )
            .await;
            if let Err(e) = forward_result {
                tracing::warn!(%target, port = forward.port, error = %e, "intercept forward failed");
                let _ = write_bad_gateway(&mut tls_stream).await;
            }
        }
        None => {
            let body = format!("rift intercepted {method} {path} for {}\n", target.host);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len(),
            );
            tls_stream.write_all(response.as_bytes()).await?;
        }
    }

    tls_stream.flush().await?;
    let _ = tls_stream.shutdown().await;
    Ok(())
}

/// Render an [`InterceptAction::Serve`] stub as an HTTP/1.1 response and write it out.
/// `content-length` and `connection` are always computed/set here, overriding any same-named
/// entry in `stub.headers`, so the response stays well-formed regardless of stub configuration.
async fn write_stub_response<S>(stream: &mut S, stub: &ServeStub) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let body = stub.body.clone().unwrap_or_default();
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        stub.status_code,
        reason_phrase(stub.status_code)
    );
    for (name, value) in &stub.headers {
        if is_hop_by_hop(name) {
            continue;
        }
        // Guard against header/response splitting via CR/LF in admin-supplied stub headers.
        if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            tracing::warn!(header = %name, "skipping intercept stub header containing CR/LF");
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("content-length: {}\r\n", body.len()));
    head.push_str("connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    Ok(())
}

/// Forward the decrypted request to `http://127.0.0.1:{port}{path}[?query]` and relay the
/// upstream status, headers, and body back over `stream`. Returns `Err` on any connection or I/O
/// failure so the caller can answer `502 Bad Gateway` without panicking.
async fn forward_and_relay<S>(
    stream: &mut S,
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &HashMap<String, String>,
    body: Option<&[u8]>,
    port: u16,
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let url = match query {
        Some(q) => format!("http://127.0.0.1:{port}{path}?{q}"),
        None => format!("http://127.0.0.1:{port}{path}"),
    };
    let reqwest_method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|e| anyhow::anyhow!("invalid method '{method}': {e}"))?;

    // Relay the imposter's own response verbatim: never follow redirects (a 3xx from the imposter
    // is a response to hand back, not to chase), and bound the forward by the same IO timeout.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(IO_TIMEOUT)
        .build()
        .map_err(|e| anyhow::anyhow!("building forward client: {e}"))?;
    let mut builder = client.request(reqwest_method, &url);
    for (name, value) in headers {
        if is_hop_by_hop(name) {
            continue;
        }
        builder = builder.header(name, value);
    }
    if let Some(bytes) = body {
        builder = builder.body(bytes.to_vec());
    }

    let upstream = builder
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("forward to 127.0.0.1:{port} failed: {e}"))?;

    let status = upstream.status();
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    let mut saw_content_length = false;
    for (name, value) in upstream.headers() {
        let name_str = name.as_str();
        if is_hop_by_hop(name_str) {
            continue;
        }
        if name_str.eq_ignore_ascii_case("content-length") {
            saw_content_length = true;
        }
        if let Ok(value_str) = value.to_str() {
            head.push_str(&format!("{name_str}: {value_str}\r\n"));
        }
    }

    let body_bytes = upstream
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("reading upstream body from 127.0.0.1:{port}: {e}"))?;
    if !saw_content_length {
        head.push_str(&format!("content-length: {}\r\n", body_bytes.len()));
    }
    head.push_str("connection: close\r\n\r\n");

    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body_bytes).await?;
    Ok(())
}

async fn write_bad_gateway<S>(stream: &mut S) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    stream
        .write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
        .await?;
    Ok(())
}

/// Hop-by-hop / connection-management headers we recompute ourselves rather than pass through
/// verbatim in either direction (request forwarding or response relaying).
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "connection" | "content-length" | "transfer-encoding"
    )
}

fn reason_phrase(status: u16) -> &'static str {
    hyper::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("")
}

/// Read the `CONNECT` request head one byte at a time, stopping exactly at the terminating
/// `\r\n\r\n`. Reading byte-by-byte avoids consuming any TLS ClientHello bytes that follow — the
/// client sends those only after our `200` response, but a buffered read could still over-read.
async fn read_connect_head(stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            anyhow::bail!("connection closed before CONNECT head completed");
        }
        buf.push(byte[0]);
        if buf.len() > MAX_HEAD_BYTES {
            anyhow::bail!("CONNECT head exceeds {MAX_HEAD_BYTES} bytes");
        }
    }
    Ok(buf)
}

/// Read the intercepted request's head (request line + headers), returning it split from any
/// bytes already read past the terminating `\r\n\r\n` (the start of the body, since reads happen
/// in fixed-size chunks and can over-read past the header boundary).
async fn read_request_message<S>(stream: &mut S) -> anyhow::Result<(Vec<u8>, Vec<u8>)>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("connection closed before request head completed");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            let leftover = buf.split_off(pos + 4);
            return Ok((buf, leftover));
        }
        if buf.len() > MAX_HEAD_BYTES {
            anyhow::bail!("request head exceeds {MAX_HEAD_BYTES} bytes");
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Read exactly `content_length` bytes of body, starting from bytes already buffered past the
/// header boundary (`leftover`). Capped at [`MAX_BODY_BYTES`] regardless of the claimed length.
async fn read_body<S>(
    stream: &mut S,
    leftover: Vec<u8>,
    content_length: usize,
) -> anyhow::Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let target_len = content_length.min(MAX_BODY_BYTES);
    let mut body = leftover;
    if body.len() > target_len {
        body.truncate(target_len);
        return Ok(body);
    }
    let mut chunk = [0u8; 8192];
    while body.len() < target_len {
        let want = (target_len - body.len()).min(chunk.len());
        let n = stream.read(&mut chunk[..want]).await?;
        if n == 0 {
            anyhow::bail!("connection closed before request body completed");
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(body)
}

fn parse_connect(head: &[u8]) -> Option<ConnectTarget> {
    let text = std::str::from_utf8(head).ok()?;
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let authority = parts.next().unwrap_or("");
    if !method.eq_ignore_ascii_case("CONNECT") || authority.is_empty() {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        // Reject a malformed port rather than silently defaulting — it signals a broken client.
        Some((h, p)) => (h, p.parse().ok()?),
        None => (authority, 443),
    };
    if host.is_empty() {
        return None;
    }
    Some(ConnectTarget {
        host: host.to_string(),
        port,
    })
}

/// Parse the request line and header block of an intercepted request into `(method, path, query,
/// headers)`. Header names are lowercased so lookups (e.g. `content-length`) are case-insensitive.
fn parse_request_head(head: &[u8]) -> (String, String, Option<String>, HashMap<String, String>) {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let raw_target = parts.next().unwrap_or("").to_string();
    let (path, query) = match raw_target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (raw_target, None),
    };

    let mut headers = HashMap::new();
    for line in lines {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    (method, path, query, headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intercept_rules::{ForwardTarget, InterceptRule};
    use rift_mock_core::proxy::intercept_ca::CertificateAuthority;

    // Issue #522: a panicked accept loop must not be swallowed by `shutdown`/`stop` — its
    // `JoinError` is logged rather than discarded.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn log_accept_loop_exit_warns_on_panic() {
        // A genuine `JoinError` from a panicked task (its only real source here).
        let joined = tokio::spawn(async { panic!("accept loop boom") }).await;
        assert!(joined.is_err(), "a panicked task yields a JoinError");
        log_accept_loop_exit(joined);
        assert!(
            logs_contain("intercept listener accept loop ended abnormally"),
            "an abnormal accept-loop exit is warned, not swallowed"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn log_accept_loop_exit_silent_on_normal_exit() {
        log_accept_loop_exit(Ok(()));
        assert!(
            !logs_contain("accept loop ended abnormally"),
            "a clean shutdown logs nothing"
        );
    }

    #[test]
    fn parse_connect_accepts_authority() {
        let t = parse_connect(b"CONNECT cdn.example.com:443 HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(t.host, "cdn.example.com");
        assert_eq!(t.port, 443);
    }

    #[test]
    fn parse_connect_defaults_port_and_rejects_malformed() {
        assert_eq!(parse_connect(b"CONNECT host\r\n\r\n").unwrap().port, 443);
        assert!(parse_connect(b"GET / HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT \r\n\r\n").is_none());
        assert!(parse_connect(b"\r\n\r\n").is_none());
        // A non-numeric port is a malformed request, not a default-to-443.
        assert!(parse_connect(b"CONNECT host:notaport HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn parse_request_head_splits_path_query_and_lowercases_headers() {
        let head = b"GET /a/b?x=1 HTTP/1.1\r\nHost: cdn.example.com\r\nX-Foo: Bar\r\n\r\n";
        let (method, path, query, headers) = parse_request_head(head);
        assert_eq!(method, "GET");
        assert_eq!(path, "/a/b");
        assert_eq!(query.as_deref(), Some("x=1"));
        assert_eq!(
            headers.get("host").map(String::as_str),
            Some("cdn.example.com")
        );
        assert_eq!(headers.get("x-foo").map(String::as_str), Some("Bar"));
    }

    async fn start_listener(rules: InterceptRules) -> (InterceptListener, String) {
        let ca = CertificateAuthority::generate().expect("ca");
        let ca_pem = ca.ca_cert_pem().to_string();
        let resolver = Arc::new(SniCertResolver::new(Arc::new(ca)));
        let listener = InterceptListener::bind("127.0.0.1:0".parse().unwrap(), resolver, rules)
            .await
            .expect("bind");
        (listener, ca_pem)
    }

    fn trusting_client(proxy_url: &str, ca_pem: &str) -> reqwest::Client {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(proxy_url).unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn intercepts_https_via_connect_and_trusts_minted_leaf() {
        let (listener, ca_pem) = start_listener(InterceptRules::new()).await;
        let proxy_url = format!("http://{}", listener.local_addr());

        // A client that trusts ONLY the intercept CA and routes HTTPS through the proxy. reqwest
        // issues CONNECT to the proxy, we MITM-terminate with a per-SNI leaf, and the client
        // validates that leaf against the CA it was handed.
        let client = trusting_client(&proxy_url, &ca_pem);

        let resp = client
            .get("https://cdn.example.com/config.json")
            .send()
            .await
            .expect("request intercepted");
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("cdn.example.com"),
            "response should echo the intercepted host, got: {body}"
        );

        listener.shutdown().await;
    }

    #[tokio::test]
    async fn non_connect_request_is_rejected_without_panic() {
        let (listener, _ca_pem) = start_listener(InterceptRules::new()).await;
        let addr = listener.local_addr();

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 405"), "got: {text}");

        listener.shutdown().await;
    }

    #[tokio::test]
    async fn tls_handshake_failure_is_handled_and_listener_survives() {
        let (listener, ca_pem) = start_listener(InterceptRules::new()).await;
        let addr = listener.local_addr();

        // A client that CONNECTs, reads the 200, then sends non-TLS garbage. The server-side
        // handshake must fail without panicking or taking the listener down.
        {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(b"CONNECT cdn.example.com:443 HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut buf = [0u8; 64];
            let _ = s.read(&mut buf).await.unwrap();
            s.write_all(b"this is not a TLS ClientHello").await.unwrap();
            let _ = s.shutdown().await;
        }

        // The listener still serves a subsequent legitimate intercept.
        let proxy_url = format!("http://{addr}");
        let client = trusting_client(&proxy_url, &ca_pem);
        let resp = client
            .get("https://cdn.example.com/still-up")
            .send()
            .await
            .expect("listener should still serve after a failed handshake");
        assert_eq!(resp.status(), 200);

        listener.shutdown().await;
    }

    #[tokio::test]
    async fn serve_rule_returns_inline_stub() {
        let rules = InterceptRules::new();
        rules.add(InterceptRule {
            host: Some("cdn.example.com".to_string()),
            predicates: vec![],
            action: InterceptAction::Serve(ServeStub {
                status_code: 418,
                headers: HashMap::from([("x-rift".to_string(), "1".to_string())]),
                body: Some("brewed".to_string()),
            }),
        });
        let (listener, ca_pem) = start_listener(rules).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        let resp = client
            .get("https://cdn.example.com/x")
            .send()
            .await
            .expect("request intercepted");
        assert_eq!(resp.status(), 418);
        assert_eq!(resp.headers().get("x-rift").unwrap(), "1");
        let body = resp.text().await.unwrap();
        assert!(body.contains("brewed"), "got: {body}");

        listener.shutdown().await;
    }

    #[tokio::test]
    async fn forward_rule_proxies_to_imposter_port() {
        // A trivial local HTTP server standing in for an imposter.
        let imposter = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let imposter_port = imposter.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = imposter.accept().await {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let body = "from-imposter";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            }
        });

        let rules = InterceptRules::new();
        rules.add(InterceptRule {
            host: None,
            predicates: vec![],
            action: InterceptAction::Forward(ForwardTarget {
                port: imposter_port,
            }),
        });
        let (listener, ca_pem) = start_listener(rules).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        let resp = client
            .get("https://cdn.example.com/anything")
            .send()
            .await
            .expect("request intercepted");
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "from-imposter");

        listener.shutdown().await;
    }

    #[tokio::test]
    async fn no_matching_rule_falls_back_to_default() {
        let (listener, ca_pem) = start_listener(InterceptRules::new()).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        let resp = client
            .get("https://cdn.example.com/whatever")
            .send()
            .await
            .expect("request intercepted");
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(body.contains("cdn.example.com"), "got: {body}");

        listener.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_forward_port_returns_502() {
        // Bind then immediately drop a listener to get a port that is very likely closed for
        // the lifetime of the test.
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);

        let rules = InterceptRules::new();
        rules.add(InterceptRule {
            host: None,
            predicates: vec![],
            action: InterceptAction::Forward(ForwardTarget { port: closed_port }),
        });
        let (listener, ca_pem) = start_listener(rules).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        let resp = client
            .get("https://cdn.example.com/x")
            .send()
            .await
            .expect("request intercepted");
        assert_eq!(resp.status(), 502);

        listener.shutdown().await;
    }
}
