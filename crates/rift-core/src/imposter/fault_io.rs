//! Real connection-level TCP faults for `_rift.fault.tcp` (issue #239).
//!
//! A normal `Response` is framed and sent by hyper, so it can never reproduce a transport-level
//! failure. Instead, the matched fault is recorded in a per-connection [`FaultCell`] (set by the
//! request handler via a `Response` extension) and applied by [`FaultIo`], a thin wrapper around
//! the connection's `TcpStream`: on the next write hyper attempts (the response), `FaultIo`
//! performs the fault on the raw socket and returns an error so hyper aborts the connection. The
//! client then observes a genuine reset / empty / malformed / random-data failure, matching
//! WireMock's `Fault` semantics.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tracing::debug;

/// The four WireMock-equivalent connection faults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TcpFaultKind {
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
}
