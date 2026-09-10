//! Bounded HTTP/1-vs-HTTP/2 detection for the `auto::Builder` listeners (issue #1030).
//!
//! `hyper_util::server::conn::auto::Builder` sniffs the connection preface to decide HTTP/1 vs
//! HTTP/2 (`ReadVersion::poll` in `hyper-util/src/server/conn/auto/mod.rs`) *before* it builds
//! either protocol's `Connection`, and the `header_read_timeout` configured on that `Connection`
//! cannot arm until this sniff resolves. A client that completes the transport handshake and then
//! sends nothing — or sends a partial HTTP/2 preface and stops — leaves this sniff waiting
//! forever, pinning a task, an fd and (over TLS) a `TlsStream`. [`sniff_h2_preface`] runs the same
//! detection ourselves, wrapped in a deadline, before handing the connection to `auto::Builder`.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

/// The HTTP/2 connection preface. Byte-identical to `hyper_util`'s private `H2_PREFACE` constant
/// (`auto/mod.rs:40`); duplicated here because it isn't exported.
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// A stream whose first few bytes were already consumed off the wire by [`sniff_h2_preface`] and
/// must be replayed before anything further is read. Writes pass straight through to `inner`,
/// untouched — sniffing only ever reads.
#[derive(Debug)]
pub struct PrefixedIo<I> {
    /// Bytes already read from `inner` during sniffing, not yet handed back to a reader.
    /// Draining this to empty is a one-way transition: once empty, every subsequent read goes
    /// straight to `inner`, so there is no cost once the connection is past the preface.
    prefix: Bytes,
    inner: I,
}

impl<I> PrefixedIo<I> {
    fn new(prefix: Bytes, inner: I) -> Self {
        Self { prefix, inner }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for PrefixedIo<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.remaining());
            let chunk = self.prefix.split_to(n);
            buf.put_slice(&chunk);
            // Only ever serves from one source per call. A read spanning the prefix/inner
            // boundary still completes correctly because `AsyncReadExt::read_exact` (and any
            // other caller that wants a specific number of bytes) polls again — it will see an
            // empty prefix on the next call and fall through to `inner` below.
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for PrefixedIo<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Why [`sniff_h2_preface`] could not resolve a version.
///
/// `#[non_exhaustive]` because this crosses the rift-mock-core -> rift-http-proxy boundary and a
/// future reason to abandon detection (a byte cap, a per-listener policy) should not be a breaking
/// change for an embedder — the same convention `reconcile` and `fault_io` follow for their own
/// public error enums.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum PrefaceError {
    /// No decision within the deadline: the client completed the transport handshake and then
    /// sent nothing, or sent a proper prefix of the H2 preface and stopped. Either way it is
    /// entirely client-controlled — never worth more than a debug log (see call sites).
    #[error("no request bytes within {0:?}")]
    Timeout(Duration),
    /// The connection closed before enough bytes arrived to tell HTTP/1 from HTTP/2.
    #[error("connection closed before any request bytes")]
    Eof,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Read up to the length of the H2 preface, stopping the instant the version is decidable, and
/// bound the whole thing by `deadline`. On success, returns an `I` that replays whatever was read
/// before falling through to the live stream — byte-exact, so the caller (`auto::Builder`, via
/// its own re-sniff) sees the connection exactly as if this function had never read from it.
pub async fn sniff_h2_preface<I>(
    mut io: I,
    deadline: Duration,
) -> Result<PrefixedIo<I>, PrefaceError>
where
    I: AsyncRead + Unpin,
{
    let mut buf = [0u8; H2_PREFACE.len()];
    let mut filled = 0usize;

    let read_to_a_decision = async {
        // Byte-identical to hyper-util's `ReadVersion::poll` (auto/mod.rs:352-364): keep reading
        // while fewer than 24 bytes are in and everything seen so far still matches the preface;
        // stop the instant a full preface is in hand or a byte diverges. Stopping any later would
        // mean `auto::Builder`'s own re-sniff still has to wait on the socket after us for bytes
        // we already hold; stopping any earlier (e.g. after the first divergent *byte* rather
        // than the first divergent *read*) would disagree with hyper-util about how many bytes it
        // is safe to have consumed, since a single `poll_read` here — like a single `poll_read`
        // there — may return several bytes past the one that actually diverges.
        while filled < H2_PREFACE.len() {
            let n = io.read(&mut buf[filled..]).await?;
            if n == 0 {
                return Err(PrefaceError::Eof);
            }
            let new_filled = filled + n;
            let diverged = buf[filled..new_filled] != H2_PREFACE[filled..new_filled];
            filled = new_filled;
            if diverged {
                break;
            }
        }
        Ok(())
    };

    match tokio::time::timeout(deadline, read_to_a_decision).await {
        Ok(Ok(())) => Ok(PrefixedIo::new(Bytes::copy_from_slice(&buf[..filled]), io)),
        Ok(Err(e)) => Err(e),
        Err(_elapsed) => Err(PrefaceError::Timeout(deadline)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const DEADLINE: Duration = Duration::from_secs(30);

    /// Read exactly `n` bytes, so an assertion cannot pass by coincidence of a short read.
    async fn read_exact_n<R: tokio::io::AsyncRead + Unpin>(io: &mut R, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        io.read_exact(&mut buf).await.expect("read_exact");
        buf
    }

    #[tokio::test]
    async fn resolves_at_the_first_byte_of_an_http1_request() {
        // The common case: `G` diverges from the preface's `P` immediately, so detection must
        // finish after one byte and hand every byte back untouched.
        let (mut client, server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            client
                .write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\n")
                .await
                .expect("write");
        });

        let mut io = sniff_h2_preface(server, DEADLINE)
            .await
            .expect("an HTTP/1 request resolves the version");

        let replayed = read_exact_n(&mut io, 32).await;
        assert_eq!(
            &replayed[..],
            b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\n".as_slice(),
            "every sniffed byte must be replayed, in order — hyper's own detection reads them \
             again from this stream and would otherwise see a truncated request"
        );
    }

    #[tokio::test]
    async fn resolves_on_a_complete_h2_preface() {
        let (mut client, server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            client
                .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
                .await
                .expect("write");
        });

        let mut io = sniff_h2_preface(server, DEADLINE)
            .await
            .expect("a complete preface resolves the version");

        let replayed = read_exact_n(&mut io, 24).await;
        assert_eq!(
            &replayed[..],
            b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".as_slice(),
            "all 24 preface bytes replayed byte-exact"
        );
    }

    #[tokio::test]
    async fn propfind_resolves_at_the_diverging_byte() {
        // `PROPFIND` shares `PR` with the preface and diverges at byte 3 (`O` != `I`): a method
        // that looks like the preface for longer than one byte, so a "we saw a first byte" rule
        // would be wrong about it. Here the whole request is already buffered, so this pins that
        // detection resolves and replays byte-exact WITHOUT reading past the 24-byte cap — it does
        // not, on its own, prove anything about stopping incrementally across several reads. The
        // test below does that.
        let (mut client, server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            client
                .write_all(b"PROPFIND /dav HTTP/1.1\r\n\r\n")
                .await
                .expect("write");
        });

        let mut io = sniff_h2_preface(server, DEADLINE)
            .await
            .expect("PROPFIND resolves as non-h2 at its third byte");

        let replayed = read_exact_n(&mut io, 26).await;
        assert_eq!(&replayed[..], b"PROPFIND /dav HTTP/1.1\r\n\r\n".as_slice());
    }

    #[tokio::test]
    async fn a_preface_like_method_resolves_without_waiting_for_bytes_the_client_never_sends() {
        // The incremental case the buffered PROPFIND test cannot reach: the divergence arrives in
        // a LATER read than the matching prefix, so detection has to decide across reads rather
        // than from one buffered slurp. If our stop rule waited for more bytes than hyper-util's
        // `ReadVersion` does (auto/mod.rs:365-372), this would hang until the deadline — the
        // client has sent everything it intends to and is waiting for a response.
        let (mut client, server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            // Matches the preface exactly so far, so no decision is possible yet.
            client.write_all(b"PR").await.expect("write prefix");
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            // `O` diverges from the preface's `I`, and only now can the version be decided.
            client
                .write_all(b"OPFIND / HTTP/1.1\r\n\r\n")
                .await
                .expect("write rest");
        });

        let mut io =
            tokio::time::timeout(Duration::from_secs(5), sniff_h2_preface(server, DEADLINE))
                .await
                .expect("detection must not wait for bytes the client already considers sent")
                .expect("PROPFIND resolves as non-h2");

        let replayed = read_exact_n(&mut io, 23).await;
        assert_eq!(
            &replayed[..],
            b"PROPFIND / HTTP/1.1\r\n\r\n".as_slice(),
            "bytes from both reads replay in order"
        );
    }

    #[tokio::test]
    async fn partial_preface_then_silence_times_out() {
        // THE BUG, at unit scope. `P` matches the preface's first byte, so hyper-util's detection
        // keeps waiting for byte 2 forever. A fix keyed on "has the client sent anything?" would
        // consider this connection active and leave it parked.
        let (mut client, server) = tokio::io::duplex(1024);
        client.write_all(b"P").await.expect("write");

        let err = sniff_h2_preface(server, Duration::from_millis(150))
            .await
            .expect_err("a partial preface followed by silence must not resolve");

        assert!(
            matches!(err, PrefaceError::Timeout(_)),
            "expected a timeout, got {err:?}"
        );
        // Keep the client alive to the end: dropping it early would close the duplex and turn
        // this into the Eof case, which is a different assertion.
        drop(client);
    }

    #[tokio::test]
    async fn silence_from_the_first_byte_times_out() {
        // The zero-byte case named in the issue: handshake completes, client sends nothing.
        let (client, server) = tokio::io::duplex(1024);

        let err = sniff_h2_preface(server, Duration::from_millis(150))
            .await
            .expect_err("a client that sends nothing must not resolve");

        assert!(
            matches!(err, PrefaceError::Timeout(_)),
            "expected a timeout, got {err:?}"
        );
        drop(client);
    }

    #[tokio::test]
    async fn eof_before_a_decision_is_reported_as_eof() {
        // Distinguished from Timeout so the caller can tell "client hung up" from "client went
        // quiet" — both close the connection, but only one of them is worth a deadline.
        let (mut client, server) = tokio::io::duplex(1024);
        client.write_all(b"PR").await.expect("write");
        drop(client);

        let err = sniff_h2_preface(server, DEADLINE)
            .await
            .expect_err("a half-sent preface then EOF cannot resolve");

        assert!(
            matches!(err, PrefaceError::Eof),
            "expected Eof, got {err:?}"
        );
    }

    #[tokio::test]
    async fn prefixed_io_reads_across_the_boundary_and_passes_writes_through() {
        // A read that spans the replayed prefix and the live socket must not lose or reorder
        // bytes at the seam, and the sniff must leave the write half completely alone.
        let (mut client, server) = tokio::io::duplex(1024);
        client.write_all(b"GET /x").await.expect("write head");

        let mut io = sniff_h2_preface(server, DEADLINE)
            .await
            .expect("resolves at the first byte");

        tokio::spawn(async move {
            client
                .write_all(b" HTTP/1.1\r\n\r\n")
                .await
                .expect("write tail");
            let mut echoed = vec![0u8; 4];
            client.read_exact(&mut echoed).await.expect("read echo");
            assert_eq!(
                &echoed, b"pong",
                "writes must reach the inner stream unchanged"
            );
        });

        let spanning = read_exact_n(&mut io, 19).await;
        assert_eq!(
            &spanning[..],
            b"GET /x HTTP/1.1\r\n\r\n".as_slice(),
            "one read spanning prefix and socket returns the bytes in send order"
        );

        io.write_all(b"pong").await.expect("write through");
        io.flush().await.expect("flush");
    }
}
