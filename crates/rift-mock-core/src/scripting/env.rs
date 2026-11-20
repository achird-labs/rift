//! What a script reads as the time and as randomness (issue #1756): Rhai's `timestamp_header()`,
//! the template `now`, JavaScript's `Date` and `Math.random()`.
//!
//! Each reads the run's [`ScriptEnv`]: its request's `X-Rift-Now`, else its imposter's clock (frozen
//! or wall, read live, #1754), and its request's random stream (#1755). On the request task and on a
//! blocking thread running the request's work those come from the request's scope; on an MB script
//! pool worker, where runs of several imposters interleave on one realm, each run carries the env it
//! was submitted with and installs it whenever it resumes. Outside any request: wall time and the
//! thread's own randomness, as before.
//!
//! Timers (`setTimeout`, a guest's sleep) keep real time: they schedule work, they do not report
//! the time.
//!
//! A JavaScript `wait` function reads them too: its `Math.random()` draws from the imposter's
//! stream, so it takes its place in a seeded sequence. A promise continuation that one run leaves
//! to a later run of the same imposter (an MB realm's bequeathed jobs) runs under that later run's
//! env, as it runs inside that run.

use crate::imposter::clock::{self, wall_ms};
use crate::imposter::rng::{self, RequestRng};
use crate::scripting::ScriptClock;
use std::cell::RefCell;

/// A run's time source and random stream.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScriptEnv {
    clock: Option<ScriptClock>,
    /// Read by JavaScript's `Math.random()`; Rhai and templates have no random reader of their own.
    #[cfg_attr(not(feature = "javascript"), allow(dead_code))]
    rng: Option<RequestRng>,
}

std::thread_local! {
    /// The env of the run executing on this thread, when one has been installed.
    static ENV: RefCell<Option<ScriptEnv>> = const { RefCell::new(None) };
}

impl ScriptEnv {
    /// The env of the work running here: the run's installed env, else the request's (its
    /// `X-Rift-Now`, else its imposter's clock; its random stream), else nothing (wall time, the
    /// thread's randomness).
    pub(crate) fn capture() -> Self {
        if let Some(env) = ENV.with(|e| e.borrow().clone()) {
            return env;
        }
        let clock = match clock::request_override() {
            Some(ms) => Some(ScriptClock::Fixed(ms)),
            None => clock::current_imposter_clock().map(ScriptClock::Imposter),
        };
        Self {
            clock,
            rng: rng::current(),
        }
    }

    /// Install this env on the thread until the guard drops, which puts back the one before (a
    /// panic included).
    #[cfg_attr(not(feature = "javascript"), allow(dead_code))]
    pub(crate) fn install(self) -> EnvGuard {
        EnvGuard(Some(ENV.with(|e| e.replace(Some(self)))))
    }

    /// Run `f` with this env installed.
    #[cfg_attr(not(feature = "javascript"), allow(dead_code))]
    pub(crate) fn enter<T>(self, f: impl FnOnce() -> T) -> T {
        let _guard = self.install();
        f()
    }
}

/// Puts the previous env back when dropped.
#[derive(Debug)]
#[cfg_attr(not(feature = "javascript"), allow(dead_code))]
pub(crate) struct EnvGuard(Option<Option<ScriptEnv>>);

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            ENV.with(|e| *e.borrow_mut() = previous);
        }
    }
}

/// The time a script reads, in milliseconds since the Unix epoch. Reads the installed env in
/// place: this runs on every `Date.now()`.
#[must_use]
pub(crate) fn now_ms() -> i64 {
    let installed = ENV.with(|e| {
        e.borrow()
            .as_ref()
            .map(|env| env.clock.as_ref().and_then(ScriptClock::frozen_ms))
    });
    let frozen = match installed {
        Some(frozen) => frozen,
        None => match clock::request_override() {
            Some(ms) => Some(ms),
            None => clock::current_imposter_clock().and_then(|clock| match clock.mode() {
                clock::ClockMode::Wall => None,
                clock::ClockMode::Frozen { now_ms } => Some(now_ms),
            }),
        },
    };
    frozen.unwrap_or_else(wall_ms)
}

/// [`now_ms`] as a UTC date-time; an instant chrono cannot hold reads as the wall clock.
#[must_use]
pub(crate) fn now_utc() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_millis(now_ms()).unwrap_or_else(chrono::Utc::now)
}

/// A random number in `[0, 1)`, from the run's stream when it has one.
#[must_use]
#[cfg_attr(not(feature = "javascript"), allow(dead_code))]
pub(crate) fn random_f64() -> f64 {
    use rand::RngExt;
    // The handle is an `Arc` clone, not the whole env: this runs on every `Math.random()`.
    let installed = ENV.with(|e| e.borrow().as_ref().map(|env| env.rng.clone()));
    match installed.unwrap_or_else(rng::current) {
        Some(mut rng) => rng.random::<f64>(),
        None => rand::random::<f64>(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::clock::ImposterClock;
    use crate::imposter::rng::ImposterRng;
    use std::sync::Arc;

    #[test]
    fn an_installed_env_is_read_and_put_back() {
        let wall = now_ms();
        assert!((wall - wall_ms()).abs() < 5_000);
        let clock = Arc::new(ImposterClock::default());
        clock.freeze(1_700_000_000_000).expect("freeze");
        let env = ScriptEnv {
            clock: Some(ScriptClock::Imposter(Arc::clone(&clock))),
            rng: Some(RequestRng::new(Arc::new(ImposterRng::new(Some(1))))),
        };
        let (at, a, b) = env.clone().enter(|| (now_ms(), random_f64(), random_f64()));
        assert_eq!(at, 1_700_000_000_000);
        assert_ne!(a, b);
        // The same seed from the start draws the same numbers.
        let again = ScriptEnv {
            clock: None,
            rng: Some(RequestRng::new(Arc::new(ImposterRng::new(Some(1))))),
        };
        assert_eq!(again.enter(|| (random_f64(), random_f64())), (a, b));
        assert!((now_ms() - wall_ms()).abs() < 5_000, "put back after");
    }
}
