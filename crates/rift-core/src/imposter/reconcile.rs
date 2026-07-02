//! Incremental config reconciliation (issue #316): stable stub identity, the order-aware
//! stub edit script, apply reports, and imposter change events.

use super::core::StubState;
use super::types::{ImposterError, Stub};
use std::collections::{HashMap, HashSet};
use tracing::error;

/// Outcome of [`ImposterManager::apply_config`](super::ImposterManager::apply_config):
/// which ports were created, replaced wholesale, stub-patched in place, deleted, or
/// failed to apply. Untouched imposters appear in none of the lists. A port may appear
/// in more than one list when that is the truth — e.g. a wholesale replace whose recreate
/// fails after teardown lands in both `deleted` and `failed`, and a patched imposter whose
/// datadir write fails lands in both `stub_patched` and `failed`. Failures for configs
/// without an explicit port (auto-assign creates) are reported under port `0`.
#[derive(Debug, Default)]
pub struct ApplyReport {
    pub created: Vec<u16>,
    pub replaced: Vec<u16>,
    pub stub_patched: Vec<u16>,
    pub deleted: Vec<u16>,
    pub failed: Vec<(u16, ImposterError)>,
}

/// A config mutation observed on the manager (issue #316), for embedders that need to
/// react to config changes (audit logging, persistence hooks, webhooks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImposterEvent {
    Created(u16),
    Replaced(u16),
    StubsChanged(u16),
    Deleted(u16),
    AllDeleted,
}

/// Observer for [`ImposterEvent`]s; register via
/// [`ImposterManager::with_event_listener`](super::ImposterManager::with_event_listener).
/// Called synchronously on the mutating path — keep implementations fast and non-blocking.
pub trait ImposterEventListener: Send + Sync {
    fn on_event(&self, event: &ImposterEvent);
}

/// Stable stub identity: the explicit `id` (issue #202) if set, else
/// `"~" + <16-hex content hash> + "#" + <occurrence among byte-identical siblings>`.
/// The `~` prefix keeps generated keys disjoint from user-supplied ids.
pub fn stub_key(stub: &Stub, occurrence: usize) -> String {
    match &stub.id {
        Some(id) => id.clone(),
        None => format!("~{:016x}#{occurrence}", content_hash(stub)),
    }
}

/// FNV-1a 64 over the stub's serialized JSON. Deterministic: struct field order is fixed
/// and serde_json maps are sorted (the `preserve_order` feature is off workspace-wide).
fn content_hash(stub: &Stub) -> u64 {
    let canonical = serde_json::to_string(stub).unwrap_or_else(|e| {
        // Debug output is content-distinguishing but not canonical across processes —
        // keys built from it may churn between reloads, so make the degradation visible.
        error!("stub serialization failed while keying; falling back to Debug format: {e}");
        format!("{stub:?}")
    });
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    canonical.bytes().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

/// Keys for a stub sequence; occurrence is counted per content hash so byte-identical
/// id-less siblings get distinct keys and stay individually addressable in the diff.
pub(crate) fn stub_keys(stubs: &[Stub]) -> Vec<String> {
    stub_keys_iter(stubs.iter())
}

fn stub_keys_iter<'a>(stubs: impl Iterator<Item = &'a Stub>) -> Vec<String> {
    let mut seen: HashMap<u64, usize> = HashMap::new();
    stubs
        .map(|stub| match &stub.id {
            Some(id) => id.clone(),
            None => {
                let hash = content_hash(stub);
                let occurrence = seen.entry(hash).or_insert(0);
                let key = format!("~{hash:016x}#{occurrence}");
                *occurrence += 1;
                key
            }
        })
        .collect()
}

/// Outcome of a stub-level reconcile.
#[derive(Debug)]
pub(crate) enum StubReconcile {
    /// Same stubs, same order — nothing touched.
    Unchanged,
    /// Edited in place; untouched slots kept their cycling state.
    Patched,
    /// More than half the stubs would change — the caller should replace the imposter
    /// wholesale instead of thrashing the stub set in place.
    Degenerate,
}

/// Reconcile a live `StubState` vector toward `desired`, preserving per-slot cycling
/// state for every stub whose key survives. Pure moves (reorder) preserve everything;
/// a same-key content change (explicit id) swaps the stub in place like
/// `replace_stub_by_id`. Returns `Degenerate` — without mutating — when the changed
/// fraction exceeds 1/2 (pure moves cost nothing in that metric).
pub(crate) fn reconcile_stub_states(
    states: &mut Vec<StubState>,
    desired: Vec<Stub>,
) -> StubReconcile {
    let old_keys = stub_keys_iter(states.iter().map(|s| &s.stub));
    let new_keys = stub_keys(&desired);

    let old_index: HashMap<&String, usize> =
        old_keys.iter().enumerate().map(|(i, k)| (k, i)).collect();
    let new_set: HashSet<&String> = new_keys.iter().collect();

    let deletes = old_keys.iter().filter(|k| !new_set.contains(*k)).count();
    let mut inserts = 0usize;
    let mut content_replaced = 0usize;
    for (i, key) in new_keys.iter().enumerate() {
        match old_index.get(key) {
            None => inserts += 1,
            // Same key, different content — explicit-id stubs (or a hash collision).
            Some(&j) if stubs_differ(&desired[i], &states[j].stub) => {
                content_replaced += 1;
            }
            Some(_) => {}
        }
    }

    if old_keys == new_keys && content_replaced == 0 {
        return StubReconcile::Unchanged;
    }
    // Changed fraction over both sides: a content replace touches one slot on each side.
    let changed_slots = deletes + inserts + 2 * content_replaced;
    if changed_slots * 2 > states.len() + desired.len() {
        return StubReconcile::Degenerate;
    }

    let mut by_key: HashMap<String, StubState> =
        old_keys.into_iter().zip(states.drain(..)).collect();
    *states = new_keys
        .into_iter()
        .zip(desired)
        .map(|(key, stub)| match by_key.remove(&key) {
            Some(mut state) => {
                if stubs_differ(&state.stub, &stub) {
                    state.stub = stub;
                }
                state
            }
            None => StubState::new(stub),
        })
        .collect();
    StubReconcile::Patched
}

/// Content inequality via canonical JSON. A serialization failure on either side counts
/// as "differs" — the conservative direction for a diff (worst case an unneeded replace,
/// never a silently dropped change) — and is logged.
fn stubs_differ(a: &Stub, b: &Stub) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(va), Ok(vb)) => va != vb,
        (ra, rb) => {
            error!(
                "stub serialization failed during reconcile; treating as changed: {:?} {:?}",
                ra.err(),
                rb.err()
            );
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::core::StubState;
    use crate::imposter::types::Stub;
    use serde_json::json;

    fn stub(v: serde_json::Value) -> Stub {
        serde_json::from_value(v).expect("test stub json")
    }

    fn one_resp(body: &str) -> Stub {
        stub(json!({
            "predicates": [{"equals": {"path": format!("/{body}")}}],
            "responses": [{"is": {"statusCode": 200, "body": body}}]
        }))
    }

    fn two_resp(first: &str, second: &str) -> Stub {
        stub(json!({
            "predicates": [{"equals": {"path": "/cycled"}}],
            "responses": [
                {"is": {"statusCode": 200, "body": first}},
                {"is": {"statusCode": 200, "body": second}}
            ]
        }))
    }

    /// Serve the state's next response and return its body (advances the cycler).
    fn next_body(state: &StubState) -> String {
        let resp = state.get_next_response().expect("stub has responses");
        serde_json::to_value(resp).expect("serialize response")["is"]["body"]
            .as_str()
            .expect("string body")
            .to_string()
    }

    // AC3: determinism, occurrence suffixes, "~" disjointness from user ids.

    #[test]
    fn stub_key_is_deterministic_and_prefixed() {
        let a = one_resp("a");
        let b = one_resp("a");
        let key = stub_key(&a, 0);
        assert_eq!(
            key,
            stub_key(&b, 0),
            "same content must hash to the same key"
        );
        assert!(key.starts_with('~'), "generated keys carry the ~ prefix");
        assert!(key.ends_with("#0"));
        assert_eq!(
            key.len(),
            "~".len() + 16 + "#0".len(),
            "16-hex content hash"
        );
    }

    #[test]
    fn stub_key_uses_explicit_id_verbatim() {
        let mut s = one_resp("a");
        s.id = Some("checkout-flow".into());
        assert_eq!(stub_key(&s, 0), "checkout-flow");
        assert_eq!(
            stub_key(&s, 3),
            "checkout-flow",
            "occurrence is irrelevant for explicit ids"
        );
    }

    #[test]
    fn stub_key_occurrence_suffix_disambiguates_identical_siblings() {
        let s = one_resp("dup");
        let keys = stub_keys(&[s.clone(), s.clone(), one_resp("other")]);
        assert_eq!(keys.len(), 3);
        assert_ne!(
            keys[0], keys[1],
            "byte-identical siblings get distinct keys"
        );
        assert!(keys[0].ends_with("#0"));
        assert!(keys[1].ends_with("#1"));
        assert!(keys[2].ends_with("#0"));
        assert_ne!(keys[0], keys[2]);
    }

    #[test]
    fn stub_key_content_change_changes_key() {
        assert_ne!(stub_key(&one_resp("a"), 0), stub_key(&one_resp("b"), 0));
    }

    // AC2a: a stub-level patch preserves untouched stubs' cursors.

    #[test]
    fn patch_preserves_untouched_stub_cursors() {
        let mut states = vec![
            StubState::new(two_resp("a1", "a2")),
            StubState::new(one_resp("b")),
            StubState::new(one_resp("c")),
        ];
        assert_eq!(next_body(&states[0]), "a1");

        let desired = vec![two_resp("a1", "a2"), one_resp("b"), one_resp("c2")];
        let outcome = reconcile_stub_states(&mut states, desired);
        assert!(matches!(outcome, StubReconcile::Patched));
        assert_eq!(states.len(), 3);
        assert_eq!(
            next_body(&states[0]),
            "a2",
            "untouched stub keeps its cursor"
        );
        assert_eq!(next_body(&states[2]), "c2", "changed stub swapped in");
    }

    // AC2b: a pure reorder converges without resetting any cursor.

    #[test]
    fn pure_reorder_preserves_all_cursors() {
        let mut states = vec![
            StubState::new(two_resp("a1", "a2")),
            StubState::new(two_resp("b1", "b2")),
        ];
        assert_eq!(next_body(&states[0]), "a1");
        assert_eq!(next_body(&states[1]), "b1");

        let desired = vec![two_resp("b1", "b2"), two_resp("a1", "a2")];
        let outcome = reconcile_stub_states(&mut states, desired);
        assert!(matches!(outcome, StubReconcile::Patched));
        assert_eq!(next_body(&states[0]), "b2", "moved stub keeps its cursor");
        assert_eq!(next_body(&states[1]), "a2", "moved stub keeps its cursor");
    }

    // AC2c: > 50 % of stubs changing is degenerate — no in-place mutation.

    #[test]
    fn degenerate_ratio_falls_back_without_mutating() {
        let mut states = vec![StubState::new(one_resp("a")), StubState::new(one_resp("b"))];
        let outcome = reconcile_stub_states(&mut states, vec![one_resp("x"), one_resp("y")]);
        assert!(matches!(outcome, StubReconcile::Degenerate));
        assert_eq!(
            next_body(&states[0]),
            "a",
            "a degenerate outcome must not mutate the live stubs"
        );
        assert_eq!(next_body(&states[1]), "b");
    }

    #[test]
    fn identical_stubs_are_unchanged() {
        let mut states = vec![StubState::new(one_resp("a")), StubState::new(one_resp("b"))];
        let outcome = reconcile_stub_states(&mut states, vec![one_resp("a"), one_resp("b")]);
        assert!(matches!(outcome, StubReconcile::Unchanged));
    }

    #[test]
    fn same_id_content_change_patches_in_place_keeping_cursor() {
        let mut with_id = two_resp("v1a", "v1b");
        with_id.id = Some("s1".into());
        let mut states = vec![
            StubState::new(with_id),
            StubState::new(one_resp("b")),
            StubState::new(one_resp("c")),
        ];
        assert_eq!(next_body(&states[0]), "v1a");

        let mut updated = two_resp("v2a", "v2b");
        updated.id = Some("s1".into());
        let outcome =
            reconcile_stub_states(&mut states, vec![updated, one_resp("b"), one_resp("c")]);
        assert!(matches!(outcome, StubReconcile::Patched));
        assert_eq!(
            next_body(&states[0]),
            "v2b",
            "in-place id replace keeps the slot's cursor"
        );
    }

    #[test]
    fn insertion_below_threshold_patches() {
        let mut states = vec![
            StubState::new(two_resp("a1", "a2")),
            StubState::new(one_resp("b")),
            StubState::new(one_resp("c")),
        ];
        assert_eq!(next_body(&states[0]), "a1");

        let desired = vec![
            two_resp("a1", "a2"),
            one_resp("new"),
            one_resp("b"),
            one_resp("c"),
        ];
        let outcome = reconcile_stub_states(&mut states, desired);
        assert!(matches!(outcome, StubReconcile::Patched));
        assert_eq!(states.len(), 4);
        assert_eq!(next_body(&states[0]), "a2");
        assert_eq!(next_body(&states[1]), "new");
    }
}
