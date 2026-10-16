//! HTTP/1.1 and HTTP/2 framed by hand over one connection, async: what a feature asserts about the
//! wire itself (a peer's certificate chain and ALPN, exact response framing, a repeated header, a
//! connection kept or closed after an answer). Port of the Scala runner's `RawHttpConnection`.
//!
//! A [`RawConnection`] never reconnects: every exchange on it goes over the stream it was opened
//! with, so a second answered exchange proves the peer kept that connection alive.

use crate::http::Response;
use crate::raw::{self, H2Outcome};
use anyhow::{Context as _, anyhow, bail};
use rustls::pki_types::CertificateDer;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
    ReadHalf, WriteHalf,
};

/// Connect, read and write-drain bound (the Scala runner's `TimeoutMillis`): generous for CI, far
/// below the scenario timeout.
pub const WIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// A byte stream a connection runs over: plain TCP, or TLS over TCP or over a CONNECT tunnel.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// One HTTP/1.1 response exactly as framed on the wire. Header names are lower-cased and kept in
/// wire order, so a repeated header stays repeated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    /// The body as UTF-8, lossily (a non-UTF-8 body fails a text assertion visibly).
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Every value of the header `name` (lower-case), in wire order.
    #[must_use]
    pub fn header(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The same response in the shape the status, body and header steps read.
    #[must_use]
    pub fn to_response(&self) -> Response {
        let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in &self.headers {
            headers.entry(k.clone()).or_default().push(v.clone());
        }
        Response {
            status: self.status,
            body: self.body_text(),
            headers,
        }
    }
}

/// What the next read on a connection produced: a complete response, or the peer closing (EOF or
/// reset) before the first byte of one. A close part-way through a response is an error, not this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawRead {
    Response(RawResponse),
    Closed,
}

/// An HTTP/1.1 client over one stream.
pub struct RawConnection {
    reader: BufReader<ReadHalf<Box<dyn Io>>>,
    writer: WriteHalf<Box<dyn Io>>,
    answered: usize,
    peer_certificates: Vec<CertificateDer<'static>>,
    alpn: Option<Vec<u8>>,
}

impl std::fmt::Debug for RawConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawConnection")
            .field("answered", &self.answered)
            .field("peer_certificates", &self.peer_certificates.len())
            .field("alpn", &self.alpn)
            .finish_non_exhaustive()
    }
}

impl RawConnection {
    /// `peer_certificates` and `alpn` are what the TLS handshake on `io` settled, empty and `None`
    /// on a plaintext stream.
    pub fn new(
        io: Box<dyn Io>,
        peer_certificates: Vec<CertificateDer<'static>>,
        alpn: Option<Vec<u8>>,
    ) -> Self {
        let (read, write) = tokio::io::split(io);
        Self {
            reader: BufReader::new(read),
            writer: write,
            answered: 0,
            peer_certificates,
            alpn,
        }
    }

    /// A plaintext connection to `host:port`.
    pub async fn plain(host: &str, port: u16) -> anyhow::Result<Self> {
        let tcp = connect(host, port).await?;
        Ok(Self::new(Box::new(tcp), Vec::new(), None))
    }

    /// Responses read so far on this one connection.
    #[must_use]
    pub fn responses_read(&self) -> usize {
        self.answered
    }

    /// The chain the TLS peer presented, leaf first; empty on a plaintext connection.
    #[must_use]
    pub fn peer_certificates(&self) -> &[CertificateDer<'static>] {
        &self.peer_certificates
    }

    /// The protocol ALPN settled on, if any.
    #[must_use]
    pub fn alpn(&self) -> Option<&[u8]> {
        self.alpn.as_deref()
    }

    /// Write `request` and read the response to it.
    ///
    /// The write runs alongside the read: a server that refuses early (a `413` over the body cap)
    /// answers while the body is still streaming, and may close without draining it. Writing first
    /// and reading after would lose that response to the reset (rift-conformance#131). A write the
    /// peer cut short is that refusal, not a failure; a write still blocked once the answer is in
    /// is: the peer stopped reading without closing, so the next exchange could interleave.
    pub async fn exchange(&mut self, request: &[u8]) -> anyhow::Result<RawRead> {
        let Self {
            reader,
            writer,
            answered,
            ..
        } = self;
        let write = async {
            writer.write_all(request).await?;
            writer.flush().await
        };
        let (read, written) = tokio::join!(
            tokio::time::timeout(WIRE_TIMEOUT, read_next(reader, answered)),
            tokio::time::timeout(WIRE_TIMEOUT, write)
        );
        let read = read
            .map_err(|_| anyhow!("no response within {WIRE_TIMEOUT:?}"))?
            .context("reading the response")?;
        if written.is_err() {
            bail!(
                "the peer answered ({}) but stopped reading the rest of the request without closing",
                describe(&read)
            );
        }
        Ok(read)
    }

    /// Read the next response without writing anything: e.g. to observe that the server closed.
    pub async fn read_next(&mut self) -> anyhow::Result<RawRead> {
        tokio::time::timeout(
            WIRE_TIMEOUT,
            read_next(&mut self.reader, &mut self.answered),
        )
        .await
        .map_err(|_| {
            anyhow!("nothing read within {WIRE_TIMEOUT:?}: the peer neither answered nor closed")
        })?
        .context("reading the response")
    }

    /// Write an `Upgrade` request and read its answer. A `101` comes back with no body, and the
    /// connection then carries the upgraded protocol ([`Self::write_raw`], [`Self::read_exact`]);
    /// any other status is read in full, as [`Self::exchange`] does.
    pub async fn upgrade(&mut self, request: &[u8]) -> anyhow::Result<RawRead> {
        self.write_raw(request).await?;
        let read = async {
            match read_head(&mut self.reader).await? {
                None => Ok(RawRead::Closed),
                Some((status, headers)) => {
                    let body = if status == 101 {
                        Vec::new()
                    } else {
                        read_body(&mut self.reader, status, &headers).await?
                    };
                    self.answered += 1;
                    Ok(RawRead::Response(RawResponse {
                        status,
                        headers,
                        body,
                    }))
                }
            }
        };
        tokio::time::timeout(WIRE_TIMEOUT, read)
            .await
            .map_err(|_| anyhow!("no answer to the upgrade within {WIRE_TIMEOUT:?}"))?
    }

    /// Write bytes as they are (an upgraded protocol's frames).
    pub async fn write_raw(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        tokio::time::timeout(WIRE_TIMEOUT, async {
            self.writer.write_all(bytes).await?;
            self.writer.flush().await
        })
        .await
        .map_err(|_| anyhow!("writing {} bytes took over {WIRE_TIMEOUT:?}", bytes.len()))?
        .context("writing")
    }

    /// Read exactly `n` bytes (an upgraded protocol's frames).
    pub async fn read_exact(&mut self, n: usize) -> anyhow::Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        tokio::time::timeout(WIRE_TIMEOUT, self.reader.read_exact(&mut buf))
            .await
            .map_err(|_| anyhow!("{n} bytes not read within {WIRE_TIMEOUT:?}"))?
            .with_context(|| format!("reading {n} bytes"))?;
        Ok(buf)
    }

    /// One HTTP/2 `GET path` on stream 1, the connection preface first: for a connection ALPN
    /// settled on `h2`. Hand-encoded frames, HPACK from the static table only, as `raw`'s h2c probe.
    pub async fn h2_get(&mut self, path: &str, authority: &str) -> anyhow::Result<H2Outcome> {
        let headers = raw::request_headers(raw::SCHEME_HTTPS, path, authority)?;
        let mut preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
        preface.extend(raw::frame(4, 0, 0, &[])?);
        // END_STREAM | END_HEADERS
        preface.extend(raw::frame(1, 0x5, 1, &headers)?);
        self.write_raw(&preface).await?;
        tokio::time::timeout(WIRE_TIMEOUT, self.h2_read_stream(1))
            .await
            .map_err(|_| anyhow!("stream 1 did not end within {WIRE_TIMEOUT:?}"))?
    }

    async fn h2_read_stream(&mut self, stream: u32) -> anyhow::Result<H2Outcome> {
        let mut body = Vec::new();
        let mut status = None;
        loop {
            let mut head = [0u8; 9];
            if let Err(e) = self.reader.read_exact(&mut head).await {
                return Ok(lost(&e));
            }
            let len = usize::from(head[0]) << 16 | usize::from(head[1]) << 8 | usize::from(head[2]);
            let (kind, flags) = (head[3], head[4]);
            let id = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
            let mut payload = vec![0u8; len];
            if let Err(e) = self.reader.read_exact(&mut payload).await {
                return Ok(lost(&e));
            }
            if kind == 4 && id == 0 && flags & 0x1 == 0 {
                // ACK the server's SETTINGS.
                self.write_raw(&raw::frame(4, 0x1, 0, &[])?).await?;
            } else if kind == 6 && flags & 0x1 == 0 {
                self.write_raw(&raw::frame(6, 0x1, 0, &payload)?).await?;
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
                    status = raw::indexed_status_of(&payload);
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
            }
        }
    }
}

fn lost(e: &std::io::Error) -> H2Outcome {
    H2Outcome::ConnectionLost(format!("{:?}: {e}", e.kind()))
}

fn describe(read: &RawRead) -> String {
    match read {
        RawRead::Closed => "a close".to_string(),
        RawRead::Response(r) => format!("{} '{}'", r.status, head(&r.body_text())),
    }
}

fn head(text: &str) -> String {
    text.chars().take(80).collect()
}

/// A TCP connection to the first address of `host:port` that accepts within [`WIRE_TIMEOUT`].
pub async fn connect(host: &str, port: u16) -> anyhow::Result<tokio::net::TcpStream> {
    tokio::time::timeout(WIRE_TIMEOUT, tokio::net::TcpStream::connect((host, port)))
        .await
        .map_err(|_| anyhow!("connecting to {host}:{port} timed out after {WIRE_TIMEOUT:?}"))?
        .with_context(|| format!("connecting to {host}:{port}"))
}

async fn read_next<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    answered: &mut usize,
) -> anyhow::Result<RawRead> {
    loop {
        match read_head(reader).await? {
            None => return Ok(RawRead::Closed),
            // An interim response: the final one follows.
            Some((status, _)) if status / 100 == 1 => {}
            Some((status, headers)) => {
                let body = read_body(reader, status, &headers).await?;
                *answered += 1;
                return Ok(RawRead::Response(RawResponse {
                    status,
                    headers,
                    body,
                }));
            }
        }
    }
}

/// Status line and headers, or `None` when the peer closed (EOF, reset, a TLS session ended without
/// `close_notify`) before sending a byte. The caller's timeout keeps a hung peer from looking like
/// one that closed.
async fn read_head<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> anyhow::Result<Option<(u16, Vec<(String, String)>)>> {
    match reader.fill_buf().await {
        Ok([]) | Err(_) => return Ok(None),
        Ok(_) => {}
    }
    let status_line = read_line(reader).await?;
    let mut parts = status_line.split(' ');
    let status = match (parts.next(), parts.next()) {
        (Some(v), Some(code)) if v.starts_with("HTTP/1.") => code.parse::<u16>().ok(),
        _ => None,
    }
    .ok_or_else(|| anyhow!("not an HTTP/1.x status line: '{status_line}'"))?;
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader).await?;
        if line.is_empty() {
            return Ok(Some((status, headers)));
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed header line: '{line}'"))?;
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
}

async fn read_body<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    status: u16,
    headers: &[(String, String)],
) -> anyhow::Result<Vec<u8>> {
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v);
    if status == 204 || status == 304 {
        return Ok(Vec::new());
    }
    if header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        let mut body = Vec::new();
        loop {
            let line = read_line(reader).await?;
            let size_text = line.split(';').next().unwrap_or_default().trim();
            let size = usize::from_str_radix(size_text, 16)
                .with_context(|| format!("chunk-size line '{line}'"))?;
            if size == 0 {
                break;
            }
            let mut chunk = vec![0u8; size];
            reader
                .read_exact(&mut chunk)
                .await
                .with_context(|| format!("a {size}-byte chunk"))?;
            body.extend(chunk);
            // The CRLF closing the chunk.
            read_line(reader).await?;
        }
        // Trailers, up to the blank line.
        while !read_line(reader).await?.is_empty() {}
        return Ok(body);
    }
    match header("content-length") {
        Some(n) => {
            let n: usize = n.parse().with_context(|| format!("content-length '{n}'"))?;
            let mut body = vec![0u8; n];
            reader
                .read_exact(&mut body)
                .await
                .with_context(|| format!("connection closed before {n} body bytes"))?;
            Ok(body)
        }
        None => {
            // Close-delimited. A TLS peer that closes without `close_notify` ends it too.
            let mut body = Vec::new();
            match reader.read_to_end(&mut body).await {
                Ok(_) => Ok(body),
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(body),
                Err(e) => Err(anyhow::Error::new(e).context("reading a close-delimited body")),
            }
        }
    }
}

/// One line without its CRLF, as ISO-8859-1. A close mid-line is an error.
async fn read_line<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> anyhow::Result<String> {
    let mut line = Vec::new();
    let n = reader
        .read_until(b'\n', &mut line)
        .await
        .context("reading a line")?;
    if n == 0 || line.last() != Some(&b'\n') {
        bail!("connection closed mid-line: '{}'", latin1(&line).trim_end());
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(latin1(&line))
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// An HTTP/1.1 request framed by `content-length` (omitted for a bodiless GET, HEAD or DELETE).
#[must_use]
pub fn request(
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let length = body.len().to_string();
    let mut all: Vec<(&str, &str)> = headers.to_vec();
    if !(body.is_empty() && ["GET", "HEAD", "DELETE"].contains(&method)) {
        all.push(("Content-Length", &length));
    }
    let mut out = head_bytes(method, host, path, &all);
    out.extend_from_slice(body);
    out
}

/// An HTTP/1.1 request whose body is sent `transfer-encoding: chunked`, one chunk per element.
#[must_use]
pub fn chunked_request(method: &str, host: &str, path: &str, chunks: &[&str]) -> Vec<u8> {
    let mut out = head_bytes(method, host, path, &[("Transfer-Encoding", "chunked")]);
    // A zero-length chunk would end the body early.
    for chunk in chunks.iter().filter(|c| !c.is_empty()) {
        out.extend(format!("{:x}\r\n", chunk.len()).into_bytes());
        out.extend_from_slice(chunk.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

/// `CONNECT target:port` with any extra header lines (`Proxy-Authorization`, say).
#[must_use]
pub fn connect_request(target: &str, port: u16, headers: &[(&str, &str)]) -> Vec<u8> {
    let mut lines = vec![
        format!("CONNECT {target}:{port} HTTP/1.1"),
        format!("Host: {target}:{port}"),
    ];
    lines.extend(headers.iter().map(|(k, v)| format!("{k}: {v}")));
    format!("{}\r\n\r\n", lines.join("\r\n")).into_bytes()
}

fn head_bytes(method: &str, host: &str, path: &str, headers: &[(&str, &str)]) -> Vec<u8> {
    let mut lines = vec![format!("{method} {path} HTTP/1.1"), format!("Host: {host}")];
    lines.extend(headers.iter().map(|(k, v)| format!("{k}: {v}")));
    format!("{}\r\n\r\n", lines.join("\r\n")).into_bytes()
}

/// Read a `CONNECT`'s reply head byte by byte from a stream not yet wrapped in anything: whatever
/// follows its blank line belongs to the TLS handshake, so nothing past it may be consumed.
pub async fn read_connect_reply<S: AsyncRead + Unpin>(stream: &mut S) -> anyhow::Result<u16> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = stream
            .read(&mut byte)
            .await
            .context("reading the CONNECT reply")?;
        if n == 0 {
            bail!(
                "the proxy closed before answering the CONNECT ({} bytes: '{}')",
                head.len(),
                latin1(&head)
            );
        }
        head.push(byte[0]);
        if head.len() > 64 * 1024 {
            bail!("a CONNECT reply head over 64 KiB");
        }
    }
    let text = latin1(&head);
    let status_line = text.lines().next().unwrap_or_default();
    status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("not an HTTP status line: '{status_line}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn_over(bytes: &'static [u8]) -> RawConnection {
        let (client, mut server) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let _ = server.write_all(bytes).await;
            // EOF after the canned bytes, then keep reading so the client's writes never block.
            let _ = server.shutdown().await;
            let mut sink = Vec::new();
            let _ = server.read_to_end(&mut sink).await;
        });
        RawConnection::new(Box::new(client), Vec::new(), None)
    }

    #[tokio::test]
    async fn reads_a_content_length_response_then_the_close() {
        let mut c =
            conn_over(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-A: 1\r\nx-a: 2\r\n\r\nok");
        let RawRead::Response(r) = c.exchange(b"GET / HTTP/1.1\r\n\r\n").await.expect("read")
        else {
            panic!("expected a response");
        };
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"ok");
        assert_eq!(r.header("x-a"), ["1", "2"]);
        assert_eq!(c.responses_read(), 1);
    }

    #[tokio::test]
    async fn decodes_a_chunked_body_and_skips_an_interim_response() {
        let mut c = conn_over(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;x=1\r\n world\r\n0\r\n\r\n",
        );
        let RawRead::Response(r) = c.read_next().await.expect("read") else {
            panic!("expected a response");
        };
        assert_eq!(r.body_text(), "hello world");
    }

    #[tokio::test]
    async fn eof_before_a_byte_is_a_close_and_eof_mid_response_is_an_error() {
        assert_eq!(
            conn_over(b"").read_next().await.expect("read"),
            RawRead::Closed
        );
        assert!(
            conn_over(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nab")
                .read_next()
                .await
                .is_err()
        );
    }

    #[test]
    fn requests_are_framed_by_content_length_or_chunks() {
        assert_eq!(
            request("GET", "h", "/p", &[("X", "1")], b""),
            b"GET /p HTTP/1.1\r\nHost: h\r\nX: 1\r\n\r\n"
        );
        assert_eq!(
            request("POST", "h", "/p", &[], b"ab"),
            b"POST /p HTTP/1.1\r\nHost: h\r\nContent-Length: 2\r\n\r\nab"
        );
        assert_eq!(
            chunked_request("POST", "h", "/", &["hello", "", "!"]),
            b"POST / HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n1\r\n!\r\n0\r\n\r\n"
        );
        assert_eq!(
            connect_request("a.test", 443, &[("Proxy-Authorization", "Basic x")]),
            b"CONNECT a.test:443 HTTP/1.1\r\nHost: a.test:443\r\nProxy-Authorization: Basic x\r\n\r\n"
        );
    }
}
