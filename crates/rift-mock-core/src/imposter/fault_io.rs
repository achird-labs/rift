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
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use hyper::{Response, Version};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::{Instant, Sleep};
use tracing::debug;

// Lingering-close bounds (issue #1460): total drain time, the longest silence between reads, and
// the most a client may send before the listener stops discarding it. Time, not size, is the real
// bound: draining costs bandwidth, never memory.
const LINGER_DEADLINE: Duration = Duration::from_secs(2);
const LINGER_IDLE: Duration = Duration::from_secs(1);
const LINGER_MAX_BYTES: usize = 64 * 1024 * 1024;
const LINGER_SCRATCH: usize = 16 * 1024;

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
    /// Every spelling and the kind it names: each kind's canonical WireMock name and Rift's short
    /// aliases. [`Self::parse`] reads this table and [`Self::SPELLINGS`] is its first column, so
    /// there is one list, not two to keep in step (issue #1342).
    const TABLE: [(&'static str, Self); 9] = [
        ("reset", Self::Reset),
        ("CONNECTION_RESET_BY_PEER", Self::Reset),
        ("empty", Self::Empty),
        ("EMPTY_RESPONSE", Self::Empty),
        ("garbage", Self::RandomData),
        ("random", Self::RandomData),
        ("RANDOM_DATA_THEN_CLOSE", Self::RandomData),
        ("malformed", Self::MalformedChunk),
        ("MALFORMED_RESPONSE_CHUNK", Self::MalformedChunk),
    ];

    /// Every spelling [`Self::parse`] accepts, the list `rift-lint` and the JSON Schema enumerate.
    pub const SPELLINGS: [&'static str; 9] = {
        let mut spellings = [""; 9];
        let mut i = 0;
        while i < spellings.len() {
            spellings[i] = Self::TABLE[i].0;
            i += 1;
        }
        spellings
    };

    /// Parse a `_rift.fault.tcp` string. Accepts the WireMock names and Rift's short aliases.
    #[must_use]
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::TABLE
            .iter()
            .find(|(spelling, _)| *spelling == value)
            .map(|(_, kind)| *kind)
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

/// Per-connection slot for a TCP fault: armed when a request's response is a carrier, read by
/// [`FaultIo`] on that connection's next write.
///
/// Public so a listener other than the imposter's own serve loop — the admin API's `/__rift/`
/// gateway, the front door, or an embedder's listener — can apply a carrier as the real transport
/// fault (issue #1234): wrap the accepted stream in [`FaultIo::new`] with a fresh cell, then arm
/// the cell from the service (`rift_http_proxy::gateway::apply_tcp_fault` does both halves of the
/// decision). A newtype so the lock type stays out of the public API. Cloning shares the slot.
///
/// It also carries the connection's lingering-close request (issue #1460, [`Self::arm_linger`]).
#[derive(Clone, Debug, Default)]
pub struct FaultCell(Arc<CellState>);

#[derive(Debug, Default)]
struct CellState {
    fault: Mutex<Option<TcpFaultKind>>,
    linger: AtomicBool,
}

impl FaultCell {
    /// An empty (un-armed) slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm the slot: the connection's next write performs `kind` instead of writing.
    ///
    /// Only meaningful on an HTTP/1 connection, where the next write is this request's response.
    /// On HTTP/2 the next write may belong to another stream, and the abort would take down every
    /// stream on the connection.
    pub fn arm(&self, kind: TcpFaultKind) {
        *self.0.fault.lock() = Some(kind);
    }

    /// Ask for a lingering close: when hyper shuts this connection down, [`FaultIo`] shuts the
    /// write side, then reads and discards whatever the client is still sending — for at most 2 s,
    /// 1 s of silence or 64 MiB — before letting it close.
    ///
    /// The drain is timed with `tokio::time`, so the connection must be driven on a Tokio runtime
    /// with its time driver enabled (a disarmed `FaultIo` needs none). It can hold a graceful
    /// shutdown or imposter delete for up to the 2 s deadline.
    ///
    /// For a response refused with the request body unread (issue #1460). Closing a socket with
    /// unread data makes the kernel send a RST, and a client still writing that body then sees
    /// "connection reset" instead of the refusal. Only arm it on HTTP/1, where the close follows
    /// this response; [`arm_linger_if_requested`] makes that decision from the response.
    pub fn arm_linger(&self) {
        self.0.linger.store(true, Ordering::Relaxed);
    }

    fn linger_armed(&self) -> bool {
        self.0.linger.load(Ordering::Relaxed)
    }

    /// Take (not just read) the armed fault so it fires exactly once. `FaultIo` only.
    fn take(&self) -> Option<TcpFaultKind> {
        self.0.fault.lock().take()
    }
}

/// Response extension: the listener should close this connection with a lingering close
/// (issue #1460, [`FaultCell::arm_linger`]).
///
/// Set on a refusal written while the request body is still unread — the `413`/`503` body-limit
/// doors. The response also says `connection: close`; this marker is what tells the listener the
/// close must not discard the answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LingerOnClose;

/// Arm `cell`'s lingering close if `response` carries [`LingerOnClose`] and the connection is
/// HTTP/1. Each listener that wraps its connections in [`FaultIo`] calls this on every response.
///
/// HTTP/2 is left alone: a refused stream is reset on its own while the connection carries on, so
/// there is no close to linger on.
pub fn arm_linger_if_requested<B>(response: &Response<B>, version: Version, cell: &FaultCell) {
    if response.extensions().get::<LingerOnClose>().is_some()
        && matches!(
            version,
            Version::HTTP_09 | Version::HTTP_10 | Version::HTTP_11
        )
    {
        cell.arm_linger();
    }
}

/// The error an injected TCP fault surfaces as: the `io::Error` [`FaultIo`] returns to abort the
/// connection wraps it, and a listener's HTTP/2 service returns it to reset one stream.
///
/// A listener tells an injected fault from a real connection failure with [`is_injected_fault`],
/// so the fault the config asked for is not logged as a server error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InjectedFault(pub TcpFaultKind);

impl std::fmt::Display for InjectedFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("rift: injected _rift.fault.tcp")
    }
}

impl std::error::Error for InjectedFault {}

/// Whether `err`, or anything in its source chain, is an [`InjectedFault`].
///
/// Not a plain `source()` walk: `io::Error::source()` returns the *inner* error's source, not the
/// inner error, so a walk through the `io::Error` [`FaultIo`] returns steps straight over the
/// marker. Each `io::Error` in the chain is therefore also opened with `get_ref()`.
#[must_use]
pub fn is_injected_fault(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(err);
    while let Some(e) = current {
        if e.is::<InjectedFault>() {
            return true;
        }
        if let Some(inner) = e.downcast_ref::<io::Error>().and_then(io::Error::get_ref)
            && is_injected_fault(inner)
        {
            return true;
        }
        current = e.source();
    }
    false
}

/// A `TcpStream` wrapper that, when its [`FaultCell`] is armed, applies a connection fault on the
/// next write instead of forwarding hyper's response. Reads and (un-armed) writes pass straight
/// through, so it can wrap every accepted connection at no cost to ordinary traffic.
///
/// It also performs the cell's lingering close (issue #1460) in `poll_shutdown`.
#[derive(Debug)]
pub struct FaultIo {
    inner: TcpStream,
    fault: FaultCell,
    linger: Linger,
}

/// Where an armed lingering close has got to. Created on the first armed `poll_shutdown`.
enum Linger {
    NotStarted,
    /// Write side being shut down: the client is to see the response, then a FIN.
    ShuttingWrite,
    Draining(Box<Drain>),
    Done,
}

struct Drain {
    deadline: Pin<Box<Sleep>>,
    idle: Pin<Box<Sleep>>,
    discarded: usize,
    scratch: Box<[u8; LINGER_SCRATCH]>,
}

impl std::fmt::Debug for Linger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStarted => f.write_str("NotStarted"),
            Self::ShuttingWrite => f.write_str("ShuttingWrite"),
            Self::Draining(drain) => write!(f, "Draining({} bytes)", drain.discarded),
            Self::Done => f.write_str("Done"),
        }
    }
}

impl Drain {
    fn new() -> Self {
        Self {
            deadline: Box::pin(tokio::time::sleep(LINGER_DEADLINE)),
            idle: Box::pin(tokio::time::sleep(LINGER_IDLE)),
            discarded: 0,
            scratch: Box::new([0; LINGER_SCRATCH]),
        }
    }

    /// Read and discard until EOF, a read error, the byte backstop or a timer. Never fails: the
    /// connection is closing either way, and a read error only means it closed sooner.
    fn poll(&mut self, inner: &mut TcpStream, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            if self.deadline.as_mut().poll(cx).is_ready() || self.idle.as_mut().poll(cx).is_ready()
            {
                return Poll::Ready(());
            }
            let mut buf = ReadBuf::new(&mut self.scratch[..]);
            match ready!(Pin::new(&mut *inner).poll_read(cx, &mut buf)) {
                Ok(()) if !buf.filled().is_empty() => {
                    self.discarded += buf.filled().len();
                    if self.discarded >= LINGER_MAX_BYTES {
                        return Poll::Ready(());
                    }
                    self.idle.as_mut().reset(Instant::now() + LINGER_IDLE);
                }
                Ok(()) => return Poll::Ready(()),
                Err(e) => {
                    debug!("lingering close: read ended the drain: {e}");
                    return Poll::Ready(());
                }
            }
        }
    }
}

impl FaultIo {
    /// Wrap an accepted connection. Wrap before any protocol sniffing or TLS so the fault lands on
    /// the raw socket.
    #[must_use]
    pub fn new(inner: TcpStream, fault: FaultCell) -> Self {
        Self {
            inner,
            fault,
            linger: Linger::NotStarted,
        }
    }

    /// Apply the fault to the raw socket. Returns the error used to abort the hyper connection so
    /// the socket is dropped (and closed) immediately afterwards. The raw socket calls are
    /// best-effort: on failure the connection still breaks, but the *observed* fault may degrade
    /// (e.g. a failed `set_zero_linger` reset becomes a graceful close), so each failure is logged.
    fn trip(&self, kind: TcpFaultKind) -> io::Error {
        match kind {
            // SO_LINGER(0): dropping the socket now emits RST rather than a graceful FIN.
            TcpFaultKind::Reset => {
                if let Err(e) = self.inner.set_zero_linger() {
                    debug!("rift fault reset: set_zero_linger failed, degraded to graceful close: {e}");
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
        io::Error::other(InjectedFault(kind))
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
        if let Some(kind) = self.fault.take() {
            return Poll::Ready(Err(self.trip(kind)));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if matches!(this.linger, Linger::NotStarted) {
            if !this.fault.linger_armed() {
                return Pin::new(&mut this.inner).poll_shutdown(cx);
            }
            this.linger = Linger::ShuttingWrite;
        }
        loop {
            match &mut this.linger {
                Linger::NotStarted | Linger::ShuttingWrite => {
                    ready!(Pin::new(&mut this.inner).poll_shutdown(cx))?;
                    this.linger = Linger::Draining(Box::new(Drain::new()));
                }
                Linger::Draining(drain) => {
                    ready!(drain.poll(&mut this.inner, cx));
                    this.linger = Linger::Done;
                }
                Linger::Done => return Poll::Ready(Ok(())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::Full;

    /// The table behind `SPELLINGS` and `parse`: every spelling parses to the kind beside it, every
    /// kind's canonical name is one of its spellings, and the lookup is case-sensitive.
    #[test]
    fn spellings_parse_to_their_kind() {
        for (spelling, kind) in TcpFaultKind::TABLE {
            assert_eq!(TcpFaultKind::parse(spelling), Some(kind), "{spelling}");
            assert!(TcpFaultKind::SPELLINGS.contains(&spelling));
            assert!(
                TcpFaultKind::SPELLINGS.contains(&kind.canonical_name()),
                "{kind:?}"
            );
        }
        assert!(
            TcpFaultKind::parse("RESET").is_none(),
            "spellings are case-sensitive"
        );
        assert!(TcpFaultKind::parse("").is_none());
    }

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

    /// An outer error whose `source()` is the given `io::Error` — the shape hyper's connection
    /// error takes around the `io::Error` `FaultIo` returns.
    #[derive(Debug)]
    struct Outer(io::Error);

    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("connection error")
        }
    }

    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// Issue #1234: the marker is found through an `io::Error` nested in an outer error.
    #[test]
    fn is_injected_fault_sees_through_a_wrapped_io_error() {
        let err = Outer(io::Error::other(InjectedFault(TcpFaultKind::Reset)));
        assert!(is_injected_fault(&err));
        // Directly, and as the bare io::Error too.
        assert!(is_injected_fault(&InjectedFault(TcpFaultKind::Empty)));
        assert!(is_injected_fault(&io::Error::other(InjectedFault(
            TcpFaultKind::MalformedChunk
        ))));
    }

    /// The trap the implementation avoids: `io::Error::source()` skips its own inner error, so a
    /// plain `source()` walk never reaches the marker. Pinned so a "simplification" to that walk
    /// fails here rather than silently re-logging every injected fault as a server error.
    #[test]
    fn a_naive_source_walk_misses_the_marker() {
        fn naive(err: &(dyn std::error::Error + 'static)) -> bool {
            let mut current = Some(err);
            while let Some(e) = current {
                if e.is::<InjectedFault>() {
                    return true;
                }
                current = e.source();
            }
            false
        }
        let err = Outer(io::Error::other(InjectedFault(TcpFaultKind::Reset)));
        assert!(!naive(&err), "the naive walk steps over the marker");
        assert!(is_injected_fault(&err));
    }

    #[test]
    fn is_injected_fault_rejects_an_ordinary_io_error() {
        let reset = io::Error::from(io::ErrorKind::ConnectionReset);
        assert!(!is_injected_fault(&reset));
        assert!(!is_injected_fault(&Outer(reset)));
        assert!(!is_injected_fault(&Outer(io::Error::other("boom"))));
    }

    /// Armed once, fires once; clones share the slot (the listener keeps one, `FaultIo` the other).
    #[test]
    fn fault_cell_is_shared_and_taken_once() {
        let cell = FaultCell::new();
        let other = cell.clone();
        assert_eq!(other.take(), None);
        cell.arm(TcpFaultKind::RandomData);
        assert_eq!(other.take(), Some(TcpFaultKind::RandomData));
        assert_eq!(cell.take(), None);
    }

    async fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let client = TcpStream::connect(listener.local_addr().expect("addr"))
            .await
            .expect("connect");
        let (server, _) = listener.accept().await.expect("accept");
        (server, client)
    }

    #[test]
    fn linger_is_armed_through_any_clone_and_only_for_http1() {
        let mut response = Response::new(());
        response.extensions_mut().insert(LingerOnClose);
        let h2 = FaultCell::new();
        arm_linger_if_requested(&response, Version::HTTP_2, &h2);
        assert!(!h2.linger_armed(), "HTTP/2 has no close to linger on");

        let cell = FaultCell::new();
        arm_linger_if_requested(&Response::new(()), Version::HTTP_11, &cell);
        assert!(!cell.linger_armed(), "only a response that asks arms it");
        arm_linger_if_requested(&response, Version::HTTP_11, &cell.clone());
        assert!(cell.linger_armed());
    }

    /// Issue #1460: an armed shutdown sends the FIN first, then discards what the peer sends until
    /// it closes, so nothing is left unread to turn the close into a RST.
    #[tokio::test]
    async fn an_armed_shutdown_sends_fin_then_drains_to_eof() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (server, mut client) = connected_pair().await;
        let cell = FaultCell::new();
        cell.arm_linger();
        let mut io = FaultIo::new(server, cell);
        let shutdown = tokio::spawn(async move { io.shutdown().await });

        let mut sink = [0u8; 1];
        assert_eq!(
            client.read(&mut sink).await.expect("read"),
            0,
            "the FIN comes before the drain"
        );
        client
            .write_all(&[b'x'; 256 * 1024])
            .await
            .expect("drained");
        client.shutdown().await.expect("client FIN");
        let result = tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .expect("EOF ends the drain at once")
            .expect("task");
        assert!(result.is_ok());
    }

    /// The drain is bounded by its idle timer when the peer goes quiet without closing.
    #[tokio::test(start_paused = true)]
    async fn an_armed_shutdown_stops_draining_a_silent_peer() {
        use tokio::io::AsyncWriteExt;
        let (server, _client) = connected_pair().await;
        let cell = FaultCell::new();
        cell.arm_linger();
        let mut io = FaultIo::new(server, cell);
        let started = Instant::now();
        io.shutdown().await.expect("shutdown");
        assert!(started.elapsed() >= LINGER_IDLE);
        assert!(started.elapsed() < LINGER_DEADLINE);
    }

    /// Unarmed, shutdown is the plain socket shutdown: it never waits on the peer.
    #[tokio::test(start_paused = true)]
    async fn an_unarmed_shutdown_does_not_wait_for_the_peer() {
        use tokio::io::AsyncWriteExt;
        let (server, _client) = connected_pair().await;
        let mut io = FaultIo::new(server, FaultCell::new());
        let started = Instant::now();
        io.shutdown().await.expect("shutdown");
        assert_eq!(started.elapsed(), Duration::ZERO);
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
