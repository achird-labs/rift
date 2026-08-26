//! Real connection-level TCP faults for `_rift.fault.tcp` (issue #239).
//!
//! A normal `Response` is framed and sent by hyper, so it can never reproduce a transport-level
//! failure. Instead, the matched fault is recorded in a per-connection `FaultCell` (set by the
//! request handler via a `Response` extension) and applied by `FaultIo`, a thin wrapper around
//! the connection's `TcpStream`: on the next write hyper attempts (the response), `FaultIo`
//! performs the fault on the raw socket and returns an error so hyper aborts the connection. The
//! client then observes a genuine reset / empty / malformed / random-data failure, matching
//! WireMock's `Fault` semantics.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::Response;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tracing::debug;

/// The four WireMock-equivalent connection faults.
///
/// Public because it is the authoritative signal a TCP-fault carrier response carries: the serve
/// loop's `FaultIo` reads exactly this extension, and an in-process embedder that calls
/// [`handle_imposter_request`](crate::imposter::handle_imposter_request) directly receives the
/// carrier the serve loop would have replaced with a socket abort (issue #965). Matching on the
/// kind beats re-deriving one from a string; [`tcp_fault_carrier`] is the string form.
///
/// `#[non_exhaustive]`: an embedder `match`es on this, and a fifth transport fault must not break
/// every one of them — the same reasoning
/// [`StubPlacement`](crate::recording::StubPlacement) carries. Match with a `_` arm
/// and treat an unknown kind as "the connection was aborted somehow" rather than guessing which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TcpFaultKind {
    /// Reset the connection (client sees `ECONNRESET`).
    Reset,
    /// Close the connection after the request with no response bytes.
    Empty,
    /// Send random bytes then close (client sees a protocol error).
    RandomData,
    /// Send a status line + a malformed chunked body then close.
    MalformedChunk,
}

impl TcpFaultKind {
    /// Parse a `_rift.fault.tcp` string. Accepts the WireMock names and Rift's short aliases.
    #[must_use]
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "reset" | "CONNECTION_RESET_BY_PEER" => Some(Self::Reset),
            "empty" | "EMPTY_RESPONSE" => Some(Self::Empty),
            "garbage" | "random" | "RANDOM_DATA_THEN_CLOSE" => Some(Self::RandomData),
            "malformed" | "MALFORMED_RESPONSE_CHUNK" => Some(Self::MalformedChunk),
            _ => None,
        }
    }

    /// The canonical name for this fault — the WireMock-style name the docs present as canonical,
    /// with `reset`/`empty`/`garbage`/`malformed` as its aliases (`docs/features/fault-injection.md`).
    ///
    /// Deliberately not the string the config author wrote: the `x-rift-fault` header echoes that
    /// raw input, which is why a consumer had to mirror the whole alias table to interpret it. One
    /// name per kind is what lets that table be deleted.
    #[must_use]
    pub fn canonical_name(self) -> &'static str {
        match self {
            Self::Reset => "CONNECTION_RESET_BY_PEER",
            Self::Empty => "EMPTY_RESPONSE",
            Self::RandomData => "RANDOM_DATA_THEN_CLOSE",
            Self::MalformedChunk => "MALFORMED_RESPONSE_CHUNK",
        }
    }
}

/// Classify a response as a TCP-fault *carrier*: the placeholder a fault stub produces, which the
/// serve loop's `FaultIo` discards in favour of a real socket abort, so a client over TCP never
/// sees it.
///
/// Returns the fault's [canonical name](TcpFaultKind::canonical_name), or `None` for an ordinary
/// response. An embedder answering a request in-process (rift-cluster #344) receives the carrier
/// itself and needs this to render "the connection would have been aborted" rather than presenting
/// the carrier's `502` as the imposter's answer.
///
/// Reads the extension, not the `x-rift-fault` header, because the header is neither necessary nor
/// sufficient: a v2 script `reset()` carrier historically set no header at all (issue #357), while
/// an `_rift.fault.error` response sets `x-rift-fault: error` and is a real response the client
/// does receive. Generic over the body so classification need not precede a body transformation.
#[must_use]
pub fn tcp_fault_carrier<B>(response: &Response<B>) -> Option<&'static str> {
    response
        .extensions()
        .get::<TcpFaultKind>()
        .map(|kind| kind.canonical_name())
}

/// Per-connection slot the handler sets when a request matches a TCP fault; read by [`FaultIo`].
pub(crate) type FaultCell = Arc<Mutex<Option<TcpFaultKind>>>;

/// A `TcpStream` wrapper that, when [`FaultCell`] is armed, applies a connection fault on the next
/// write instead of forwarding hyper's response. Reads and (un-armed) writes pass straight through.
pub(crate) struct FaultIo {
    inner: TcpStream,
    fault: FaultCell,
}

impl FaultIo {
    pub(crate) fn new(inner: TcpStream, fault: FaultCell) -> Self {
        Self { inner, fault }
    }

    /// Apply the fault to the raw socket. Returns the error used to abort the hyper connection so
    /// the socket is dropped (and closed) immediately afterwards. The raw socket calls are
    /// best-effort: on failure the connection still breaks, but the *observed* fault may degrade
    /// (e.g. a failed `set_linger` reset becomes a graceful close), so each failure is logged.
    fn trip(&self, kind: TcpFaultKind) -> io::Error {
        match kind {
            // SO_LINGER(0): dropping the socket now emits RST rather than a graceful FIN.
            TcpFaultKind::Reset => {
                if let Err(e) = self.inner.set_linger(Some(Duration::ZERO)) {
                    debug!("rift fault reset: set_linger(0) failed, degraded to graceful close: {e}");
                }
            }
            // Nothing written; the abort closes the connection with no response bytes.
            TcpFaultKind::Empty => {}
            // Best-effort raw write, then a graceful close delivers the bytes before EOF.
            TcpFaultKind::RandomData => {
                if let Err(e) = self.inner.try_write(b"\x00\xff\xfe\xfd\x13\x37\xde\xad\xbe\xef") {
                    debug!("rift fault random-data: write failed, degraded to empty close: {e}");
                }
            }
            TcpFaultKind::MalformedChunk => {
                if let Err(e) = self.inner.try_write(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nZZZZZ not a valid chunk size\r\n",
                ) {
                    debug!("rift fault malformed-chunk: write failed, degraded to empty close: {e}");
                }
            }
        }
        io::Error::other("rift: injected _rift.fault.tcp")
    }
}

impl AsyncRead for FaultIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for FaultIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Take (not just read) the armed fault so it fires exactly once.
        if let Some(kind) = self.fault.lock().take() {
            return Poll::Ready(Err(self.trip(kind)));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::Full;

    #[test]
    fn parses_wiremock_names_and_aliases() {
        assert_eq!(
            TcpFaultKind::parse("CONNECTION_RESET_BY_PEER"),
            Some(TcpFaultKind::Reset)
        );
        assert_eq!(TcpFaultKind::parse("reset"), Some(TcpFaultKind::Reset));
        assert_eq!(
            TcpFaultKind::parse("EMPTY_RESPONSE"),
            Some(TcpFaultKind::Empty)
        );
        assert_eq!(TcpFaultKind::parse("empty"), Some(TcpFaultKind::Empty));
        assert_eq!(
            TcpFaultKind::parse("RANDOM_DATA_THEN_CLOSE"),
            Some(TcpFaultKind::RandomData)
        );
        assert_eq!(
            TcpFaultKind::parse("garbage"),
            Some(TcpFaultKind::RandomData)
        );
        assert_eq!(
            TcpFaultKind::parse("random"),
            Some(TcpFaultKind::RandomData)
        );
        assert_eq!(
            TcpFaultKind::parse("malformed"),
            Some(TcpFaultKind::MalformedChunk)
        );
        assert_eq!(
            TcpFaultKind::parse("MALFORMED_RESPONSE_CHUNK"),
            Some(TcpFaultKind::MalformedChunk)
        );
        assert_eq!(TcpFaultKind::parse("nonsense"), None);
    }

    /// The names the docs call canonical (`docs/features/fault-injection.md`,
    /// `docs/configuration/native.md`: "canonical name or short alias") — literal, so a rename of
    /// the wire vocabulary has to come here and be deliberate.
    #[test]
    fn canonical_name_returns_the_documented_wiremock_name() {
        assert_eq!(
            TcpFaultKind::Reset.canonical_name(),
            "CONNECTION_RESET_BY_PEER"
        );
        assert_eq!(TcpFaultKind::Empty.canonical_name(), "EMPTY_RESPONSE");
        assert_eq!(
            TcpFaultKind::RandomData.canonical_name(),
            "RANDOM_DATA_THEN_CLOSE"
        );
        assert_eq!(
            TcpFaultKind::MalformedChunk.canonical_name(),
            "MALFORMED_RESPONSE_CHUNK"
        );
    }

    /// Distinct names per kind: a classifier that collapsed two kinds onto one string would let an
    /// embedder render an empty-close as a reset while every individual assertion above still held.
    #[test]
    fn canonical_names_are_distinct_across_kinds() {
        let names = [
            TcpFaultKind::Reset.canonical_name(),
            TcpFaultKind::Empty.canonical_name(),
            TcpFaultKind::RandomData.canonical_name(),
            TcpFaultKind::MalformedChunk.canonical_name(),
        ];
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), 4, "each kind needs its own name: {names:?}");
    }

    /// Every kind round-trips through the extension the serve loop itself reads.
    #[test]
    fn tcp_fault_carrier_reports_the_kind_in_the_extension() {
        for (kind, expected) in [
            (TcpFaultKind::Reset, "CONNECTION_RESET_BY_PEER"),
            (TcpFaultKind::Empty, "EMPTY_RESPONSE"),
            (TcpFaultKind::RandomData, "RANDOM_DATA_THEN_CLOSE"),
            (TcpFaultKind::MalformedChunk, "MALFORMED_RESPONSE_CHUNK"),
        ] {
            let mut response = Response::new(Full::new(Bytes::new()));
            response.extensions_mut().insert(kind);
            assert_eq!(tcp_fault_carrier(&response), Some(expected));
        }
    }

    #[test]
    fn tcp_fault_carrier_ignores_an_ordinary_response() {
        let response = Response::new(Full::new(Bytes::from_static(b"hello")));
        assert_eq!(tcp_fault_carrier(&response), None);
    }

    /// The false positive a header-based classifier hits today: `_rift.fault.error` stamps
    /// `x-rift-fault: error` and attaches no extension, so it is an ordinary HTTP response that
    /// the client really does receive — never a carrier.
    #[test]
    fn tcp_fault_carrier_ignores_the_error_fault_marker_header() {
        let mut response = Response::new(Full::new(Bytes::new()));
        response.headers_mut().insert(
            "x-rift-fault",
            hyper::header::HeaderValue::from_static("error"),
        );
        assert_eq!(tcp_fault_carrier(&response), None);
    }

    /// Body-agnostic: an embedder classifies whatever body type it is holding, so classification
    /// never has to happen before a body transformation.
    #[test]
    fn tcp_fault_carrier_is_generic_over_the_body() {
        let mut response: Response<()> = Response::new(());
        response.extensions_mut().insert(TcpFaultKind::Empty);
        assert_eq!(tcp_fault_carrier(&response), Some("EMPTY_RESPONSE"));
    }
}
