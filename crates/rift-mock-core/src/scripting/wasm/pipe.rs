//! A component's stdout or stderr: kept up to a cap, the rest dropped.
//!
//! wasmtime-wasi's `MemoryOutputPipe` traps a write that would pass its capacity, so a component
//! that logs a lot would fail its request. This one accepts every write and keeps only the first
//! `cap` bytes, so a chatty component costs log lines, never its answer.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::p2::{OutputStream, Pollable, StreamError};

/// How much a guest may write in one go: `check_write` always grants it, so a write never waits.
const PERMIT: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub(super) struct LogPipe {
    kept: Arc<parking_lot::Mutex<Vec<u8>>>,
    cap: usize,
}

impl LogPipe {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            kept: Arc::default(),
            cap,
        }
    }

    /// The bytes kept so far, emptying the pipe: an instance kept across calls (issue #1793)
    /// reports each call's lines once, and its cap applies to each call.
    pub(super) fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.kept.lock())
    }

    fn keep(&self, bytes: &[u8]) {
        let mut kept = self.kept.lock();
        let room = self.cap.saturating_sub(kept.len());
        kept.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }
}

impl IsTerminal for LogPipe {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for LogPipe {
    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }

    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

impl OutputStream for LogPipe {
    fn write(&mut self, bytes: bytes::Bytes) -> Result<(), StreamError> {
        self.keep(&bytes);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StreamError> {
        Ok(())
    }

    fn check_write(&mut self) -> Result<usize, StreamError> {
        Ok(PERMIT)
    }
}

#[wasmtime_wasi::async_trait]
impl Pollable for LogPipe {
    async fn ready(&mut self) {}
}

impl tokio::io::AsyncWrite for LogPipe {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.keep(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Past its cap the pipe still accepts every write and keeps the first `cap` bytes: a guest's
    // `println!` never fails, which would trap it.
    #[test]
    fn writes_past_the_cap_are_accepted_and_dropped() {
        let pipe = LogPipe::new(8);
        let mut stream = pipe.p2_stream();
        assert_eq!(stream.check_write().ok(), Some(PERMIT));
        assert!(stream.write(bytes::Bytes::from_static(b"hello ")).is_ok());
        assert!(
            stream
                .write(bytes::Bytes::from_static(b"world, again"))
                .is_ok()
        );
        assert_eq!(stream.check_write().ok(), Some(PERMIT));
        assert_eq!(pipe.take(), b"hello wo");
    }
}
