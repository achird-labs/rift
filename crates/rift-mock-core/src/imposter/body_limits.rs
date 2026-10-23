//! Body size limits shared by the imposter listeners, the proxy's upstream read and the admin API
//! (issue #1439).
//!
//! Mountebank has no body cap of its own and serves bodies up to V8's string limit (about
//! 512 MiB), past which it crashes. Rift refuses nothing Mountebank can serve: the default
//! per-body ceiling is that same 512 MiB, and a body over it gets a clean `413` instead of a dead
//! process. The ceiling bounds one body; a shared in-flight **memory budget** bounds them all
//! together, so concurrent large uploads get `503` + `Retry-After` rather than exhausting memory.
//! Both are configurable, so an embedder (rift-cluster) can tighten them for its pod sizes.

use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default per-body ceiling: Mountebank's own breaking point, so no body it serves is refused.
pub const DEFAULT_MAX_BODY_BYTES: usize = 512 * 1024 * 1024;

/// Default in-flight memory budget shared by every body a manager is buffering.
pub const DEFAULT_BODY_MEMORY_BUDGET: usize = 1024 * 1024 * 1024;

/// How long a request body with a known length waits for the budget before it is refused. The wait
/// is bounded, so bodies waiting on each other's shares cannot deadlock: the worst case is a `503`
/// after this long.
const RESERVE_WAIT: Duration = Duration::from_secs(2);

/// The budget is counted in KiB: `Semaphore::acquire_many` takes a `u32`, and bytes would cap a
/// single reservation at 4 GiB.
const UNIT: usize = 1024;

/// Validated body limits: a per-body ceiling and an optional shared budget at least as large.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyLimits {
    max_body_bytes: NonZeroUsize,
    budget_bytes: Option<NonZeroUsize>,
}

/// Why a [`BodyLimits`] configuration is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BodyLimitsError {
    #[error("the maximum body size must be at least 1 byte")]
    ZeroCeiling,
    #[error(
        "the body memory budget ({budget} bytes) is smaller than the maximum body size ({max} \
         bytes), so a body under the ceiling could never be admitted"
    )]
    BudgetBelowCeiling { budget: usize, max: usize },
}

impl BodyLimits {
    /// `budget_bytes` of `None` means unbounded. A budget below the ceiling is refused: a body
    /// under the ceiling could then never be admitted.
    pub fn new(
        max_body_bytes: usize,
        budget_bytes: Option<usize>,
    ) -> Result<Self, BodyLimitsError> {
        let max = NonZeroUsize::new(max_body_bytes).ok_or(BodyLimitsError::ZeroCeiling)?;
        let budget = match budget_bytes {
            None => None,
            Some(budget) if budget < max_body_bytes => {
                return Err(BodyLimitsError::BudgetBelowCeiling {
                    budget,
                    max: max_body_bytes,
                });
            }
            Some(budget) => NonZeroUsize::new(budget),
        };
        Ok(Self {
            max_body_bytes: max,
            budget_bytes: budget,
        })
    }

    #[must_use]
    pub fn max_body_bytes(&self) -> usize {
        self.max_body_bytes.get()
    }

    #[must_use]
    pub fn budget_bytes(&self) -> Option<usize> {
        self.budget_bytes.map(NonZeroUsize::get)
    }
}

impl Default for BodyLimits {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BODY_BYTES, Some(DEFAULT_BODY_MEMORY_BUDGET))
            .expect("the defaults satisfy budget >= ceiling")
    }
}

/// Why a body was not collected.
#[derive(Debug, thiserror::Error)]
pub enum BodyReadError {
    /// The body is over the per-body ceiling: a `413`.
    #[error("body exceeds the maximum size of {limit} bytes")]
    TooLarge { limit: usize },
    /// The shared budget cannot hold the body right now: a `503` with `Retry-After`.
    #[error("not enough body memory is free to accept this body; retry shortly")]
    BudgetExhausted,
    /// The body could not be read to completion: a client transmission failure (#694).
    #[error("failed to read body: {0}")]
    Read(Box<dyn std::error::Error + Send + Sync>),
}

/// A collected body and its share of the budget, which is released when this is dropped. Keep it
/// alive until the response is built: the bytes stay in memory throughout matching.
#[derive(Debug)]
pub struct CollectedBody {
    bytes: Bytes,
    _reservation: Option<OwnedSemaphorePermit>,
}

impl CollectedBody {
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

impl std::ops::Deref for CollectedBody {
    type Target = Bytes;

    fn deref(&self) -> &Bytes {
        &self.bytes
    }
}

/// The limits plus the budget they share. One per [`ImposterManager`](super::ImposterManager), so
/// every listener, the proxy and the admin API draw on the same budget.
#[derive(Debug)]
pub struct BodyGuard {
    limits: BodyLimits,
    budget: Option<Arc<Semaphore>>,
}

impl BodyGuard {
    #[must_use]
    pub fn new(limits: BodyLimits) -> Self {
        let budget = limits
            .budget_bytes()
            .map(|bytes| Arc::new(Semaphore::new(units(bytes) as usize)));
        Self { limits, budget }
    }

    #[must_use]
    pub fn limits(&self) -> BodyLimits {
        self.limits
    }

    /// Start collecting a request body of `content_length` bytes, if known.
    ///
    /// A known length over the ceiling is refused before a byte is read, so with
    /// `Expect: 100-continue` the client is never told to send it. A known length under it reserves
    /// its whole share up front, waiting up to [`RESERVE_WAIT`] for the budget. An unknown length
    /// (chunked) reserves as it goes and never waits.
    pub async fn start(&self, content_length: Option<u64>) -> Result<BodyCollector, BodyReadError> {
        let (mut collector, length) = self.open(content_length)?;
        if let (Some(budget), Some(length)) = (&self.budget, length) {
            let wanted = units(length);
            if wanted > 0 {
                // The semaphore is never closed, so an `Err` from it cannot happen; it is treated
                // as exhaustion rather than unwrapped.
                let permit = tokio::time::timeout(
                    RESERVE_WAIT,
                    Arc::clone(budget).acquire_many_owned(wanted),
                )
                .await
                .ok()
                .and_then(Result::ok)
                .ok_or(BodyReadError::BudgetExhausted)?;
                collector.hold(permit, wanted);
            }
        }
        collector.reserve_capacity(length);
        Ok(collector)
    }

    /// [`Self::start`] without waiting for the budget, for a body read while another is already
    /// held — the proxy's upstream response, read while the request's body holds its share. Waiting
    /// there would make concurrent proxied requests time out on each other.
    pub async fn start_without_waiting(
        &self,
        content_length: Option<u64>,
    ) -> Result<BodyCollector, BodyReadError> {
        self.try_start(content_length)
    }

    /// [`Self::start_without_waiting`] for a caller that cannot await: a `lookup` CSV file read on
    /// a blocking thread while the request's body holds its share (issue #1654).
    pub fn try_start(&self, content_length: Option<u64>) -> Result<BodyCollector, BodyReadError> {
        let (mut collector, length) = self.open(content_length)?;
        if let (Some(budget), Some(length)) = (&self.budget, length) {
            let wanted = units(length);
            if wanted > 0 {
                let permit = Arc::clone(budget)
                    .try_acquire_many_owned(wanted)
                    .map_err(|_| BodyReadError::BudgetExhausted)?;
                collector.hold(permit, wanted);
            }
        }
        collector.reserve_capacity(length);
        Ok(collector)
    }

    /// An empty collector, and the known length checked against the ceiling. A known length over
    /// it is refused here, before anything is reserved or read.
    fn open(
        &self,
        content_length: Option<u64>,
    ) -> Result<(BodyCollector, Option<usize>), BodyReadError> {
        let max = self.limits.max_body_bytes();
        let length = content_length
            .map(|length| {
                usize::try_from(length)
                    .ok()
                    .filter(|length| *length <= max)
                    .ok_or(BodyReadError::TooLarge { limit: max })
            })
            .transpose()?;
        let collector = BodyCollector {
            buf: BytesMut::new(),
            max,
            budget: self.budget.clone(),
            reservation: None,
            reserved_units: 0,
        };
        Ok((collector, length))
    }

    /// Collect a whole `hyper` body under the limits. The length comes from the body's own size
    /// hint, which is exact when the request carried `Content-Length`.
    pub async fn collect<B>(&self, body: B) -> Result<CollectedBody, BodyReadError>
    where
        B: hyper::body::Body<Data = Bytes>,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let mut collector = self.start(body.size_hint().exact()).await?;
        let mut body = std::pin::pin!(body);
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| BodyReadError::Read(e.into()))?;
            if let Ok(data) = frame.into_data() {
                collector.push(&data)?;
            }
        }
        Ok(collector.finish())
    }
}

impl Default for BodyGuard {
    fn default() -> Self {
        Self::new(BodyLimits::default())
    }
}

/// A body being collected chunk by chunk under a [`BodyGuard`].
#[derive(Debug)]
pub struct BodyCollector {
    buf: BytesMut,
    max: usize,
    budget: Option<Arc<Semaphore>>,
    reservation: Option<OwnedSemaphorePermit>,
    reserved_units: u32,
}

impl BodyCollector {
    /// Take `permit`, worth `units` of the budget, as this body's up-front share.
    fn hold(&mut self, permit: OwnedSemaphorePermit, units: u32) {
        self.reservation = Some(permit);
        self.reserved_units = units;
    }

    /// Allocate a known length once its share is held, never before: a refused body allocates
    /// nothing.
    fn reserve_capacity(&mut self, length: Option<usize>) {
        if let Some(length) = length {
            self.buf = BytesMut::with_capacity(length);
        }
    }

    /// Append a chunk, refusing it past the ceiling or when the budget cannot cover it now.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), BodyReadError> {
        let length = self.buf.len() + chunk.len();
        if length > self.max {
            return Err(BodyReadError::TooLarge { limit: self.max });
        }
        if let Some(budget) = &self.budget {
            let wanted = units(length);
            if wanted > self.reserved_units {
                let more = Arc::clone(budget)
                    .try_acquire_many_owned(wanted - self.reserved_units)
                    .map_err(|_| BodyReadError::BudgetExhausted)?;
                match &mut self.reservation {
                    Some(held) => held.merge(more),
                    None => self.reservation = Some(more),
                }
                self.reserved_units = wanted;
            }
        }
        self.buf.extend_from_slice(chunk);
        Ok(())
    }

    #[must_use]
    pub fn finish(self) -> CollectedBody {
        CollectedBody {
            bytes: self.buf.freeze(),
            _reservation: self.reservation,
        }
    }
}

/// `bytes` in budget units, rounded up. Saturates at `u32::MAX` units (4 TiB), past any ceiling a
/// semaphore could grant, so an oversized request is refused rather than wrapped.
fn units(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(UNIT)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// A body of the given frames with no known length, as a chunked upload is.
    struct Chunks(VecDeque<Result<Bytes, std::io::Error>>);

    impl hyper::body::Body for Chunks {
        type Data = Bytes;
        type Error = std::io::Error;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, std::io::Error>>> {
            Poll::Ready(self.0.pop_front().map(|r| r.map(hyper::body::Frame::data)))
        }
    }

    fn guard(max: usize, budget: Option<usize>) -> BodyGuard {
        BodyGuard::new(BodyLimits::new(max, budget).expect("valid limits"))
    }

    fn chunked(chunks: Vec<&'static [u8]>) -> Chunks {
        Chunks(
            chunks
                .into_iter()
                .map(|c| Ok(Bytes::from_static(c)))
                .collect(),
        )
    }

    #[test]
    fn a_budget_below_the_ceiling_is_refused() {
        assert_eq!(
            BodyLimits::new(2048, Some(1024)),
            Err(BodyLimitsError::BudgetBelowCeiling {
                budget: 1024,
                max: 2048
            })
        );
        assert_eq!(BodyLimits::new(0, None), Err(BodyLimitsError::ZeroCeiling));
        assert!(BodyLimits::new(2048, Some(2048)).is_ok());
        assert!(BodyLimits::new(2048, None).is_ok());
    }

    #[test]
    fn the_defaults_are_mountebanks_ceiling_and_a_one_gib_budget() {
        let limits = BodyLimits::default();
        assert_eq!(limits.max_body_bytes(), 536_870_912);
        assert_eq!(limits.budget_bytes(), Some(1_073_741_824));
    }

    #[tokio::test]
    async fn a_body_at_the_ceiling_is_collected_and_one_byte_over_is_refused() {
        let g = guard(4096, Some(8192));
        let at = g
            .collect(Full::new(Bytes::from(vec![b'x'; 4096])))
            .await
            .expect("at the ceiling");
        assert_eq!(at.len(), 4096);
        assert!(matches!(
            g.collect(Full::new(Bytes::from(vec![b'x'; 4097]))).await,
            Err(BodyReadError::TooLarge { limit: 4096 })
        ));
    }

    #[tokio::test]
    async fn a_chunked_body_over_the_ceiling_is_refused() {
        let g = guard(4, None);
        assert!(matches!(
            g.collect(chunked(vec![b"ab", b"cd", b"e"])).await,
            Err(BodyReadError::TooLarge { limit: 4 })
        ));
        let ok = g
            .collect(chunked(vec![b"ab", b"cd"]))
            .await
            .expect("at the ceiling");
        assert_eq!(ok.as_ref(), b"abcd");
    }

    /// A known length over the ceiling is refused before the body is polled.
    #[tokio::test]
    async fn a_known_length_over_the_ceiling_is_refused_up_front() {
        let g = guard(1024, Some(4096));
        assert!(matches!(
            g.start(Some(1025)).await,
            Err(BodyReadError::TooLarge { limit: 1024 })
        ));
    }

    /// The budget is held while a collected body lives, refuses a chunked body that does not fit,
    /// and is free again once the body is dropped.
    #[tokio::test]
    async fn the_budget_is_held_until_the_body_is_dropped() {
        let g = guard(4096, Some(4096));
        let held = g
            .collect(Full::new(Bytes::from(vec![b'x'; 4096])))
            .await
            .expect("fits the budget");
        assert!(matches!(
            g.collect(chunked(vec![b"y"])).await,
            Err(BodyReadError::BudgetExhausted)
        ));
        drop(held);
        g.collect(chunked(vec![b"y"]))
            .await
            .expect("the budget is free again");
    }

    /// A known length waits for the budget rather than failing at once, and is refused when the
    /// wait expires.
    #[tokio::test(start_paused = true)]
    async fn a_known_length_waits_for_the_budget_then_is_refused() {
        let g = Arc::new(guard(4096, Some(4096)));
        let held = g
            .collect(Full::new(Bytes::from(vec![b'x'; 4096])))
            .await
            .expect("fits the budget");
        let waiter = {
            let g = Arc::clone(&g);
            tokio::spawn(async move { g.start(Some(1024)).await.map(BodyCollector::finish) })
        };
        tokio::time::advance(Duration::from_millis(500)).await;
        drop(held);
        assert!(waiter.await.expect("task").is_ok(), "admitted once freed");

        let _held = g
            .collect(Full::new(Bytes::from(vec![b'x'; 4096])))
            .await
            .expect("fits the budget");
        assert!(matches!(
            g.start(Some(1024)).await,
            Err(BodyReadError::BudgetExhausted)
        ));
    }

    #[tokio::test]
    async fn starting_without_waiting_refuses_at_once() {
        let g = guard(4096, Some(4096));
        let _held = g
            .collect(Full::new(Bytes::from(vec![b'x'; 4096])))
            .await
            .expect("fits the budget");
        let started = std::time::Instant::now();
        assert!(matches!(
            g.start_without_waiting(Some(1024)).await,
            Err(BodyReadError::BudgetExhausted)
        ));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn an_unbounded_budget_never_refuses() {
        let g = guard(4096, None);
        let _a = g
            .collect(Full::new(Bytes::from(vec![b'x'; 4096])))
            .await
            .expect("a");
        let _b = g
            .collect(Full::new(Bytes::from(vec![b'x'; 4096])))
            .await
            .expect("b");
    }

    #[tokio::test]
    async fn a_read_failure_is_not_a_size_error() {
        let failing = Chunks(VecDeque::from([Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ))]));
        assert!(matches!(
            guard(4096, None).collect(failing).await,
            Err(BodyReadError::Read(_))
        ));
    }
}
