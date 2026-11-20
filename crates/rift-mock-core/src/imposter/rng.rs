//! An imposter's random stream (issue #1755): one SplitMix64, seeded with `_rift.scriptEngine.seed`
//! when set and from OS entropy otherwise, so behaviour and cost are the same either way.
//!
//! What draws from it: `_rift.fault` rolls, the template `randomInt`, and a wasm component's
//! `wasi:random` (`random`, `insecure`, `insecure-seed`). With a seed and a frozen clock (#1754), a
//! serialized request sequence replays byte for byte; concurrent requests interleave their draws
//! nondeterministically, as they would against any single stream. What does not: the `wait`
//! behaviour's random range, which only changes timing. JavaScript's `Math.random()` draws from it
//! too (#1756, through `scripting::env`); Rhai has no random function.
//! The template `uuid` draws from it too.
//!
//! Readers draw through a request's [`RequestRng`], never the imposter's stream directly, which is
//! where request-keyed streams go (#1795). A fresh stream starts when the imposter is created,
//! recreated, or restarted (`restart_runtime_state`), so request *n* after any of them sees the
//! same draws; a request keeps the stream it started on.

use parking_lot::Mutex;
use std::convert::Infallible;
use std::sync::Arc;

/// SplitMix64 (Steele, Lea and Flood, 2014): a 64-bit state stepped by a fixed odd constant and
/// mixed. Fast, and every seed gives a full-period stream.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// `_rift.scriptEngine.seed`. Decoded tolerantly: engines before #1755 admitted any value under this
/// key and ignored it, so a stored config carrying one must still replay (rift-cluster's log,
/// `--datadir`). A value that is not an unsigned 64-bit integer is kept as a JSON value (an integer
/// past `u64` re-encodes as a float), runs unseeded, and is logged when the imposter is built.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Seed {
    /// The seed.
    Value(u64),
    /// Anything else: runs unseeded.
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unrecognized(serde_json::Value),
}

impl Seed {
    /// The seed, or `None` for a value that is not one.
    #[must_use]
    pub fn value(&self) -> Option<u64> {
        match self {
            Self::Value(seed) => Some(*seed),
            Self::Unrecognized(_) => None,
        }
    }
}

/// One imposter's stream.
#[derive(Debug)]
pub struct ImposterRng {
    seed: Option<u64>,
    stream: Mutex<SplitMix64>,
}

fn entropy() -> u64 {
    rand::random()
}

impl ImposterRng {
    /// A stream from `seed`, or from OS entropy when `None`.
    #[must_use]
    pub fn new(seed: Option<u64>) -> Self {
        Self {
            seed,
            stream: Mutex::new(SplitMix64::new(seed.unwrap_or_else(entropy))),
        }
    }

    /// The configured seed.
    #[must_use]
    pub fn seed(&self) -> Option<u64> {
        self.seed
    }

    /// Back to the start: the seed's first draw, or a fresh entropy seed.
    pub fn reseed(&self) {
        *self.stream.lock() = SplitMix64::new(self.seed.unwrap_or_else(entropy));
    }

    fn next_u64(&self) -> u64 {
        self.stream.lock().next_u64()
    }
}

/// A request's handle on its imposter's stream: what every reader draws through. A `rand` RNG
/// (`rand::Rng` via `TryRng<Error = Infallible>`), cheap to clone; clones share the stream.
#[derive(Debug, Clone)]
pub struct RequestRng(Arc<ImposterRng>);

impl RequestRng {
    #[must_use]
    pub fn new(imposter: Arc<ImposterRng>) -> Self {
        Self(imposter)
    }

    /// A handle on a stream of its own, seeded from OS entropy: for a caller with no imposter.
    #[must_use]
    pub fn unseeded() -> Self {
        Self(Arc::new(ImposterRng::new(None)))
    }

    /// A `wasi:random/insecure-seed` value: two draws.
    pub fn seed_u128(&mut self) -> u128 {
        (u128::from(self.0.next_u64()) << 64) | u128::from(self.0.next_u64())
    }
}

impl rand::TryRng for RequestRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        // The high half: SplitMix64's best-mixed bits.
        Ok((self.0.next_u64() >> 32) as u32)
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        Ok(self.0.next_u64())
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        for chunk in dst.chunks_mut(8) {
            let bytes = self.0.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        Ok(())
    }
}

tokio::task_local! {
    /// The handle of the request this task serves.
    static REQUEST_RNG: RequestRng;
}

std::thread_local! {
    /// The same, for a blocking-pool thread running a request's work.
    static THREAD_RNG: std::cell::RefCell<Option<RequestRng>> = const { std::cell::RefCell::new(None) };
}

/// The handle of the request being served here, on its task or on a blocking thread running its
/// work; `None` outside a request.
#[must_use]
pub fn current() -> Option<RequestRng> {
    REQUEST_RNG
        .try_with(Clone::clone)
        .ok()
        .or_else(|| THREAD_RNG.with(|slot| slot.borrow().clone()))
}

/// Run `fut` with `rng` as its request's handle.
pub async fn with_request_rng<F: std::future::Future>(rng: RequestRng, fut: F) -> F::Output {
    REQUEST_RNG.scope(rng, fut).await
}

/// Run `f` on this thread with `rng` as [`current`], for a closure handed to the blocking pool.
/// The thread's previous handle comes back after, a panicking `f` included.
pub fn carry_request_rng<T>(rng: Option<RequestRng>, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<RequestRng>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            THREAD_RNG.with(|slot| *slot.borrow_mut() = previous);
        }
    }
    let _restore = Restore(THREAD_RNG.with(|slot| slot.replace(rng)));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{RngExt, TryRng};

    // Published SplitMix64 outputs for seed 0 (e.g. the reference C, `next()` from x = 0).
    #[test]
    fn splitmix64_matches_the_reference_vectors() {
        let mut s = SplitMix64::new(0);
        assert_eq!(s.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(s.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(s.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn a_seed_repeats_after_reseed_and_entropy_does_not() {
        let seeded = Arc::new(ImposterRng::new(Some(42)));
        let mut rng = RequestRng::new(Arc::clone(&seeded));
        let first: Vec<u64> = (0..4).map(|_| rng.try_next_u64().unwrap_or(0)).collect();
        seeded.reseed();
        let again: Vec<u64> = (0..4).map(|_| rng.try_next_u64().unwrap_or(0)).collect();
        assert_eq!(first, again);
        let mut other = RequestRng::new(Arc::new(ImposterRng::new(Some(43))));
        assert_ne!(first[0], other.try_next_u64().unwrap_or(0));
        // Unseeded: two fresh streams start apart (a 1-in-2^64 chance otherwise).
        let mut a = RequestRng::unseeded();
        let mut b = RequestRng::unseeded();
        assert_ne!(a.try_next_u64().ok(), b.try_next_u64().ok());
    }

    #[test]
    fn bytes_and_ranges_draw_from_the_stream() {
        let mut a = RequestRng::new(Arc::new(ImposterRng::new(Some(7))));
        let mut b = RequestRng::new(Arc::new(ImposterRng::new(Some(7))));
        let (mut x, mut y) = ([0u8; 13], [0u8; 13]);
        a.try_fill_bytes(&mut x).ok();
        b.try_fill_bytes(&mut y).ok();
        assert_eq!(x, y);
        assert_eq!(a.random_range(0..=1000), b.random_range(0..=1000));
        assert_eq!(a.seed_u128(), b.seed_u128());
    }

    #[test]
    fn a_seed_decodes_tolerantly() {
        let seed: Seed = serde_json::from_value(serde_json::json!(42)).expect("decodes");
        assert_eq!(seed.value(), Some(42));
        for odd in [
            serde_json::json!(-1),
            serde_json::json!("42"),
            serde_json::json!(1.5),
        ] {
            let seed: Seed = serde_json::from_value(odd.clone()).expect("decodes");
            assert_eq!(seed.value(), None, "{odd}");
            assert_eq!(
                serde_json::to_value(&seed).expect("encodes"),
                odd,
                "kept as written"
            );
        }
    }

    #[tokio::test]
    async fn the_handle_reaches_its_task_and_a_carried_thread_only() {
        assert!(current().is_none());
        let rng = RequestRng::new(Arc::new(ImposterRng::new(Some(1))));
        let seen = with_request_rng(rng, async {
            let carried = current();
            let on_thread = tokio::task::spawn_blocking(move || {
                carry_request_rng(carried, || current().is_some())
            })
            .await
            .expect("join");
            let uncarried = tokio::task::spawn_blocking(|| current().is_some())
                .await
                .expect("join");
            (current().is_some(), on_thread, uncarried)
        })
        .await;
        assert_eq!(seen, (true, true, false));
        assert!(current().is_none());
    }
}
