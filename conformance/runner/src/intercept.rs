//! What feature 12 drives the intercept / TLS-MITM listener with (port of `Intercept12Suite.scala`,
//! `RawHttpConnection.tunnel` and `WsEchoOrigin.scala`): the path a real client takes through it,
//! `CONNECT host:443` then TLS over the tunnel trusting the intercept CA; the truststores a client
//! would load that CA from; and a TLS WebSocket echo origin for the passthrough relay.

use crate::tls;
use crate::wire::{self, RawConnection};
use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// `CONNECT target:target_port` through the proxy at `proxy_host:proxy_port`, then a TLS handshake
/// over the tunnel for `sni` (`target` unless given; also the name the chain is verified against)
/// trusting only `roots`, offering `alpn`. `connect_headers` ride on the `CONNECT` itself. Fails
/// unless the proxy answers the `CONNECT` with `200`.
#[allow(clippy::too_many_arguments)]
pub async fn tunnel(
    proxy_host: &str,
    proxy_port: u16,
    target: &str,
    target_port: u16,
    roots: &[CertificateDer<'static>],
    alpn: &[&str],
    connect_headers: &[(&str, &str)],
    sni: Option<&str>,
) -> anyhow::Result<RawConnection> {
    let mut tcp = wire::connect(proxy_host, proxy_port).await?;
    let connect = wire::connect_request(target, target_port, connect_headers);
    tokio::time::timeout(wire::WIRE_TIMEOUT, async {
        tcp.write_all(&connect).await?;
        tcp.flush().await
    })
    .await
    .map_err(|_| anyhow!("writing the CONNECT took over {:?}", wire::WIRE_TIMEOUT))?
    .context("writing the CONNECT")?;
    let status = tokio::time::timeout(wire::WIRE_TIMEOUT, wire::read_connect_reply(&mut tcp))
        .await
        .map_err(|_| anyhow!("no CONNECT reply within {:?}", wire::WIRE_TIMEOUT))??;
    if status != 200 {
        bail!("CONNECT {target}:{target_port} -> {status}, not 200");
    }
    let config = tls::client_config(roots, alpn)?;
    tls::handshake(Box::new(tcp), sni.unwrap_or(target), config).await
}

/// `Basic <base64(credentials)>`, a `Proxy-Authorization` value.
#[must_use]
pub fn basic_auth(credentials: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(credentials)
    )
}

/// The certificates a PKCS#12 truststore holds, after its MAC is checked against `password`: what
/// a client loading the exported `truststore.p12` would trust.
pub fn pkcs12_certificates(
    bytes: &[u8],
    password: &str,
) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let pfx = p12::PFX::parse(bytes).map_err(|e| anyhow!("not a PKCS#12 truststore: {e:?}"))?;
    if !pfx.verify_mac(password) {
        bail!("the PKCS#12 truststore's MAC does not verify with password {password:?}");
    }
    let certs = pfx
        .cert_x509_bags(password)
        .map_err(|e| anyhow!("reading the PKCS#12 certificates: {e:?}"))?;
    if certs.is_empty() {
        bail!("the PKCS#12 truststore holds no certificate");
    }
    Ok(certs.into_iter().map(CertificateDer::from).collect())
}

/// The trusted certificates of a JKS keystore (magic `FEEDFEED`), after its integrity digest is
/// checked against `password`, as the JDK's `KeyStore.load` does.
pub fn jks_trusted_certificates(bytes: &[u8], password: &str) -> anyhow::Result<Vec<Vec<u8>>> {
    const DIGEST_LEN: usize = 20;
    if bytes.len() < 12 + DIGEST_LEN {
        bail!("a {}-byte JKS keystore is too short", bytes.len());
    }
    let (data, digest) = bytes.split_at(bytes.len() - DIGEST_LEN);
    // SHA-1 over the password as UTF-16BE, the JDK's whitener, then the keystore body.
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    for unit in password.encode_utf16() {
        ctx.update(&unit.to_be_bytes());
    }
    ctx.update(b"Mighty Aphrodite");
    ctx.update(data);
    if ctx.finish().as_ref() != digest {
        bail!("the JKS keystore's digest does not verify with password {password:?}");
    }
    let mut r = Reader { bytes: data, at: 0 };
    let magic = r.u32()?;
    if magic != 0xFEED_FEED {
        bail!("JKS magic {magic:08X}, expected FEEDFEED");
    }
    let version = r.u32()?;
    if version != 2 {
        bail!("JKS version {version}, expected 2");
    }
    let count = r.u32()?;
    let mut certs = Vec::new();
    for _ in 0..count {
        match r.u32()? {
            // A trusted certificate: alias, timestamp, type, length-prefixed DER.
            2 => {
                r.utf()?;
                r.take(8)?;
                let kind = r.utf()?;
                if kind != "X.509" {
                    bail!("a JKS certificate of type {kind:?}");
                }
                let len = usize::try_from(r.u32()?).context("a JKS certificate length")?;
                certs.push(r.take(len)?.to_vec());
            }
            tag => bail!("a JKS entry with tag {tag}: a truststore holds certificates only"),
        }
    }
    Ok(certs)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&e| e <= self.bytes.len())
            .ok_or_else(|| anyhow!("the JKS keystore ends inside an entry"))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn utf(&mut self) -> anyhow::Result<String> {
        let b = self.take(2)?;
        let len = usize::from(u16::from_be_bytes([b[0], b[1]]));
        String::from_utf8(self.take(len)?.to_vec()).context("a JKS string")
    }
}

// ── WebSocket (RFC 6455) ─────────────────────────────────────────────────────────────────────

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The `Sec-WebSocket-Accept` value for a handshake `key`.
#[must_use]
pub fn ws_accept(key: &str) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}{WS_GUID}").as_bytes(),
    );
    base64::engine::general_purpose::STANDARD.encode(digest.as_ref())
}

/// A masked (client-to-server) text frame of under 126 bytes.
pub fn client_text_frame(text: &str) -> anyhow::Result<Vec<u8>> {
    let payload = text.as_bytes();
    let len = u8::try_from(payload.len())
        .ok()
        .filter(|&n| n < 126)
        .ok_or_else(|| {
            anyhow!(
                "only short frames are needed here, not {} bytes",
                payload.len()
            )
        })?;
    let mask = [1u8, 2, 3, 4];
    let mut frame = vec![0x81, 0x80 | len];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
    Ok(frame)
}

/// Read one frame off an upgraded connection: `(opcode, payload)`, unmasking a masked one.
pub async fn read_frame(conn: &mut RawConnection) -> anyhow::Result<(u8, Vec<u8>)> {
    let head = conn.read_exact(2).await?;
    let len = match head[1] & 0x7f {
        126 => {
            let b = conn.read_exact(2).await?;
            usize::from(u16::from_be_bytes([b[0], b[1]]))
        }
        127 => {
            let b = conn.read_exact(8).await?;
            let mut n = [0u8; 8];
            n.copy_from_slice(&b);
            usize::try_from(u64::from_be_bytes(n)).context("a WebSocket frame length")?
        }
        n => usize::from(n),
    };
    let mask = if head[1] & 0x80 != 0 {
        Some(conn.read_exact(4).await?)
    } else {
        None
    };
    let mut payload = conn.read_exact(len).await?;
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Ok((head[0] & 0x0f, payload))
}

/// A minimal TLS WebSocket echo origin: the real origin an unmatched `Upgrade: websocket`
/// handshake is relayed to through the intercept tunnel. It serves a leaf for `localhost`, answers
/// every handshake with `101`, and echoes each text frame back unmasked until the peer closes. It
/// binds a loopback port itself and holds it, so there is no probe-then-bind window. Dropping it
/// stops it.
pub struct WsEchoOrigin {
    pub port: u16,
    failure: Arc<Mutex<Option<String>>>,
    acceptor: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for WsEchoOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsEchoOrigin")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Drop for WsEchoOrigin {
    fn drop(&mut self) {
        self.acceptor.abort();
    }
}

impl WsEchoOrigin {
    /// Serve `cert_pem` (the chain) with `key_pem` (PKCS#8) on a fresh loopback port.
    pub async fn start(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<Self> {
        let chain = tls::certs_from_pem(cert_pem)?;
        let key = PrivateKeyDer::from_pem_slice(key_pem).context("the origin's private key")?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("TLS protocol versions")?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .context("the origin's certificate and key")?;
        let tls = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("binding the WebSocket origin")?;
        let port = listener
            .local_addr()
            .context("the WebSocket origin's address")?
            .port();
        let failure = Arc::new(Mutex::new(None));
        let failed = Arc::clone(&failure);
        let acceptor = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((tcp, _)) = listener.accept().await {
                let (tls, failed) = (tls.clone(), Arc::clone(&failed));
                connections.spawn(async move {
                    if let Err(e) = serve(tls, tcp).await {
                        // The first failure only: it is the one that explains a tunnel closing.
                        if let Ok(mut slot) = failed.lock() {
                            slot.get_or_insert_with(|| format!("{e:#}"));
                        }
                    }
                });
            }
        });
        Ok(Self {
            port,
            failure,
            acceptor,
        })
    }

    /// The first failure while serving a connection, if any: why the origin gave up on a tunnel.
    #[must_use]
    pub fn last_error(&self) -> String {
        match self.failure.lock() {
            Ok(slot) => slot.clone().unwrap_or_else(|| "none".to_string()),
            Err(_) => "(the origin's error slot is poisoned)".to_string(),
        }
    }
}

/// One origin connection: the TLS handshake, the upgrade, then echo until the peer goes away (an
/// EOF or reset between frames is a normal end).
async fn serve(tls: tokio_rustls::TlsAcceptor, tcp: tokio::net::TcpStream) -> anyhow::Result<()> {
    let mut stream = tokio::time::timeout(wire::WIRE_TIMEOUT, tls.accept(tcp))
        .await
        .map_err(|_| {
            anyhow!(
                "the origin's TLS handshake took over {:?}",
                wire::WIRE_TIMEOUT
            )
        })?
        .context("the origin's TLS handshake")?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream
            .read(&mut byte)
            .await
            .context("reading the upgrade request")?
            == 0
        {
            bail!("closed before the handshake");
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let key = text
        .split("\r\n")
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim().to_string())
        })
        .ok_or_else(|| anyhow!("no Sec-WebSocket-Key in: {text}"))?;
    let reply = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        ws_accept(&key)
    );
    stream.write_all(reply.as_bytes()).await?;
    stream.flush().await?;
    loop {
        let mut b = [0u8; 2];
        if stream.read_exact(&mut b).await.is_err() {
            return Ok(());
        }
        let len = usize::from(b[1] & 0x7f);
        if len >= 126 {
            bail!("only short frames are echoed, got a length byte of {len}");
        }
        let mut mask = [0u8; 4];
        if b[1] & 0x80 != 0 {
            stream.read_exact(&mut mask).await?;
        }
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).await?;
        for (i, p) in payload.iter_mut().enumerate() {
            *p ^= mask[i % 4];
        }
        if b[0] & 0x0f != 1 {
            return Ok(());
        }
        let mut frame = vec![0x81, b[1] & 0x7f];
        frame.extend(payload);
        stream.write_all(&frame).await?;
        stream.flush().await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rfc_6455_sample_key_has_the_rfc_accept() {
        assert_eq!(
            ws_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn a_client_frame_is_masked() {
        let f = client_text_frame("ab").expect("a short frame");
        assert_eq!(f, [0x81, 0x82, 1, 2, 3, 4, b'a' ^ 1, b'b' ^ 2]);
        assert!(client_text_frame(&"x".repeat(126)).is_err());
    }

    #[test]
    fn basic_auth_encodes_the_credentials() {
        assert_eq!(basic_auth("u:p"), "Basic dTpw");
    }

    /// A JKS truststore built here the way `keytool` writes one: one trusted certificate.
    fn jks(cert: &[u8], password: &str) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(0xFEED_FEEDu32.to_be_bytes());
        body.extend(2u32.to_be_bytes());
        body.extend(1u32.to_be_bytes());
        body.extend(2u32.to_be_bytes());
        body.extend(2u16.to_be_bytes());
        body.extend(b"ca");
        body.extend(0u64.to_be_bytes());
        body.extend(5u16.to_be_bytes());
        body.extend(b"X.509");
        body.extend(u32::try_from(cert.len()).expect("small").to_be_bytes());
        body.extend(cert);
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
        for unit in password.encode_utf16() {
            ctx.update(&unit.to_be_bytes());
        }
        ctx.update(b"Mighty Aphrodite");
        ctx.update(&body);
        body.extend(ctx.finish().as_ref());
        body
    }

    #[test]
    fn a_jks_truststore_yields_its_certificate_only_under_its_password() {
        let store = jks(b"DER", "s3cret");
        assert_eq!(
            jks_trusted_certificates(&store, "s3cret").expect("valid"),
            [b"DER".to_vec()]
        );
        assert!(jks_trusted_certificates(&store, "wrong").is_err());
        let mut bad_magic = store.clone();
        bad_magic[0] = 0;
        assert!(jks_trusted_certificates(&bad_magic, "s3cret").is_err());
    }

    #[tokio::test]
    async fn the_echo_origin_upgrades_and_echoes_through_a_tls_client() {
        crate::http::install_crypto_provider();
        let fixtures = crate::spec::fixtures_dir();
        let read = |p: &str| std::fs::read(fixtures.join(p)).expect("a committed fixture");
        let origin = WsEchoOrigin::start(&read("tls/server-cert.pem"), &read("tls/server-key.pem"))
            .await
            .expect("an origin");
        let roots = tls::certs_from_pem(&read("intercept/ca-cert.pem")).expect("the test CA");
        let config = tls::client_config(&roots, tls::ALPN_HTTP1).expect("a config");
        let tcp = wire::connect("127.0.0.1", origin.port)
            .await
            .expect("a connect");
        let mut conn = tls::handshake(Box::new(tcp), "localhost", config)
            .await
            .expect("a handshake");
        let req = wire::request(
            "GET",
            "localhost",
            "/echo",
            &[
                ("Upgrade", "websocket"),
                ("Connection", "Upgrade"),
                ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ("Sec-WebSocket-Version", "13"),
            ],
            b"",
        );
        let wire::RawRead::Response(r) = conn.upgrade(&req).await.expect("an answer") else {
            panic!("the origin closed: {}", origin.last_error());
        };
        assert_eq!(r.status, 101);
        assert_eq!(
            r.header("sec-websocket-accept"),
            ["s3pPLMBiTxaQ9kYGzzhZRbK+xOo="]
        );
        conn.write_raw(&client_text_frame("ping").expect("a frame"))
            .await
            .expect("a write");
        assert_eq!(
            read_frame(&mut conn).await.expect("an echo"),
            (1, b"ping".to_vec())
        );
    }
}
