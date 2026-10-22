//! Seed, case count, time budget and pacing, all from the environment.
//!
//! The pacing cap is about ephemeral ports, not Mountebank's speed: the harness client never pools
//! connections (`engine::client`), so every step opens about 2.6 of them (both engines, plus the
//! proxy hop), and each leaves a `TIME_WAIT` socket behind. 150 steps/s is about 390 connections/s,
//! under Linux's sustained ceiling of about 470/s (28k ports, 60 s `TIME_WAIT`).

use std::time::{Duration, Instant};

/// The seed a run uses when `RIFT_GEN_SEED` is unset: fixed, so every PR runs the same cases.
pub const DEFAULT_SEED: u64 = 1549;
pub const DEFAULT_CASES: u32 = 32;
pub const DEFAULT_MAX_STEPS_PER_SEC: u32 = 150;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    /// `RIFT_GEN_SEED`: the first batch's seed; batch `n` uses `seed + n`.
    pub seed: u64,
    /// `RIFT_GEN_CASES`: cases per batch.
    pub cases: u32,
    /// `RIFT_GEN_SECONDS`: run batches until this much wall-clock time has passed, instead of
    /// one batch.
    pub seconds: Option<u64>,
    /// `RIFT_GEN_MAX_STEPS_PER_SEC`: the pacing cap.
    pub max_steps_per_sec: u32,
}

impl Budget {
    /// Reads the budget through `var` (the environment, in the test).
    ///
    /// # Errors
    /// A variable that is set but is not a positive integer: a typo must not quietly run the
    /// default budget.
    pub fn from(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        fn parse<T: std::str::FromStr + PartialEq + Default>(
            name: &str,
            value: Option<String>,
        ) -> Result<Option<T>, String> {
            match value {
                None => Ok(None),
                Some(text) => match text.trim().parse::<T>() {
                    Ok(n) if n != T::default() => Ok(Some(n)),
                    _ => Err(format!("{name}={text:?} is not a positive integer")),
                },
            }
        }
        Ok(Self {
            seed: match var("RIFT_GEN_SEED") {
                None => DEFAULT_SEED,
                Some(text) => text
                    .trim()
                    .parse()
                    .map_err(|_| format!("RIFT_GEN_SEED={text:?} is not an integer"))?,
            },
            cases: parse("RIFT_GEN_CASES", var("RIFT_GEN_CASES"))?.unwrap_or(DEFAULT_CASES),
            seconds: parse("RIFT_GEN_SECONDS", var("RIFT_GEN_SECONDS"))?,
            max_steps_per_sec: parse(
                "RIFT_GEN_MAX_STEPS_PER_SEC",
                var("RIFT_GEN_MAX_STEPS_PER_SEC"),
            )?
            .unwrap_or(DEFAULT_MAX_STEPS_PER_SEC),
        })
    }
}

/// Holds the average step rate under the cap: after each case it sleeps until the steps sent so
/// far fit the elapsed time.
#[derive(Debug)]
pub struct Pacer {
    started: Instant,
    steps: u64,
    per_sec: u32,
}

impl Pacer {
    #[must_use]
    pub fn new(per_sec: u32) -> Self {
        Self {
            started: Instant::now(),
            steps: 0,
            per_sec,
        }
    }

    /// Counts `steps` more and returns how long to wait before the next case.
    pub fn record(&mut self, steps: usize) -> Duration {
        self.steps += steps as u64;
        let due = Duration::from_secs_f64(self.steps as f64 / f64::from(self.per_sec));
        due.saturating_sub(self.started.elapsed())
    }

    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }
}
