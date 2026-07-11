//! Pluggable response sequencing (issue #313): the trait extracted from the per-stub
//! `RuleCycler` internals, so embedders can supply cursors that persist, seed
//! deterministically, or are shared between engine instances.
//!
//! With no sequencer registered, imposters keep today's embedded per-stub cycler — the
//! only addition on that path is one branch on the absent `Option`; no key building,
//! repeat materialization, or map lookups happen. A sequencer
//! registered via
//! [`ImposterManager::with_sequencer`](crate::imposter::ImposterManager::with_sequencer)
//! receives every cursor decision with a full [`SequenceKey`] and materialized repeats.

use super::cycler::RuleCycler;
use anyhow::Result;
use parking_lot::RwLock;
use std::collections::HashMap;

/// Identity of one cursor decision.
#[derive(Debug, Clone, Copy)]
pub struct SequenceKey<'a> {
    pub port: u16,
    /// Per-imposter slot token, minted when a stub is inserted and kept across in-place
    /// replaces (mirrors the internal `StubState` slot lifetime).
    pub slot: u64,
    /// Process-independent stub identity: the explicit stub `id`, else the stable content
    /// key (#316) without an occurrence suffix — byte-identical id-less siblings share it
    /// (and therefore share a cursor under backends keying by `stub_key`).
    pub stub_key: &'a str,
    /// The stub's isolation scope (`space`, #223), or `""` for global stubs.
    pub scope: &'a str,
}

/// Pluggable response-cursor backend. Built-ins never fail; `Err` means the backend is
/// unavailable and surfaces as a structured backend error (#318).
///
/// Contract: `next` and `peek` must return an index `< response_count`; a violation is
/// treated as a backend error (500), never silently served. `peek` may be called several
/// times per request (response-type dispatch peeks before the single advancing `next`) —
/// remote backends should expect a few round-trips per request.
pub trait ResponseSequencer: Send + Sync {
    /// Atomically advance and return the response index, honoring per-response repeats.
    fn next(&self, key: SequenceKey<'_>, response_count: usize, repeats: &[u32]) -> Result<usize>;
    /// Return the upcoming response index without advancing.
    fn peek(&self, key: SequenceKey<'_>, response_count: usize, repeats: &[u32]) -> Result<usize>;
    /// Reset cursors: one stub's (`Some(stub_key)`), or every cursor on the port (`None`).
    /// Also the GC hook — called on stub delete, bulk stub replace, and imposter teardown.
    fn reset_scope(&self, port: u16, stub_key: Option<&str>);
}

/// Reference sequencer with the exact semantics of the embedded per-stub cycler: the same
/// `RuleCycler` packing, keyed by `(port, slot, scope)`. The `stub_key` is recorded per
/// cursor so `reset_scope(port, Some(key))` works even though slots are the primary key.
#[derive(Default)]
pub struct LocalSequencer {
    #[allow(clippy::type_complexity)]
    cursors: RwLock<HashMap<(u16, u64, String), (RuleCycler, String)>>,
}

impl LocalSequencer {
    fn with_cursor<R>(&self, key: &SequenceKey<'_>, f: impl Fn(&RuleCycler) -> R) -> R {
        let map_key = (key.port, key.slot, key.scope.to_string());
        {
            let cursors = self.cursors.read();
            if let Some((cycler, _)) = cursors.get(&map_key) {
                return f(cycler);
            }
        }
        let mut cursors = self.cursors.write();
        let (cycler, _) = cursors
            .entry(map_key)
            .or_insert_with(|| (RuleCycler::new(), key.stub_key.to_string()));
        f(cycler)
    }
}

impl ResponseSequencer for LocalSequencer {
    fn next(&self, key: SequenceKey<'_>, response_count: usize, repeats: &[u32]) -> Result<usize> {
        if response_count == 0 {
            return Ok(0);
        }
        // Clamp: right after `responses` shrinks, the cycler's advance can return the
        // stale pre-clamp index once; clamping keeps the trait's `< response_count`
        // contract instead of surfacing a one-off backend error.
        Ok(self.with_cursor(&key, |cycler| {
            (cycler.get_response_index_advance(response_count as u32, |i| {
                repeats.get(i as usize).copied()
            }) as usize)
                .min(response_count - 1)
        }))
    }

    fn peek(&self, key: SequenceKey<'_>, response_count: usize, _repeats: &[u32]) -> Result<usize> {
        if response_count == 0 {
            return Ok(0);
        }
        let map_key = (key.port, key.slot, key.scope.to_string());
        Ok(self.cursors.read().get(&map_key).map_or(0, |(cycler, _)| {
            cycler.peek_response_index(response_count as u32) as usize
        }))
    }

    fn reset_scope(&self, port: u16, stub_key: Option<&str>) {
        let mut cursors = self.cursors.write();
        match stub_key {
            None => cursors.retain(|(p, _, _), _| *p != port),
            Some(key) => cursors.retain(|(p, _, _), (_, sk)| *p != port || sk != key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key<'a>(slot: u64, stub_key: &'a str, scope: &'a str) -> SequenceKey<'a> {
        SequenceKey {
            port: 4919,
            slot,
            stub_key,
            scope,
        }
    }

    // AC1: plain sequences advance and wrap like the embedded cycler.
    #[test]
    fn local_sequences_and_wraps() {
        let seq = LocalSequencer::default();
        let repeats = [1, 1, 1];
        for expected in [0, 1, 2, 0, 1] {
            assert_eq!(
                seq.next(key(1, "s", ""), 3, &repeats).expect("infallible"),
                expected
            );
        }
    }

    // AC1: per-response repeat counts hold each index for `repeat` pulls.
    #[test]
    fn local_honors_per_response_repeats() {
        let seq = LocalSequencer::default();
        let repeats = [2, 3];
        let got: Vec<usize> = (0..7)
            .map(|_| seq.next(key(1, "s", ""), 2, &repeats).expect("infallible"))
            .collect();
        assert_eq!(got, vec![0, 0, 1, 1, 1, 0, 0]);
    }

    // AC1: peek returns the upcoming index without advancing.
    #[test]
    fn local_peek_does_not_advance() {
        let seq = LocalSequencer::default();
        let repeats = [1, 1];
        assert_eq!(seq.peek(key(1, "s", ""), 2, &repeats).expect("ok"), 0);
        assert_eq!(seq.peek(key(1, "s", ""), 2, &repeats).expect("ok"), 0);
        assert_eq!(seq.next(key(1, "s", ""), 2, &repeats).expect("ok"), 0);
        assert_eq!(seq.peek(key(1, "s", ""), 2, &repeats).expect("ok"), 1);
    }

    // Scopes (issue #223 spaces) cycle independently for the same slot.
    #[test]
    fn local_scopes_are_isolated() {
        let seq = LocalSequencer::default();
        let repeats = [1, 1, 1];
        assert_eq!(seq.next(key(1, "s", "a"), 3, &repeats).expect("ok"), 0);
        assert_eq!(seq.next(key(1, "s", "a"), 3, &repeats).expect("ok"), 1);
        assert_eq!(
            seq.next(key(1, "s", "b"), 3, &repeats).expect("ok"),
            0,
            "a different scope starts fresh"
        );
    }

    // AC1: reset by stub key resets only that stub's cursors; reset by port drops all.
    #[test]
    fn local_reset_scope_by_key_and_port() {
        let seq = LocalSequencer::default();
        let repeats = [1, 1, 1];
        let _ = seq.next(key(1, "alpha", ""), 3, &repeats);
        let _ = seq.next(key(2, "beta", ""), 3, &repeats);

        seq.reset_scope(4919, Some("alpha"));
        assert_eq!(
            seq.next(key(1, "alpha", ""), 3, &repeats).expect("ok"),
            0,
            "alpha restarted"
        );
        assert_eq!(
            seq.next(key(2, "beta", ""), 3, &repeats).expect("ok"),
            1,
            "beta untouched"
        );

        seq.reset_scope(4919, None);
        assert_eq!(
            seq.next(key(2, "beta", ""), 3, &repeats).expect("ok"),
            0,
            "port-wide reset drops every cursor"
        );
    }

    // After the response list shrinks, the next index stays in range (the trait
    // contract), rather than leaking the cycler's stale pre-clamp value.
    #[test]
    fn local_clamps_after_shrink() {
        let seq = LocalSequencer::default();
        let repeats3 = [1, 1, 1];
        let _ = seq.next(key(1, "s", ""), 3, &repeats3);
        let _ = seq.next(key(1, "s", ""), 3, &repeats3);
        let idx = seq.next(key(1, "s", ""), 1, &[1]).expect("infallible");
        assert!(idx < 1, "index {idx} out of range after shrink to 1");
    }

    // Different ports never share cursors even with identical slots/keys.
    #[test]
    fn local_ports_are_isolated() {
        let seq = LocalSequencer::default();
        let repeats = [1, 1];
        let a = SequenceKey {
            port: 1000,
            slot: 1,
            stub_key: "s",
            scope: "",
        };
        let b = SequenceKey {
            port: 2000,
            slot: 1,
            stub_key: "s",
            scope: "",
        };
        assert_eq!(seq.next(a, 2, &repeats).expect("ok"), 0);
        assert_eq!(seq.next(b, 2, &repeats).expect("ok"), 0, "fresh per port");
    }
}
