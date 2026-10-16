//! What the HTTP client cannot show: a request written on a bare socket and every byte read back
//! until the peer closes (feature 03's tcp faults), a two-stream HTTP/2 exchange on one cleartext
//! connection (rift#1234), a body larger than the admin cap sent while the answer is read
//! (feature 21), and a bare TCP connect. Ports of `RiftConformanceSupport.rawCapture`,
//! `H2cProbe.scala` and `RiftConformanceSupport.sendOversize`.
//!
//! Everything here is blocking `std::net` I/O: callers run it on `spawn_blocking`.

use crate::http::Response;
use anyhow::{Context as _, anyhow, bail};
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Connect and read budget of a raw capture and an h2c exchange (the Scala runner's 5 s).
const RAW_TIMEOUT: Duration = Duration::from_secs(5);

/// The whole budget of an over-cap send (the Scala runner's `SendDeadlineMillis`).
const OVERSIZE_DEADLINE: Duration = Duration::from_secs(30);

/// Connect to the first address of `host:port` that accepts within `timeout`.
fn connect(host: &str, port: u16, timeout: Duration) -> std::io::Result<TcpStream> {
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(
            ErrorKind::NotFound,
            format!("{host} resolved to no address"),
        )
    }))
}

/// One bare TCP connect: `Ok(true)` accepted, `Ok(false)` refused. Any other failure (a timeout, a
/// name that does not resolve) is an error, never a refusal.
pub fn tcp_connect_accepted(host: &str, port: u16) -> anyhow::Result<bool> {
    match connect(host, port, Duration::from_secs(2)) {
        Ok(s) => {
            drop(s);
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => Ok(false),
        Err(e) => Err(anyhow::Error::new(e).context(format!("connecting to {host}:{port}"))),
    }
}

// ── raw capture ──────────────────────────────────────────────────────────────────────────────

/// What one request written on a bare socket read back: every byte before the peer closed or
/// reset, whether it did close (a read timeout is not a close), and over TLS whether the
/// handshake completed first.
#[derive(Debug, Clone, Default)]
pub struct RawCapture {
    pub bytes: Vec<u8>,
    pub tls_handshake: Option<bool>,
    pub closed: bool,
    /// The read failure that ended the capture: a reset, a TLS failure, or "no close within 5s".
    pub error: Option<String>,
}

impl RawCapture {
    /// The bytes as ISO-8859-1 text, one char per byte, as the Scala runner read them.
    #[must_use]
    pub fn text(&self) -> String {
        self.bytes.iter().map(|&b| char::from(b)).collect()
    }

    /// Whether the capture shows `outcome`, or `None` for an outcome this runner does not know.
    /// A reset is an RST (the read fails "connection reset"), an empty reply a clean EOF with zero
    /// bytes, garbage bytes that are not an HTTP response, and a broken chunk a `200` chunked head
    /// followed by a chunk-size line that is not hex.
    #[must_use]
    pub fn shows(&self, outcome: &str) -> Option<bool> {
        let text = self.text();
        match outcome {
            "connection reset" => Some(
                self.bytes.is_empty()
                    && self.closed
                    && self
                        .error
                        .as_ref()
                        .is_some_and(|e| e.to_lowercase().contains("reset")),
            ),
            "empty reply" => Some(self.bytes.is_empty() && self.closed && self.error.is_none()),
            "garbage bytes" => {
                Some(!self.bytes.is_empty() && self.closed && !text.starts_with("HTTP/"))
            }
            "200 head then a broken chunk" => {
                let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                let size_line = rest.split(['\r', '\n']).next().unwrap_or_default().trim();
                Some(
                    self.closed
                        && head.starts_with("HTTP/1.1 200")
                        && head.to_lowercase().contains("transfer-encoding: chunked")
                        && !size_line.is_empty()
                        && !is_chunk_size(size_line),
                )
            }
            _ => None,
        }
    }
}

/// `[0-9a-fA-F]+(;.*)?`: a well-formed chunk-size line.
fn is_chunk_size(line: &str) -> bool {
    let size = line.split_once(';').map_or(line, |(s, _)| s);
    !size.is_empty() && size.chars().all(|c| c.is_ascii_hexdigit())
}

/// Write `METHOD path HTTP/1.1` with `Host: host_header` to `host:port` (over TLS, trusting any
/// certificate, when `tls`) and read until the peer closes. A connect that fails is an error: no
/// fault kind refuses the TCP connect itself.
pub fn raw_capture(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    host_header: &str,
    tls: bool,
) -> anyhow::Result<RawCapture> {
    let sock =
        connect(host, port, RAW_TIMEOUT).with_context(|| format!("connecting to {host}:{port}"))?;
    sock.set_read_timeout(Some(RAW_TIMEOUT))
        .context("setting the read timeout")?;
    sock.set_write_timeout(Some(RAW_TIMEOUT))
        .context("setting the write timeout")?;
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n");
    let mut cap = RawCapture::default();
    let ended = if tls {
        let config = trust_all_tls()?;
        let name = rustls::pki_types::ServerName::try_from(host.to_string())
            .with_context(|| format!("TLS server name {host}"))?;
        let conn = rustls::ClientConnection::new(config, name).context("a TLS client")?;
        let mut stream = rustls::StreamOwned::new(conn, sock);
        cap.tls_handshake = Some(false);
        let handshake = complete_handshake(&mut stream);
        match handshake {
            Ok(()) => {
                cap.tls_handshake = Some(true);
                exchange(&mut stream, request.as_bytes(), &mut cap.bytes)
            }
            Err(e) => Err(e),
        }
    } else {
        let mut sock = sock;
        exchange(&mut sock, request.as_bytes(), &mut cap.bytes)
    };
    match ended {
        Ok(()) => cap.closed = true,
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            cap.error = Some(format!("no close within 5s: {e}"));
        }
        // A reset, or a TLS read failing on one, is the peer closing the connection.
        Err(e) => {
            cap.closed = true;
            cap.error = Some(format!("{:?}: {e}", e.kind()));
        }
    }
    Ok(cap)
}

fn complete_handshake(
    stream: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
) -> std::io::Result<()> {
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock)?;
    }
    Ok(())
}

/// Write `request`, then read into `into` until EOF.
fn exchange(
    stream: &mut impl ReadWrite,
    request: &[u8],
    into: &mut Vec<u8>,
) -> std::io::Result<()> {
    stream.write_all(request)?;
    stream.flush()?;
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk)? {
            0 => return Ok(()),
            n => into.extend_from_slice(&chunk[..n]),
        }
    }
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// A TLS client config trusting any server certificate: the imposters serve rift's self-signed
/// default, and these scenarios are about what happens after the handshake.
fn trust_all_tls() -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustAll(provider)))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct TrustAll(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for TrustAll {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

// ── h2c ──────────────────────────────────────────────────────────────────────────────────────

/// RST_STREAM / GOAWAY error code INTERNAL_ERROR (RFC 9113 §7).
pub const H2_INTERNAL_ERROR: u32 = 2;

/// How one HTTP/2 stream ended, as seen on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum H2Outcome {
    /// The server answered: `status` is known only when HPACK sent it as one indexed byte.
    Answered { status: Option<u16>, body: String },
    /// The server reset the stream with `RST_STREAM` carrying this error code.
    Reset(u32),
    /// The connection ended (GOAWAY, EOF, timeout) before the stream did.
    ConnectionLost(String),
}

/// The static-table `:status` values HPACK can send as one indexed byte (RFC 7541 appendix A).
fn indexed_status(index: u8) -> Option<u16> {
    match index {
        8 => Some(200),
        9 => Some(204),
        10 => Some(206),
        11 => Some(304),
        12 => Some(400),
        13 => Some(404),
        14 => Some(500),
        _ => None,
    }
}

pub(crate) fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let len = u32::try_from(payload.len()).context("frame payload length")?;
    if len >= 1 << 24 {
        bail!("frame payload of {len} bytes is over HTTP/2's 24-bit length");
    }
    let mut f = Vec::with_capacity(9 + payload.len());
    f.extend_from_slice(&len.to_be_bytes()[1..]);
    f.push(kind);
    f.push(flags);
    f.extend_from_slice(&stream.to_be_bytes());
    f.extend_from_slice(payload);
    Ok(f)
}

/// A literal header field without indexing, its name from static-table `index`, its value raw
/// (not Huffman-coded): enough for values under 127 bytes.
fn literal(index: u8, value: &str) -> anyhow::Result<Vec<u8>> {
    let v = value.as_bytes();
    let len = u8::try_from(v.len())
        .ok()
        .filter(|&n| n < 127)
        .ok_or_else(|| anyhow!("header value too long for this probe: {value}"))?;
    let mut out = vec![index, len];
    out.extend_from_slice(v);
    Ok(out)
}

/// `:scheme http`, indexed (static-table entry 6).
pub(crate) const SCHEME_HTTP: u8 = 0x86;
/// `:scheme https`, indexed (static-table entry 7).
pub(crate) const SCHEME_HTTPS: u8 = 0x87;

/// The `:status` a HEADERS block opens with, when HPACK sent it as one indexed byte. Dynamic-table
/// size updates (`001xxxxx`, RFC 7541 §6.3) may come first and are skipped; anything else leaves
/// the status unknown, which fails an assertion rather than guessing.
pub(crate) fn indexed_status_of(block: &[u8]) -> Option<u16> {
    let mut rest = block;
    while let Some((&b, tail)) = rest.split_first() {
        if b & 0xe0 != 0x20 {
            return (b & 0x80 != 0).then(|| indexed_status(b & 0x7f)).flatten();
        }
        rest = tail;
        // A 5-bit prefix of all ones continues in 7-bit groups while the high bit is set.
        if b & 0x1f == 0x1f {
            let continued = rest.iter().position(|&c| c & 0x80 == 0)?;
            rest = &rest[continued + 1..];
        }
    }
    None
}

/// `:method GET` (2) and `scheme` indexed, then `:path` (4) and `:authority` (1).
pub(crate) fn request_headers(scheme: u8, path: &str, authority: &str) -> anyhow::Result<Vec<u8>> {
    let mut block = vec![0x82, scheme];
    block.extend(literal(4, path)?);
    block.extend(literal(1, authority)?);
    Ok(block)
}

/// A cleartext HTTP/2 (prior-knowledge h2c) client just rich enough to show what a server does to
/// two requests sent one after the other on ONE connection: GET `first`, wait for its stream to
/// end, then GET `second`. Hand-encoded frames, HPACK from the static table only.
pub fn h2c_two_streams(
    host: &str,
    port: u16,
    authority: &str,
    first: &str,
    second: &str,
) -> anyhow::Result<(H2Outcome, H2Outcome)> {
    let mut sock =
        connect(host, port, RAW_TIMEOUT).with_context(|| format!("connecting to {host}:{port}"))?;
    sock.set_read_timeout(Some(RAW_TIMEOUT))
        .context("setting the read timeout")?;
    sock.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .context("writing the h2c preface")?;
    sock.write_all(&frame(4, 0, 0, &[])?)
        .context("writing SETTINGS")?;
    sock.flush().context("flushing the preface")?;
    let a = h2_exchange(
        &mut sock,
        1,
        &request_headers(SCHEME_HTTP, first, authority)?,
    )?;
    let b = match a {
        H2Outcome::ConnectionLost(_) => {
            H2Outcome::ConnectionLost("connection already lost".to_string())
        }
        _ => h2_exchange(
            &mut sock,
            3,
            &request_headers(SCHEME_HTTP, second, authority)?,
        )?,
    };
    Ok((a, b))
}

/// One prior-knowledge h2c `GET path` (feature 20): the preface, an empty SETTINGS and one HEADERS
/// frame, then frames until stream 1 ends.
pub fn h2c_get(host: &str, port: u16, authority: &str, path: &str) -> anyhow::Result<H2Outcome> {
    let mut sock =
        connect(host, port, RAW_TIMEOUT).with_context(|| format!("connecting to {host}:{port}"))?;
    sock.set_read_timeout(Some(RAW_TIMEOUT))
        .context("setting the read timeout")?;
    sock.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .context("writing the h2c preface")?;
    sock.write_all(&frame(4, 0, 0, &[])?)
        .context("writing SETTINGS")?;
    sock.flush().context("flushing the preface")?;
    h2_exchange(
        &mut sock,
        1,
        &request_headers(SCHEME_HTTP, path, authority)?,
    )
}

/// Send one request on `stream` and read frames until that stream ends. An I/O failure while
/// reading is the connection lost, which is an outcome; a frame this probe cannot follow is an
/// error.
fn h2_exchange(sock: &mut TcpStream, stream: u32, headers: &[u8]) -> anyhow::Result<H2Outcome> {
    // END_STREAM | END_HEADERS
    sock.write_all(&frame(1, 0x5, stream, headers)?)
        .context("writing HEADERS")?;
    sock.flush().context("flushing HEADERS")?;
    let mut body = Vec::new();
    let mut status = None;
    loop {
        let mut head = [0u8; 9];
        if let Err(e) = sock.read_exact(&mut head) {
            return Ok(lost(&e));
        }
        let len = usize::from(head[0]) << 16 | usize::from(head[1]) << 8 | usize::from(head[2]);
        let (kind, flags) = (head[3], head[4]);
        let id = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
        let mut payload = vec![0u8; len];
        if let Err(e) = sock.read_exact(&mut payload) {
            return Ok(lost(&e));
        }
        let replied = if kind == 4 && id == 0 && flags & 0x1 == 0 {
            // ACK the server's SETTINGS.
            sock.write_all(&frame(4, 0x1, 0, &[])?)
        } else if kind == 6 && flags & 0x1 == 0 {
            // PING ACK.
            sock.write_all(&frame(6, 0x1, 0, &payload)?)
        } else if kind == 7 {
            return Ok(H2Outcome::ConnectionLost(format!(
                "GOAWAY before stream {stream} ended"
            )));
        } else if id == stream {
            if kind == 3 {
                let code: [u8; 4] = payload
                    .get(..4)
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| anyhow!("RST_STREAM with a {len}-byte payload"))?;
                return Ok(H2Outcome::Reset(u32::from_be_bytes(code)));
            }
            if kind == 9 || (kind == 1 && flags & 0x28 != 0) {
                bail!("frame type {kind}/flags {flags} not handled");
            }
            if kind == 1 {
                status = indexed_status_of(&payload);
            }
            if kind == 0 {
                body.extend_from_slice(&payload);
            }
            if (kind == 0 || kind == 1) && flags & 0x1 != 0 {
                return Ok(H2Outcome::Answered {
                    status,
                    body: String::from_utf8_lossy(&body).into_owned(),
                });
            }
            Ok(())
        } else {
            Ok(())
        };
        if let Err(e) = replied.and_then(|()| sock.flush()) {
            return Ok(lost(&e));
        }
    }
}

fn lost(e: &std::io::Error) -> H2Outcome {
    H2Outcome::ConnectionLost(format!("{:?}: {e}", e.kind()))
}

// ── over-cap send ────────────────────────────────────────────────────────────────────────────

/// POST `body` to `url` over a bare socket, reading the response while the body is still being
/// written: an engine refusing an over-cap body answers and closes mid-upload, and a client that
/// writes everything before reading would only see the reset. Plaintext HTTP/1.1 only.
pub fn send_oversize(
    url: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> anyhow::Result<Response> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("URL {url}"))?;
    if parsed.scheme() != "http" {
        bail!("send_oversize is plaintext-only, got {url}");
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("{url} has no host"))?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);
    let target = match parsed.query() {
        Some(q) => format!("{}?{q}", parsed.path()),
        None => parsed.path().to_string(),
    };
    let started = Instant::now();
    let mut sock = connect(&host, port, Duration::from_secs(10))
        .with_context(|| format!("connecting to {host}:{port}"))?;
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .context("setting the read timeout")?;
    sock.set_write_timeout(Some(Duration::from_secs(10)))
        .context("setting the write timeout")?;
    let mut head = format!(
        "{} {target} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Length: {}\r\nConnection: close\r\n",
        method.to_uppercase(),
        body.len()
    );
    // Framing headers are ours; a caller restating one would emit a duplicate line.
    for (k, v) in headers {
        if !["host", "content-length", "connection"].contains(&k.to_ascii_lowercase().as_str()) {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    head.push_str("\r\n");

    // Started BEFORE the body write. Shutting the socket down once a complete response is in also
    // releases a writer blocked on a peer that has stopped reading.
    let reader = {
        let mut sock = sock
            .try_clone()
            .context("cloning the socket for the reader")?;
        std::thread::spawn(move || {
            let mut collected = Vec::new();
            let mut chunk = [0u8; 8 * 1024];
            // A reset or close once the response is in is expected here: what was read is the
            // answer, and parse_response judges whether it is whole.
            while let Ok(n) = sock.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                collected.extend_from_slice(&chunk[..n]);
                if is_complete(&collected) {
                    // Already shut down by the peer is the same outcome.
                    let _already_closed = sock.shutdown(Shutdown::Both);
                    break;
                }
            }
            collected
        })
    };

    sock.write_all(head.as_bytes())
        .context("writing the request head")?;
    let mut write_error = None;
    for piece in body.chunks(64 * 1024) {
        if started.elapsed() > OVERSIZE_DEADLINE {
            bail!(
                "over-size send to {url} exceeded {OVERSIZE_DEADLINE:?} with no response: the peer \
                 stopped reading without answering"
            );
        }
        // The peer refused and closed mid-upload. Keep the cause: if no response turns up, it is
        // the only evidence of what went wrong.
        if let Err(e) = sock.write_all(piece) {
            write_error = Some(e);
            break;
        }
    }

    let budget = OVERSIZE_DEADLINE
        .saturating_sub(started.elapsed())
        .max(Duration::from_secs(1));
    let waited = Instant::now();
    while !reader.is_finished() {
        if waited.elapsed() > budget {
            // Unblocks the reader so its thread ends; the error below is the outcome either way.
            let _already_closed = sock.shutdown(Shutdown::Both);
            bail!(
                "over-size send to {url}: the response never completed within {OVERSIZE_DEADLINE:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let raw = reader
        .join()
        .map_err(|_| anyhow!("the over-size send's reader panicked"))?;
    parse_response(&raw).map_err(|e| match write_error {
        Some(w) => e.context(format!("the body write failed first: {w}")),
        None => e,
    })
}

/// Whether `raw` holds a complete response: a head plus a body as long as its `Content-Length`.
fn is_complete(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    text.find("\r\n\r\n").is_some_and(|sep| {
        content_length(&text[..sep]).is_some_and(|n| text.len() - (sep + 4) >= n)
    })
}

fn content_length(head: &str) -> Option<usize> {
    head.split("\r\n").find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse().ok())
            .flatten()
    })
}

/// Parse a raw HTTP/1.1 response. Anything partial fails: a truncated body parsed into a plausible
/// response would let a status-only assertion pass on a response that never fully arrived.
fn parse_response(raw: &[u8]) -> anyhow::Result<Response> {
    let text = String::from_utf8_lossy(raw);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        bail!(
            "no complete HTTP response read before the peer closed (got {} bytes)",
            raw.len()
        );
    };
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("unparseable status line: {status_line:?}"))?;
    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers
                .entry(k.trim().to_ascii_lowercase())
                .or_default()
                .push(v.trim().to_string());
        }
    }
    // Chunk framing would otherwise land in the body verbatim and still satisfy a `contains`.
    if let Some(te) = headers.get("transfer-encoding") {
        bail!(
            "send_oversize cannot decode Transfer-Encoding: {}",
            te.join(",")
        );
    }
    if let Some(declared) = content_length(head)
        && body.len() < declared
    {
        bail!(
            "truncated response body: declared {declared} bytes, read {}",
            body.len()
        );
    }
    Ok(Response {
        status,
        body: body.to_string(),
        headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(bytes: &[u8], closed: bool, error: Option<&str>) -> RawCapture {
        RawCapture {
            bytes: bytes.to_vec(),
            tls_handshake: None,
            closed,
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn a_reset_needs_no_bytes_a_close_and_a_reset_error() {
        let reset = cap(b"", true, Some("ConnectionReset: Connection reset by peer"));
        assert_eq!(reset.shows("connection reset"), Some(true));
        assert_eq!(reset.shows("empty reply"), Some(false));
        assert_eq!(cap(b"", true, None).shows("connection reset"), Some(false));
        assert_eq!(
            cap(b"", false, Some("no close within 5s")).shows("connection reset"),
            Some(false)
        );
    }

    #[test]
    fn an_empty_reply_is_a_clean_eof_with_no_bytes() {
        assert_eq!(cap(b"", true, None).shows("empty reply"), Some(true));
        assert_eq!(cap(b"x", true, None).shows("empty reply"), Some(false));
    }

    #[test]
    fn garbage_is_bytes_that_are_not_an_http_response() {
        assert_eq!(
            cap(b"\x01\x02zz", true, None).shows("garbage bytes"),
            Some(true)
        );
        assert_eq!(
            cap(b"HTTP/1.1 200 OK", true, None).shows("garbage bytes"),
            Some(false)
        );
        assert_eq!(cap(b"", true, None).shows("garbage bytes"), Some(false));
    }

    #[test]
    fn a_broken_chunk_is_a_chunked_200_head_then_a_non_hex_size() {
        let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let broken = format!("{head}zz\r\n");
        assert_eq!(
            cap(broken.as_bytes(), true, None).shows("200 head then a broken chunk"),
            Some(true)
        );
        let fine = format!("{head}1a;ext=1\r\n");
        assert_eq!(
            cap(fine.as_bytes(), true, None).shows("200 head then a broken chunk"),
            Some(false)
        );
        assert_eq!(cap(b"", true, None).shows("teapot"), None);
    }

    #[test]
    fn the_status_is_read_past_a_table_size_update() {
        assert_eq!(indexed_status_of(&[0x88]), Some(200));
        assert_eq!(indexed_status_of(&[0x3f, 0xe1, 0x1f, 0x88]), Some(200));
        assert_eq!(indexed_status_of(&[0x20, 0x8d]), Some(404));
        assert_eq!(indexed_status_of(&[0x48, 0x03]), None);
    }

    #[test]
    fn frames_carry_a_24_bit_length_type_flags_and_stream() {
        let f = frame(1, 0x5, 3, b"ab").expect("a frame");
        assert_eq!(f, [0, 0, 2, 1, 5, 0, 0, 0, 3, b'a', b'b']);
    }

    #[test]
    fn a_complete_response_has_its_whole_content_length() {
        assert!(!is_complete(
            b"HTTP/1.1 413 X\r\nContent-Length: 4\r\n\r\nab"
        ));
        assert!(is_complete(
            b"HTTP/1.1 413 X\r\nContent-Length: 4\r\n\r\nabcd"
        ));
        let r = parse_response(
            b"HTTP/1.1 413 Too Large\r\nContent-Type: a/b\r\nContent-Length: 2\r\n\r\n{}",
        )
        .expect("parses");
        assert_eq!(r.status, 413);
        assert_eq!(r.body, "{}");
        assert_eq!(r.header_values("content-type"), ["a/b"]);
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nab").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_err());
    }
}
