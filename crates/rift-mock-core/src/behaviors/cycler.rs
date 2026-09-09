//! Response cycling state management.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// A lock-free atomic cycler that packs response index and repeat index into a single AtomicU64
#[derive(Default)]
pub struct RuleCycler(AtomicU64);

fn split(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

fn join(resp_idx: u32, repeat_idx: u32) -> u64 {
    (u64::from(resp_idx) << 32) | u64::from(repeat_idx)
}

fn advance(
    (mut resp_idx, mut repeat_idx): (u32, u32),
    response_count: u32,
    repeat_count: u32,
) -> (u32, u32) {
    repeat_idx = repeat_idx.saturating_add(1);
    if repeat_idx >= repeat_count {
        repeat_idx = 0;
        resp_idx += 1;
        if resp_idx >= response_count {
            resp_idx = 0;
        }
    }
    (resp_idx, repeat_idx)
}

impl RuleCycler {
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    #[must_use]
    pub fn peek_response_index(&self, response_count: u32) -> u32 {
        let value = self.0.load(Ordering::SeqCst);
        let (resp_idx, _repeat_idx) = split(value);
        resp_idx.min(response_count.saturating_sub(1))
    }

    pub fn reset(&self) {
        self.0.store(0, Ordering::SeqCst);
    }

    /// Get current response index for a rule, handling repeat behavior
    /// Returns the index to use for this request
    #[must_use]
    pub fn get_response_index_advance(
        &self,
        response_count: u32,
        mut repeat_for_response: impl FnMut(u32) -> Option<u32>,
    ) -> u32 {
        // SeqCst (not Relaxed): the advanced cursor must be *published* so a strictly-sequential
        // zero-latency next request — reached via a different tokio worker thread after the client
        // handoff — observes it, rather than serving a stale repeat branch (issue #565). Matches the
        // ordering the sibling per-imposter cross-request state already uses (`enabled`, journal
        // counts). The window is invisible under load-free / spawn transports but real on the
        // in-process embedded runtime on loaded runners.
        let old_value = self
            .0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                let (mut resp_idx, repeat_idx) = split(v);
                if resp_idx >= response_count {
                    resp_idx = response_count.saturating_sub(1);
                }
                let repeat_count = repeat_for_response(resp_idx).unwrap_or(1).max(1);
                let (resp_idx, repeat_idx) =
                    advance((resp_idx, repeat_idx), response_count, repeat_count);
                Some(join(resp_idx, repeat_idx))
            })
            .unwrap_or_else(|e| {
                debug_assert!(false, "we never return None from fetch_update");
                e
            });
        split(old_value).0
    }
}

impl fmt::Debug for RuleCycler {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (response_idx, repeat_idx) = split(self.0.load(Ordering::SeqCst));
        f.debug_struct("RuleCycler")
            .field("response_idx", &response_idx)
            .field("repeat_idx", &repeat_idx)
            .finish()
    }
}

/// Trait for types that can have a repeat behavior
pub trait HasRepeatBehavior {
    fn get_repeat(&self) -> Option<u32>;
}

#[cfg(test)]
mod rule_cycler_ordering_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;

    /// AC1 — durable regression tripwire. A pure memory-visibility fix can't be caught by a
    /// deterministic runtime test on a strong memory model, so gate the source instead: the cursor's
    /// atomic ops must stay on a published ordering (`SeqCst`) and never regress to the relaxed
    /// ordering that let a zero-latency next request observe a stale cursor (issue #565). The needle
    /// is assembled at runtime so this assertion never matches its own source.
    #[test]
    fn rule_cycler_stays_on_published_ordering() {
        let src = include_str!("cycler.rs");
        let regressed = format!("Ordering::{}", "Relaxed");
        assert!(
            !src.contains(&regressed),
            "RuleCycler atomic ops must use SeqCst (a published ordering), not the relaxed ordering — issue #565"
        );
    }

    /// AC2 — the exact cycling the flaky `/retry` fixture asserts: two responses where the first
    /// repeats twice and the second once, i.e. index sequence 0,0,1 then wrap (503,503,200,...).
    #[test]
    fn cycler_matches_retry_fixture_pattern() {
        let cycler = RuleCycler::new();
        let repeats = [2u32, 1u32];
        let repeat_for = |i: u32| repeats.get(i as usize).copied();
        let got: Vec<u32> = (0..6)
            .map(|_| cycler.get_response_index_advance(2, repeat_for))
            .collect();
        assert_eq!(
            got,
            vec![0, 0, 1, 0, 0, 1],
            "repeat:[2,1] must cycle 503,503,200 and wrap"
        );
    }

    /// AC3 — a strictly-sequential client hands the cursor from one worker thread to the next; the
    /// request that crosses a repeat boundary must observe the prior request's advance. Emulates
    /// the conformance replay (pull exactly one index, hand off, pull the next) with the pulls
    /// landing on alternating threads, and asserts the concatenated sequence is the exact
    /// deterministic cycling — no skipped or duplicated index across the hand-off.
    #[test]
    fn concurrent_strict_handoff_preserves_cycling() {
        // Two responses, first repeats twice: expected 0,0,1,0,0,1,... (the /retry shape).
        const ROUNDS: usize = 2_000;
        let cycler = Arc::new(RuleCycler::new());
        let (to_b, from_a) = mpsc::channel::<()>();
        let (to_a, from_b) = mpsc::channel::<u32>();

        let cycler_b = Arc::clone(&cycler);
        let handle = std::thread::spawn(move || {
            let repeats = [2u32, 1u32];
            let repeat_for = |i: u32| repeats.get(i as usize).copied();
            while from_a.recv().is_ok() {
                let idx = cycler_b.get_response_index_advance(2, repeat_for);
                if to_a.send(idx).is_err() {
                    break;
                }
            }
        });

        let repeats = [2u32, 1u32];
        let repeat_for = |i: u32| repeats.get(i as usize).copied();
        let mut seq = Vec::with_capacity(ROUNDS);
        for round in 0..ROUNDS {
            // Even rounds pulled on this (main) thread, odd rounds on the worker — so consecutive
            // pulls alternate threads, exercising the cross-thread hand-off on every boundary.
            if round % 2 == 0 {
                seq.push(cycler.get_response_index_advance(2, repeat_for));
            } else {
                to_b.send(()).expect("worker alive");
                seq.push(from_b.recv().expect("worker replied"));
            }
        }
        drop(to_b);
        handle.join().expect("worker joined");

        let expected: Vec<u32> = (0..ROUNDS).map(|i| [0, 0, 1][i % 3]).collect();
        assert_eq!(
            seq, expected,
            "cross-thread strict hand-off must observe every prior advance (no stale cursor)"
        );
    }
}
