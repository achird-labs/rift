//! Response cycling state management.

use parking_lot::{RwLock, RwLockWriteGuard};
use std::collections::HashMap;
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
        let value = self.0.load(Ordering::Relaxed);
        let (resp_idx, _repeat_idx) = split(value);
        resp_idx.min(response_count.saturating_sub(1))
    }

    pub fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    /// Get current response index for a rule, handling repeat behavior
    /// Returns the index to use for this request
    #[must_use]
    pub fn get_response_index_advance(
        &self,
        response_count: u32,
        mut repeat_for_response: impl FnMut(u32) -> Option<u32>,
    ) -> u32 {
        let old_value = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
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
        let (response_idx, repeat_idx) = split(self.0.load(Ordering::Relaxed));
        f.debug_struct("RuleCycler")
            .field("response_idx", &response_idx)
            .field("repeat_idx", &repeat_idx)
            .finish()
    }
}

/// Combined state for all rules - protected by a single lock to prevent deadlocks
#[derive(Default)]
struct CyclerState {
    rules: HashMap<String, RuleCycler>,
}

/// Tracks response cycling state per rule
///
/// Uses a single lock to protect all state, avoiding the deadlock that could occur
/// with multiple locks acquired in inconsistent order.
pub struct ResponseCycler {
    state: RwLock<CyclerState>,
}

impl Default for ResponseCycler {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseCycler {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(CyclerState::default()),
        }
    }

    /// Get current response index for a rule, handling repeat behavior
    /// Returns the index to use for this request
    pub fn get_response_index(
        &self,
        rule_id: &str,
        response_count: usize,
        repeat: Option<u32>,
    ) -> usize {
        if response_count == 0 {
            return 0;
        }

        let mut state = self.state.read();
        // Opportunistically attempt to use just a read lock. If the rule doesn't exist yet,
        // lock for writing, then downgrade
        let rule_state = if let Some(rule) = state.rules.get(rule_id) {
            rule
        } else {
            drop(state);
            let mut write = self.state.write();
            write.rules.entry(rule_id.to_string()).or_default();
            state = RwLockWriteGuard::downgrade(write);
            state
                .rules
                .get(rule_id)
                .expect("We atomically downgraded the lock, the rule we just inserted must exist")
        };
        rule_state.get_response_index_advance(response_count as u32, |_| repeat) as usize
    }

    /// Reset cycling state for a rule
    pub fn reset(&self, rule_id: &str) {
        let state = self.state.read();
        if let Some(rule_state) = state.rules.get(rule_id) {
            rule_state.reset();
        }
    }

    /// Reset all cycling state
    pub fn reset_all(&self) {
        self.state.write().rules.clear();
    }

    /// Peek at current response index without modifying state
    /// Used to check response type before committing to cycling
    pub fn peek_response_index(&self, rule_id: &str, response_count: usize) -> usize {
        if response_count == 0 {
            return 0;
        }

        let state = self.state.read();
        state
            .rules
            .get(rule_id)
            .map_or(0, |r| r.peek_response_index(response_count as u32) as usize)
    }

    /// Advance the cycler for a proxy response (which has no repeat behavior)
    /// This should be called after successfully handling a proxy response
    pub fn advance_for_proxy(&self, rule_id: &str, response_count: usize) {
        if response_count == 0 {
            return;
        }

        let mut state = self.state.read();
        let rule_state = if let Some(rule) = state.rules.get(rule_id) {
            rule
        } else {
            drop(state);
            let mut write = self.state.write();
            write.rules.entry(rule_id.to_string()).or_default();
            state = RwLockWriteGuard::downgrade(write);
            state
                .rules
                .get(rule_id)
                .expect("We atomically downgraded the lock, the rule we just inserted must exist")
        };
        _ = rule_state.get_response_index_advance(response_count as u32, |_| None);
    }

    /// Get response index with per-response repeat values
    /// Each response can have its own repeat count via _behaviors.repeat
    pub fn get_response_index_with_per_response_repeat<T: HasRepeatBehavior>(
        &self,
        rule_id: &str,
        responses: &[T],
    ) -> usize {
        if responses.is_empty() {
            return 0;
        }

        let mut state = self.state.read();
        let rule_state = if let Some(rule) = state.rules.get(rule_id) {
            rule
        } else {
            drop(state);
            let mut write = self.state.write();
            write.rules.entry(rule_id.to_string()).or_default();
            state = RwLockWriteGuard::downgrade(write);
            state
                .rules
                .get(rule_id)
                .expect("We atomically downgraded the lock, the rule we just inserted must exist")
        };
        rule_state.get_response_index_advance(responses.len() as u32, |i| {
            responses.get(i as usize).and_then(|resp| resp.get_repeat())
        }) as usize
    }
}

/// Trait for types that can have a repeat behavior
pub trait HasRepeatBehavior {
    fn get_repeat(&self) -> Option<u32>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_cycler_basic() {
        let cycler = ResponseCycler::new();

        // With 3 responses, no repeat
        assert_eq!(cycler.get_response_index("rule1", 3, None), 0);
        assert_eq!(cycler.get_response_index("rule1", 3, None), 1);
        assert_eq!(cycler.get_response_index("rule1", 3, None), 2);
        assert_eq!(cycler.get_response_index("rule1", 3, None), 0); // Wrap around
    }

    #[test]
    fn test_response_cycler_with_repeat() {
        let cycler = ResponseCycler::new();

        // With 2 responses, repeat=3
        assert_eq!(cycler.get_response_index("rule1", 2, Some(3)), 0);
        assert_eq!(cycler.get_response_index("rule1", 2, Some(3)), 0);
        assert_eq!(cycler.get_response_index("rule1", 2, Some(3)), 0);
        assert_eq!(cycler.get_response_index("rule1", 2, Some(3)), 1); // Advance after 3 repeats
        assert_eq!(cycler.get_response_index("rule1", 2, Some(3)), 1);
        assert_eq!(cycler.get_response_index("rule1", 2, Some(3)), 1);
        assert_eq!(cycler.get_response_index("rule1", 2, Some(3)), 0); // Wrap around
    }

    #[test]
    fn test_response_cycler_independent_rules() {
        let cycler = ResponseCycler::new();

        // Different rules should have independent state
        assert_eq!(cycler.get_response_index("rule1", 3, None), 0);
        assert_eq!(cycler.get_response_index("rule2", 3, None), 0);
        assert_eq!(cycler.get_response_index("rule1", 3, None), 1);
        assert_eq!(cycler.get_response_index("rule2", 3, None), 1);
    }

    #[test]
    fn test_response_cycler_peek() {
        let cycler = ResponseCycler::new();

        // Peek should not modify state
        assert_eq!(cycler.peek_response_index("rule1", 3), 0);
        assert_eq!(cycler.peek_response_index("rule1", 3), 0);

        // After actual get, peek should reflect new state
        cycler.get_response_index("rule1", 3, None);
        assert_eq!(cycler.peek_response_index("rule1", 3), 1);
    }

    #[test]
    fn test_response_cycler_reset() {
        let cycler = ResponseCycler::new();

        cycler.get_response_index("rule1", 3, None);
        cycler.get_response_index("rule1", 3, None);
        assert_eq!(cycler.peek_response_index("rule1", 3), 2);

        cycler.reset("rule1");
        assert_eq!(cycler.peek_response_index("rule1", 3), 0);
    }

    #[test]
    fn test_response_cycler_advance_for_proxy() {
        let cycler = ResponseCycler::new();

        assert_eq!(cycler.peek_response_index("rule1", 3), 0);
        cycler.advance_for_proxy("rule1", 3);
        assert_eq!(cycler.peek_response_index("rule1", 3), 1);
        cycler.advance_for_proxy("rule1", 3);
        assert_eq!(cycler.peek_response_index("rule1", 3), 2);
        cycler.advance_for_proxy("rule1", 3);
        assert_eq!(cycler.peek_response_index("rule1", 3), 0); // Wrap around
    }

    #[test]
    fn test_response_cycler_zero_responses() {
        let cycler = ResponseCycler::new();

        // Should handle zero responses gracefully
        assert_eq!(cycler.get_response_index("rule1", 0, None), 0);
        assert_eq!(cycler.peek_response_index("rule1", 0), 0);
    }

    struct MockResponse {
        repeat: Option<u32>,
    }

    impl HasRepeatBehavior for MockResponse {
        fn get_repeat(&self) -> Option<u32> {
            self.repeat
        }
    }

    #[test]
    fn test_per_response_repeat() {
        let cycler = ResponseCycler::new();

        // First response repeats 2x, second repeats 3x
        let responses = vec![
            MockResponse { repeat: Some(2) },
            MockResponse { repeat: Some(3) },
        ];

        // First response, repeat 2x
        assert_eq!(
            cycler.get_response_index_with_per_response_repeat("rule1", &responses),
            0
        );
        assert_eq!(
            cycler.get_response_index_with_per_response_repeat("rule1", &responses),
            0
        );

        // Second response, repeat 3x
        assert_eq!(
            cycler.get_response_index_with_per_response_repeat("rule1", &responses),
            1
        );
        assert_eq!(
            cycler.get_response_index_with_per_response_repeat("rule1", &responses),
            1
        );
        assert_eq!(
            cycler.get_response_index_with_per_response_repeat("rule1", &responses),
            1
        );

        // Back to first response
        assert_eq!(
            cycler.get_response_index_with_per_response_repeat("rule1", &responses),
            0
        );
    }
}
