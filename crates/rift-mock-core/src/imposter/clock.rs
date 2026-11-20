//! An imposter's own clock (issue #1754): wall time until a test freezes or steps it.
//!
//! A test whose code runs on a fake clock (ZIO `TestClock`, a simulation's virtual time) can move
//! the mock's time with it: `PUT /imposters/{port}/clock` freezes it at an instant or steps it
//! forward, and with `_rift.scriptEngine.clock: "header"` one request can name its own instant in
//! `X-Rift-Now`. What reads it: the built-in flow store's TTLs, and a wasm component's
//! `wasi:clocks`. What deliberately does not: `Last-Modified` and `LoadClock` (HTTP cache semantics
//! are about real time), journal timestamps (observability), scenario transition stamps and proxy
//! latency. Rhai, JavaScript and templates read it from #1756.
//!
//! Runtime state, like script state: never persisted, wall again on restart and on recreate.

use arc_swap::ArcSwap;
use std::time::{Duration, SystemTime};

/// Where an imposter's time comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockMode {
    /// The system clock.
    Wall,
    /// Stopped at this instant, in milliseconds since the Unix epoch, until set again.
    Frozen { now_ms: i64 },
}

/// Why a clock change was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClockError {
    #[error("advanceMs must not be negative (got {0}); set nowMs to go back")]
    NegativeAdvance(i64),
    #[error("the clock cannot be advanced past the latest instant it holds")]
    Overflow,
    #[error("{0} ms is outside the instants the clock holds (years 1 to 9999)")]
    OutOfRange(i64),
}

/// The earliest instant a clock holds, in milliseconds since the Unix epoch: year 1. With
/// [`MAX_MS`] (year 9999), a range every platform's `SystemTime` can represent, where Windows'
/// cannot hold most of `i64` milliseconds.
pub const MIN_MS: i64 = -62_135_596_800_000;
/// The latest instant a clock holds: the end of year 9999.
pub const MAX_MS: i64 = 253_402_300_799_999;

fn in_range(ms: i64) -> Result<i64, ClockError> {
    if (MIN_MS..=MAX_MS).contains(&ms) {
        Ok(ms)
    } else {
        Err(ClockError::OutOfRange(ms))
    }
}

/// One imposter's clock.
#[derive(Debug)]
pub struct ImposterClock {
    mode: ArcSwap<ClockMode>,
}

impl Default for ImposterClock {
    fn default() -> Self {
        Self {
            mode: ArcSwap::from_pointee(ClockMode::Wall),
        }
    }
}

/// The system clock in milliseconds since the Unix epoch.
#[must_use]
pub fn wall_ms() -> i64 {
    let since = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
}

/// `ms` since the Unix epoch as a [`SystemTime`]; a negative instant is before the epoch. Every
/// instant a clock holds fits; one outside [`MIN_MS`]..=[`MAX_MS`] (never stored) reads as the
/// epoch rather than panicking.
#[must_use]
pub fn system_time_of(ms: i64) -> SystemTime {
    let magnitude = Duration::from_millis(ms.unsigned_abs());
    let at = if ms >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(magnitude)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(magnitude)
    };
    at.unwrap_or(SystemTime::UNIX_EPOCH)
}

impl ImposterClock {
    #[must_use]
    pub fn mode(&self) -> ClockMode {
        **self.mode.load()
    }

    /// The current instant, in milliseconds since the Unix epoch.
    #[must_use]
    pub fn now_ms(&self) -> i64 {
        match self.mode() {
            ClockMode::Wall => wall_ms(),
            ClockMode::Frozen { now_ms } => now_ms,
        }
    }

    /// Stop the clock at `now_ms`, which must be within [`MIN_MS`]..=[`MAX_MS`].
    pub fn freeze(&self, now_ms: i64) -> Result<ClockMode, ClockError> {
        let mode = ClockMode::Frozen {
            now_ms: in_range(now_ms)?,
        };
        self.mode.store(std::sync::Arc::new(mode));
        Ok(mode)
    }

    /// Step the clock forward by `ms`: a frozen clock moves by `ms`, a wall clock freezes at wall
    /// time plus `ms`. Atomic against a concurrent step: two advances add up.
    pub fn advance(&self, ms: i64) -> Result<ClockMode, ClockError> {
        if ms < 0 {
            return Err(ClockError::NegativeAdvance(ms));
        }
        let mut out = Err(ClockError::Overflow);
        self.mode.rcu(|current| {
            let base = match **current {
                ClockMode::Wall => wall_ms(),
                ClockMode::Frozen { now_ms } => now_ms,
            };
            match base.checked_add(ms).filter(|next| *next <= MAX_MS) {
                Some(now_ms) => {
                    out = Ok(ClockMode::Frozen { now_ms });
                    std::sync::Arc::new(ClockMode::Frozen { now_ms })
                }
                None => {
                    out = Err(ClockError::Overflow);
                    std::sync::Arc::clone(current)
                }
            }
        });
        out
    }

    /// Back to the system clock.
    pub fn reset(&self) {
        self.mode.store(std::sync::Arc::new(ClockMode::Wall));
    }
}

/// `_rift.scriptEngine.clock`: whether a request may name its own instant (issue #1754).
///
/// Decoded tolerantly: engines before #1754 admitted any value under this key and ignored it, so a
/// stored config carrying one must still replay (rift-cluster's log, `--datadir`). Any other string
/// decodes as [`Self::Unrecognized`], runs as `wall`, and is logged when the imposter is built.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ClockSource {
    /// The imposter's clock only; `X-Rift-Now` is ignored.
    #[default]
    Wall,
    /// A request's `X-Rift-Now: <ms since epoch>` is its own instant, ahead of the imposter's clock.
    Header,
    /// Any other string: runs as `wall`.
    #[serde(other)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unrecognized,
}

/// The header a request names its own instant in, under `clock: "header"`.
pub const NOW_HEADER: &str = "x-rift-now";

/// An `X-Rift-Now` that is not a whole number of milliseconds within the clock's range.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "X-Rift-Now must be milliseconds since the Unix epoch, an integer from {MIN_MS} to {MAX_MS} \
     (got {0:?})"
)]
pub struct BadNowHeader(pub String);

/// A request's own instant (issue #1754): its `X-Rift-Now` under `clock: "header"`, else `None`.
/// Resolved once, when the request is admitted. Everything else reads the imposter's clock live, so
/// a step taken while the request runs is seen; a request's own instant stays fixed.
pub fn resolve_request_now(
    source: ClockSource,
    header: Option<&str>,
) -> Result<Option<i64>, BadNowHeader> {
    match (source, header) {
        (ClockSource::Header, Some(raw)) => raw
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|ms| in_range(*ms).is_ok())
            .map(Some)
            .ok_or_else(|| BadNowHeader(raw.to_owned())),
        _ => Ok(None),
    }
}

tokio::task_local! {
    /// The instant the request this task serves named in `X-Rift-Now`, if it named one.
    static REQUEST_NOW: i64;
}

std::thread_local! {
    /// The same, for a blocking-pool thread running a request's work.
    static THREAD_NOW: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// The `X-Rift-Now` instant of the request being served here, on its task or on a blocking thread
/// running its work.
#[must_use]
pub fn request_override() -> Option<i64> {
    REQUEST_NOW
        .try_with(|ms| *ms)
        .ok()
        .or_else(|| THREAD_NOW.with(std::cell::Cell::get))
}

/// Run `fut` with `ms` as its request's instant; `None` runs it as it is.
pub async fn with_request_override<F: std::future::Future>(ms: Option<i64>, fut: F) -> F::Output {
    match ms {
        Some(ms) => REQUEST_NOW.scope(ms, fut).await,
        None => fut.await,
    }
}

/// Run `f` on this thread with the instant [`request_override`] reads on the calling task, for a
/// closure handed to the blocking pool.
pub fn carry_request_override<T>(ms: Option<i64>, f: impl FnOnce() -> T) -> T {
    /// Puts the thread's previous instant back, a panicking `f` included: the blocking pool reuses
    /// the thread for another request.
    struct Restore(Option<i64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            THREAD_NOW.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(THREAD_NOW.with(|slot| slot.replace(ms)));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freeze_advance_and_reset() {
        let clock = ImposterClock::default();
        assert_eq!(clock.mode(), ClockMode::Wall);
        let before = wall_ms();
        assert!(clock.now_ms() >= before);
        assert_eq!(
            clock.freeze(1_700_000_000_000),
            Ok(ClockMode::Frozen {
                now_ms: 1_700_000_000_000
            })
        );
        assert_eq!(clock.now_ms(), 1_700_000_000_000);
        assert_eq!(
            clock.advance(86_400_000),
            Ok(ClockMode::Frozen {
                now_ms: 1_700_086_400_000
            })
        );
        assert_eq!(clock.now_ms(), 1_700_086_400_000);
        assert_eq!(clock.advance(-1), Err(ClockError::NegativeAdvance(-1)));
        clock.freeze(MAX_MS - 1).expect("in range");
        assert_eq!(clock.advance(2), Err(ClockError::Overflow));
        assert_eq!(
            clock.now_ms(),
            MAX_MS - 1,
            "a refused advance changes nothing"
        );
        assert_eq!(
            clock.freeze(MAX_MS + 1),
            Err(ClockError::OutOfRange(MAX_MS + 1))
        );
        assert_eq!(
            clock.freeze(i64::MIN),
            Err(ClockError::OutOfRange(i64::MIN))
        );
        assert_eq!(
            clock.now_ms(),
            MAX_MS - 1,
            "a refused freeze changes nothing"
        );
        assert!(clock.freeze(MIN_MS).is_ok());
        clock.reset();
        assert_eq!(clock.mode(), ClockMode::Wall);
    }

    // On a wall clock, an advance freezes at wall time plus the step.
    #[test]
    fn advancing_a_wall_clock_freezes_it_ahead() {
        let clock = ImposterClock::default();
        let before = wall_ms();
        let ClockMode::Frozen { now_ms } = clock.advance(60_000).expect("advance") else {
            panic!("an advanced clock is frozen");
        };
        assert!(
            now_ms >= before + 60_000 && now_ms <= wall_ms() + 60_000,
            "{now_ms}"
        );
    }

    #[test]
    fn only_a_header_in_header_mode_is_a_request_instant() {
        assert_eq!(resolve_request_now(ClockSource::Header, None), Ok(None));
        assert_eq!(
            resolve_request_now(ClockSource::Header, Some("1700000000000")),
            Ok(Some(1_700_000_000_000))
        );
        // `clock: "wall"` ignores the header, malformed or not; so does an unrecognized mode.
        assert_eq!(resolve_request_now(ClockSource::Wall, Some("7")), Ok(None));
        assert_eq!(
            resolve_request_now(ClockSource::Wall, Some("soon")),
            Ok(None)
        );
        assert_eq!(
            resolve_request_now(ClockSource::Unrecognized, Some("7")),
            Ok(None)
        );
        for bad in ["soon", "1.5", "9223372036854775807", "-62135596800001"] {
            assert_eq!(
                resolve_request_now(ClockSource::Header, Some(bad)),
                Err(BadNowHeader(bad.into())),
                "{bad}"
            );
        }
    }

    // An unknown `clock` value decodes, so a stored config that carries one replays.
    #[test]
    fn an_unknown_clock_source_decodes_as_unrecognized() {
        let source: ClockSource =
            serde_json::from_value(serde_json::json!("virtual")).expect("decodes");
        assert_eq!(source, ClockSource::Unrecognized);
        let header: ClockSource =
            serde_json::from_value(serde_json::json!("header")).expect("decodes");
        assert_eq!(header, ClockSource::Header);
    }

    #[test]
    fn system_time_of_handles_both_sides_of_the_epoch() {
        assert_eq!(
            system_time_of(1_000)
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("after"),
            Duration::from_secs(1)
        );
        assert_eq!(
            SystemTime::UNIX_EPOCH
                .duration_since(system_time_of(-1_000))
                .expect("before"),
            Duration::from_secs(1)
        );
        // Out of every platform's range: the epoch, not a panic.
        let _ = system_time_of(i64::MAX);
        let _ = system_time_of(i64::MIN);
    }

    #[tokio::test]
    async fn a_request_override_reaches_its_task_and_a_carried_thread_only() {
        assert_eq!(request_override(), None);
        let seen = with_request_override(Some(42), async {
            let on_task = request_override();
            let carried = request_override();
            let on_thread = tokio::task::spawn_blocking(move || {
                carry_request_override(carried, request_override)
            })
            .await
            .expect("join");
            let uncarried = tokio::task::spawn_blocking(request_override)
                .await
                .expect("join");
            (on_task, on_thread, uncarried)
        })
        .await;
        assert_eq!(seen, (Some(42), Some(42), None));
        assert_eq!(request_override(), None);
    }

    // A panicking closure on a blocking thread does not leave its instant for the next job there.
    #[test]
    fn a_panicking_carry_restores_the_thread() {
        let caught = std::panic::catch_unwind(|| {
            carry_request_override(Some(9), || -> () { panic!("boom") })
        });
        assert!(caught.is_err());
        assert_eq!(request_override(), None);
    }
}
