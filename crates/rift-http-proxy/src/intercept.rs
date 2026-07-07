//! Inbound forward-proxy intercept listener (epic #394, slice 3/5).
//!
//! An opt-in listener a SUT points at via `https.proxyHost`/`proxyPort`. It accepts HTTP
//! `CONNECT`, TLS-terminates the tunnel using the per-SNI cert resolver from slice 1
//! ([`SniCertResolver`]), and hands the decrypted request to a minimal handler. Predicate-based
//! redirect/serve of intercepted requests is slice 4; here the handler proves end-to-end
//! termination by answering with a fixed 200.
//!
//! It is entirely opt-in: nothing runs until [`InterceptListener::bind`] is called, so the
//! default imposter-on-a-port model is unchanged.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rift_core::proxy::intercept_ca::SniCertResolver;
use rustls::ServerConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

const MAX_HEAD_BYTES: usize = 16 * 1024;
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
    pub async fn bind(addr: SocketAddr, resolver: Arc<SniCertResolver>) -> anyhow::Result<Self> {
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
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, tls).await {
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
        let _ = self.handle.await;
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

async fn handle_connection(mut stream: TcpStream, tls: TlsAcceptor) -> anyhow::Result<()> {
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

    let request_head = timeout(IO_TIMEOUT, read_request_head(&mut tls_stream))
        .await
        .map_err(|_| anyhow::anyhow!("timed out reading intercepted request head"))??;
    let (method, path) = parse_request_line(&request_head);

    let body = format!("rift intercepted {method} {path} for {}\n", target.host);
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len(),
    );
    tls_stream.write_all(response.as_bytes()).await?;
    tls_stream.flush().await?;
    let _ = tls_stream.shutdown().await;
    Ok(())
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

async fn read_request_head<S>(stream: &mut S) -> anyhow::Result<Vec<u8>>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("connection closed before request head completed");
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEAD_BYTES {
            anyhow::bail!("request head exceeds {MAX_HEAD_BYTES} bytes");
        }
    }
    Ok(buf)
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

fn parse_request_line(head: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(head);
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    (method, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_core::proxy::intercept_ca::CertificateAuthority;

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

    async fn start_listener() -> (InterceptListener, String) {
        let ca = CertificateAuthority::generate().expect("ca");
        let ca_pem = ca.ca_cert_pem().to_string();
        let resolver = Arc::new(SniCertResolver::new(Arc::new(ca)));
        let listener = InterceptListener::bind("127.0.0.1:0".parse().unwrap(), resolver)
            .await
            .expect("bind");
        (listener, ca_pem)
    }

    #[tokio::test]
    async fn intercepts_https_via_connect_and_trusts_minted_leaf() {
        let (listener, ca_pem) = start_listener().await;
        let proxy_url = format!("http://{}", listener.local_addr());

        // A client that trusts ONLY the intercept CA and routes HTTPS through the proxy. reqwest
        // issues CONNECT to the proxy, we MITM-terminate with a per-SNI leaf, and the client
        // validates that leaf against the CA it was handed.
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(&proxy_url).unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
            .build()
            .unwrap();

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
        let (listener, _ca_pem) = start_listener().await;
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
        let (listener, ca_pem) = start_listener().await;
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
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(&proxy_url).unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
            .build()
            .unwrap();
        let resp = client
            .get("https://cdn.example.com/still-up")
            .send()
            .await
            .expect("listener should still serve after a failed handshake");
        assert_eq!(resp.status(), 200);

        listener.shutdown().await;
    }
}
