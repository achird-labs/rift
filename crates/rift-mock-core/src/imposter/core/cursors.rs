//! Each stub's response-cycle position as `--datadir` keeps it in `{port}.cursors` (issue #1603).
//!
//! Mountebank's datadir is its live store: every served response rewrites the stub's `meta.json`
//! with its `nextIndex`, so a restart resumes each cycle where it stopped. Rift keeps the position
//! in memory and writes it behind, from the manager's flusher and on a graceful stop.
//!
//! The file is runtime state, not config. It is never part of `ImposterConfig` (so stored and
//! replicated config bytes, `GET /imposters` and `?replayable=true` are untouched), and a file that
//! cannot be read is ignored with a warning rather than refusing the imposter: starting a cycle over
//! is better than not serving it.
//!
//! Entries are keyed by [`stub_key`](crate::imposter::reconcile::stub_key) (the stub's `id`, else
//! its content hash and occurrence), never by position: a stub inserted ahead, or a `{port}.json`
//! edited by hand, must not hand a cursor to a different stub. An entry whose key matches no stub
//! is dropped. Versioned so it can grow (the recorded requests of #1608 will sit beside it).

use super::{Imposter, StubState};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// The only version this engine writes and reads.
pub(crate) const CURSOR_FILE_VERSION: u32 = 1;

/// `{port}.cursors`: `{"version":1,"stubs":[{"key":"~1a2b…#0","response":1,"repeat":0}]}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CursorFile {
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) stubs: Vec<CursorEntry>,
}

/// One stub's position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CursorEntry {
    /// The stub's key, as [`stub_key`](crate::imposter::reconcile::stub_key) computes it.
    pub(crate) key: String,
    /// The response index, or the position in `cycle` when there is one.
    pub(crate) response: u32,
    /// How many times that response has been served in its `repeat` run.
    pub(crate) repeat: u32,
    /// The stub's mid-cycle order after a `proxyAlways` append (issue #1513), when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cycle: Option<Vec<u32>>,
}

impl CursorFile {
    /// Parse a `{port}.cursors` document. A version this engine does not know is an error, so a
    /// newer engine's file is ignored rather than misread.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, String> {
        let file: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if file.version != CURSOR_FILE_VERSION {
            return Err(format!(
                "unsupported version {} (this engine reads version {CURSOR_FILE_VERSION})",
                file.version
            ));
        }
        Ok(file)
    }

    /// Whether there is nothing to resume: every stub is at the start of its cycle.
    pub(crate) fn is_empty(&self) -> bool {
        self.stubs.is_empty()
    }
}

fn keys(stubs: &[Arc<StubState>]) -> Vec<String> {
    crate::imposter::reconcile::keys_from(
        stubs
            .iter()
            .map(|state| (state.stub.id.as_deref(), state.content_hash())),
    )
}

impl StubState {
    /// This state's position, or `None` at the start of a display-order cycle (nothing to resume).
    fn cursor_entry(&self, key: String) -> Option<CursorEntry> {
        let (response, repeat) = self.cursor();
        if (response, repeat) == (0, 0) && self.cycle.is_none() {
            return None;
        }
        Some(CursorEntry {
            key,
            response,
            repeat,
            cycle: self.cycle.as_deref().map(<[u32]>::to_vec),
        })
    }

    /// This state resumed at `entry`, or `None` when the entry cannot describe this stub (a cycle
    /// naming a response it does not have, or a position past its end): a stale or hand-edited
    /// entry restarts the cycle instead of serving out of range.
    fn resumed(&self, entry: &CursorEntry) -> Option<Self> {
        let responses = self.stub.responses.len();
        let len = match &entry.cycle {
            Some(cycle) => {
                if cycle.is_empty() || cycle.iter().any(|&i| i as usize >= responses) {
                    return None;
                }
                cycle.len()
            }
            None => responses,
        };
        if entry.response as usize >= len {
            return None;
        }
        let state = self.with_fresh_cycle();
        state.cycler.set_position(entry.response, entry.repeat);
        Some(Self {
            cycle: entry.cycle.as_deref().map(Into::into),
            ..state
        })
    }
}

impl Imposter {
    /// Note that a response cycle may have moved, so the manager's next flush writes it. Read
    /// first: an imposter already marked pays a load, not a contended store, per response.
    pub(crate) fn mark_cursors_dirty(&self) {
        if !self.cursors_dirty.load(Ordering::SeqCst) {
            self.cursors_dirty.store(true, Ordering::SeqCst);
        }
    }

    /// Whether a cycle moved since the last call, clearing the mark. `SeqCst` with the cycler's own
    /// `SeqCst` advance, so a snapshot taken after this sees every advance whose mark it cleared.
    pub(crate) fn take_cursors_dirty(&self) -> bool {
        self.cursors_dirty.swap(false, Ordering::SeqCst)
    }

    /// Every stub's position, in stub order, leaving out stubs at the start of their cycle.
    pub(crate) fn cursor_snapshot(&self) -> CursorFile {
        let snapshot = self.snapshot();
        let stubs = snapshot.stubs();
        CursorFile {
            version: CURSOR_FILE_VERSION,
            stubs: stubs
                .iter()
                .zip(keys(stubs))
                .filter_map(|(state, key)| state.cursor_entry(key))
                .collect(),
        }
    }

    /// Resume each stub whose key `file` names; return how many did. An entry naming no stub, or
    /// one that does not fit its stub, is dropped.
    pub(crate) fn restore_cursors(&self, file: &CursorFile) -> usize {
        if file.is_empty() {
            return 0;
        }
        let by_key: std::collections::HashMap<&str, &CursorEntry> = file
            .stubs
            .iter()
            .map(|entry| (entry.key.as_str(), entry))
            .collect();
        self.mutate_stubs(|stubs| {
            let mut resumed = 0;
            let keys = keys(stubs);
            for (state, key) in stubs.iter_mut().zip(keys) {
                if let Some(next) = by_key
                    .get(key.as_str())
                    .and_then(|entry| state.resumed(entry))
                {
                    *state = Arc::new(next);
                    resumed += 1;
                }
            }
            resumed
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn imposter(stubs: serde_json::Value) -> Imposter {
        let config =
            serde_json::from_value(json!({"port": 4545, "protocol": "http", "stubs": stubs}))
                .expect("config");
        Imposter::new(config).expect("imposter")
    }

    fn bodies(labels: &[&str]) -> serde_json::Value {
        json!(
            labels
                .iter()
                .map(|l| json!({"is": {"body": l}}))
                .collect::<Vec<_>>()
        )
    }

    fn next(imposter: &Imposter, index: usize) -> String {
        let snapshot = imposter.snapshot();
        let state = &snapshot.stubs()[index];
        let response = imposter
            .next_stub_response(state)
            .expect("no sequencer")
            .expect("has responses");
        serde_json::to_value(response).expect("prints")["is"]["body"]
            .as_str()
            .expect("string body")
            .to_string()
    }

    #[test]
    fn a_snapshot_resumes_on_an_imposter_with_the_same_stubs() {
        let stubs = json!([
            {"responses": bodies(&["a1", "a2", "a3"])},
            {"id": "b", "responses": [{"is": {"body": "b1"}, "repeat": 2}, {"is": {"body": "b2"}}]},
        ]);
        let first = imposter(stubs.clone());
        assert_eq!(next(&first, 0), "a1");
        assert_eq!(next(&first, 1), "b1");
        let file = first.cursor_snapshot();
        assert_eq!(file.stubs.len(), 2);
        assert_eq!(file.stubs[1].key, "b");
        assert_eq!((file.stubs[1].response, file.stubs[1].repeat), (0, 1));

        let round_tripped =
            CursorFile::parse(&serde_json::to_vec(&file).expect("serializes")).expect("parses");
        let second = imposter(stubs);
        assert_eq!(second.restore_cursors(&round_tripped), 2);
        assert_eq!(next(&second, 0), "a2");
        assert_eq!(
            next(&second, 1),
            "b1",
            "the repeat run is resumed, not restarted"
        );
        assert_eq!(next(&second, 1), "b2");
    }

    #[test]
    fn a_stub_at_the_start_of_its_cycle_is_left_out() {
        let first = imposter(json!([{"responses": bodies(&["a1", "a2"])}]));
        assert!(first.cursor_snapshot().is_empty());
        assert_eq!(next(&first, 0), "a1");
        assert_eq!(next(&first, 0), "a2");
        assert!(
            first.cursor_snapshot().is_empty(),
            "wrapped back to the start"
        );
    }

    #[test]
    fn a_changed_stub_starts_over_and_its_unchanged_sibling_resumes() {
        let first = imposter(json!([
            {"responses": bodies(&["a1", "a2"])},
            {"responses": bodies(&["b1", "b2"])},
        ]));
        next(&first, 0);
        next(&first, 1);
        let file = first.cursor_snapshot();

        let edited = imposter(json!([
            {"responses": bodies(&["a1", "a2", "a3"])},
            {"responses": bodies(&["b1", "b2"])},
        ]));
        assert_eq!(edited.restore_cursors(&file), 1);
        assert_eq!(next(&edited, 0), "a1", "new content restarts");
        assert_eq!(next(&edited, 1), "b2", "the unchanged stub resumes");
    }

    #[test]
    fn a_cursor_follows_its_stub_when_another_is_inserted_ahead() {
        let first = imposter(json!([{"responses": bodies(&["a1", "a2"])}]));
        next(&first, 0);
        let file = first.cursor_snapshot();
        let inserted = imposter(json!([
            {"responses": bodies(&["x1", "x2"])},
            {"responses": bodies(&["a1", "a2"])},
        ]));
        assert_eq!(inserted.restore_cursors(&file), 1);
        assert_eq!(next(&inserted, 0), "x1");
        assert_eq!(next(&inserted, 1), "a2");
    }

    #[test]
    fn an_entry_that_does_not_fit_its_stub_is_dropped() {
        let target = imposter(json!([{"id": "s", "responses": bodies(&["a1", "a2"])}]));
        let entry = |response, cycle: Option<Vec<u32>>| CursorFile {
            version: CURSOR_FILE_VERSION,
            stubs: vec![CursorEntry {
                key: "s".into(),
                response,
                repeat: 0,
                cycle,
            }],
        };
        assert_eq!(target.restore_cursors(&entry(2, None)), 0, "past the end");
        assert_eq!(target.restore_cursors(&entry(0, Some(vec![0, 5]))), 0);
        assert_eq!(target.restore_cursors(&entry(0, Some(vec![]))), 0);
        assert_eq!(next(&target, 0), "a1");
        assert_eq!(target.restore_cursors(&entry(1, Some(vec![1, 0]))), 1);
        assert_eq!(next(&target, 0), "a1", "position 1 of the order [1, 0]");
        assert_eq!(next(&target, 0), "a2");
    }

    #[test]
    fn an_unknown_version_or_malformed_file_is_an_error() {
        assert!(CursorFile::parse(br#"{"version":2,"stubs":[]}"#).is_err());
        assert!(CursorFile::parse(b"{not json").is_err());
        assert!(CursorFile::parse(br#"{"version":1}"#).is_ok());
    }

    #[test]
    fn serving_and_editing_mark_the_imposter_dirty() {
        let target = imposter(json!([{"responses": bodies(&["a1", "a2"])}]));
        assert!(!target.take_cursors_dirty());
        next(&target, 0);
        assert!(target.take_cursors_dirty());
        assert!(!target.take_cursors_dirty(), "taking it clears it");
        target.restart_cycles();
        assert!(target.take_cursors_dirty(), "a reset is written too");
    }
}
