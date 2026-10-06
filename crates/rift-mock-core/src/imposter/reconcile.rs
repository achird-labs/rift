//! Incremental config reconciliation (issue #316): stable stub identity, the order-aware
//! stub edit script, apply reports, and imposter change events.

use super::core::StubState;
use super::types::{ImposterError, Stub};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::error;

/// Whether an imposter is written through to the manager's datadir (issue #1122).
///
/// Persist-on-create exists so an imposter created through the admin API survives a restart
/// (issues #563/#575). An imposter loaded from `--configfile`/`--imposters` already has a store it
/// is re-read from; a datadir copy of it would be a second imposter with no rule to match it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persistence {
    /// Written to `{port}.json` on create and on every config change.
    Datadir,
    /// Never written to the datadir.
    Ephemeral,
}

/// One entry of the desired set passed to
/// [`ImposterManager::apply_desired`](super::ImposterManager::apply_desired).
#[derive(Debug, Clone)]
pub struct DesiredImposter {
    pub config: super::types::ImposterConfig,
    pub persistence: Persistence,
}

/// Outcome of [`ImposterManager::apply_config`](super::ImposterManager::apply_config):
/// which ports were created, replaced wholesale, stub-patched in place, deleted, or
/// failed to apply. Untouched imposters appear in none of the lists. A port may appear
/// in more than one list when that is the truth — e.g. a wholesale replace whose recreate
/// fails after teardown lands in both `deleted` and `failed`, and a patched imposter whose
/// datadir write fails lands in both `stub_patched` and `failed`. Failures for configs
/// without an explicit port (auto-assign creates) are reported under port `0`. Explicit-port configs
/// are applied before auto-assigned ones (issue #1112), so `created` lists explicit ports first, in
/// input order, then auto-assigned ports.
#[derive(Debug, Default)]
pub struct ApplyReport {
    pub created: Vec<u16>,
    pub replaced: Vec<u16>,
    pub stub_patched: Vec<u16>,
    /// Ports whose only imposter-level change was the `enabled` flag, applied
    /// in place — runtime state intact (pausing an imposter must never reset
    /// its scenario state).
    pub toggled: Vec<u16>,
    pub deleted: Vec<u16>,
    pub failed: Vec<(u16, ImposterError)>,
}

/// Outcome of [`ImposterManager::delete_all`](super::ImposterManager::delete_all) (issue #1124).
///
/// A port in `failed` was **not** deleted: it is still registered and serving, because its datadir
/// file could not be removed and deleting it anyway would bring it back on the next restart.
#[derive(Debug, Default)]
pub struct DeleteAllReport {
    pub deleted: Vec<super::types::ImposterConfig>,
    pub failed: Vec<(u16, ImposterError)>,
}

/// A config mutation observed on the manager (issue #316), for embedders that need to
/// react to config changes (audit logging, persistence hooks, webhooks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImposterEvent {
    Created(u16),
    Replaced(u16),
    StubsChanged(u16),
    /// The serve/pause flag flipped (issue #817): config, not ephemera — the
    /// toggle persists and replicates like any other config change.
    EnabledChanged {
        port: u16,
        enabled: bool,
    },
    Deleted(u16),
    AllDeleted,
}

/// Who caused a change event (issue #855).
///
/// Attribution rides here, on the listener signature, rather than on [`ImposterEvent`]: the enum
/// is not `#[non_exhaustive]`, so adding a field to every variant would break every downstream
/// `match` — the wrong trade for a seam whose premise is that installing nothing changes nothing.
/// `#[non_exhaustive]` for the same reason attribution is not on [`ImposterEvent`]: the next
/// attribution field (scope, request id, remote addr) must not be a second breaking change to the
/// same seam. Embedders read this type; to build one in a test, start from
/// [`Default`] and assign — a struct literal, including `..Default::default()`, is not available
/// outside this crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventContext {
    /// The principal an [`AdminAuthorizer`](crate::extensions::authz::AdminAuthorizer) attributed
    /// the causing request to, via `AuthzDecision::Allow { principal }`.
    ///
    /// `None` whenever there is nobody to name — no authorizer installed, an authorizer that
    /// allowed without identifying anyone, or a mutation with no request behind it at all (a
    /// config-file load, or an embedder driving [`ImposterManager`](super::ImposterManager)
    /// directly). Absent attribution is reported as absent; it is never guessed.
    pub principal: Option<String>,
}

/// Observer for [`ImposterEvent`]s; register via
/// [`ImposterManager::with_event_listener`](super::ImposterManager::with_event_listener).
/// Called synchronously on the mutating path — keep implementations fast and non-blocking.
///
/// `ctx` carries attribution (issue #855). A listener that only cares *what* changed ignores it.
pub trait ImposterEventListener: Send + Sync {
    fn on_event(&self, event: &ImposterEvent, ctx: &EventContext);
}

/// Stable stub identity: the explicit `id` (issue #202) if set, else
/// `"~" + <16-hex content hash> + "#" + <occurrence among content-identical siblings>`.
/// The `~` prefix keeps generated keys disjoint from user-supplied ids.
pub fn stub_key(stub: &Stub, occurrence: usize) -> String {
    match &stub.id {
        Some(id) => id.clone(),
        None => key_for(None, content_hash(stub), occurrence),
    }
}

/// [`stub_key`] from a stub's id and an already-computed [`content_hash`].
pub(crate) fn key_for(id: Option<&str>, hash: u64, occurrence: usize) -> String {
    match id {
        Some(id) => id.to_string(),
        None => format!("~{hash:016x}#{occurrence}"),
    }
}

/// FNV-1a 64 over the stub's canonical JSON. `Stub` holds `std::collections::HashMap`s
/// (predicate operations, response headers) that serialize in per-instance iteration order, so the
/// stub is first converted to a `serde_json::Value`, whose objects are sorted maps (`preserve_order`
/// is off workspace-wide), and that is what gets hashed (issue #1256). Struct fields are sorted
/// too, so this form differs from serializing the stub directly: every id-less key moved once.
pub(crate) fn content_hash(stub: &Stub) -> u64 {
    let mut hasher = Fnv1a::default();
    let hashed = serde_json::to_value(stub)
        .and_then(|canonical| serde_json::to_writer(&mut hasher, &canonical));
    if let Err(e) = hashed {
        // Debug output is content-distinguishing but not canonical across processes —
        // keys built from it may churn between reloads, so make the degradation visible.
        error!("stub serialization failed while keying; falling back to Debug format: {e}");
        hasher = Fnv1a::default();
        hasher.update(format!("{stub:?}").as_bytes());
    }
    hasher.0
}

/// Streaming FNV-1a 64, so the canonical JSON is hashed without materializing a `String`. Also the
/// `_rift.conditional` ETag (issue #1280), which must be the same on every process and node.
pub(crate) struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv1a {
    pub(crate) fn finish(&self) -> u64 {
        self.0
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        self.0 = bytes.iter().fold(self.0, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
        });
    }
}

impl std::io::Write for Fnv1a {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Keys for a stub sequence; occurrence is counted per content hash so content-identical
/// id-less siblings get distinct keys and stay individually addressable in the diff.
#[cfg(test)]
pub(crate) fn stub_keys(stubs: &[Stub]) -> Vec<String> {
    keys_from(
        stubs
            .iter()
            .map(|stub| (stub.id.as_deref(), content_hash(stub))),
    )
}

/// Keys from each stub's `(id, content hash)`, so a caller holding cached hashes (live
/// [`StubState`]s) never re-serializes a stub.
fn keys_from<'a>(stubs: impl Iterator<Item = (Option<&'a str>, u64)>) -> Vec<String> {
    let mut seen: HashMap<u64, usize> = HashMap::new();
    stubs
        .map(|(id, hash)| match id {
            Some(id) => id.to_string(),
            None => {
                let occurrence = seen.entry(hash).or_insert(0);
                let key = key_for(None, hash, *occurrence);
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
    Patched {
        /// Sequencer keys (`stub_key(stub, 0)`) of stubs the patch removed, so the
        /// manager can fire the per-stub `reset_scope` GC hook (issue #313).
        removed_keys: Vec<String>,
    },
    /// More than half the stubs would change — the caller should replace the imposter
    /// wholesale instead of thrashing the stub set in place.
    Degenerate,
}

/// What reconciling a live stub vector toward a desired one would do, computed without touching
/// the live vector, so an `Unchanged` or `Degenerate` outcome stores nothing (issue #1254).
pub(crate) enum StubPlan {
    Unchanged,
    Degenerate,
    Patched {
        /// The new stub vector, in desired order, reusing every surviving `Arc<StubState>`.
        next: Vec<Arc<StubState>>,
        /// Sequencer keys (occurrence 0) of the stubs the patch removes.
        removed_keys: Vec<String>,
    },
}

/// Plan the reconcile of `states` toward `desired`, preserving per-slot cycling state for every
/// stub whose key survives. Pure moves (reorder) preserve everything; a same-key content change
/// (explicit id) swaps the stub in place like `replace_stub_by_id`. `Degenerate` when the changed
/// fraction exceeds 1/2 (pure moves cost nothing in that metric).
///
/// Content is compared by [`content_hash`]: cached on each live state, computed once per desired
/// stub (issue #1254). A content key already embeds the hash, so for id-less stubs a matching key
/// is a matching content; for an explicit id the hashes are compared. Equal `(id, hash)` sequences
/// are `Unchanged`, which is the common case of re-applying an unchanged set and costs one
/// serialization per desired stub and nothing per live one.
pub(crate) fn plan_stub_reconcile(states: &[Arc<StubState>], desired: &[Stub]) -> StubPlan {
    let desired_hashes: Vec<u64> = desired.iter().map(content_hash).collect();
    let unchanged = states.len() == desired.len()
        && states
            .iter()
            .zip(desired.iter().zip(&desired_hashes))
            .all(|(state, (stub, hash))| state.stub.id == stub.id && state.content_hash() == *hash);
    if unchanged {
        return StubPlan::Unchanged;
    }

    let old_keys = keys_from(
        states
            .iter()
            .map(|state| (state.stub.id.as_deref(), state.content_hash())),
    );
    let new_keys = keys_from(
        desired
            .iter()
            .zip(&desired_hashes)
            .map(|(stub, hash)| (stub.id.as_deref(), *hash)),
    );

    let old_index: HashMap<&String, usize> =
        old_keys.iter().enumerate().map(|(i, k)| (k, i)).collect();
    let new_set: HashSet<&String> = new_keys.iter().collect();

    let deletes = old_keys.iter().filter(|k| !new_set.contains(*k)).count();
    let mut inserts = 0usize;
    let mut content_replaced = 0usize;
    for (i, key) in new_keys.iter().enumerate() {
        match old_index.get(key) {
            None => inserts += 1,
            // Same explicit id, different content.
            Some(&j) if states[j].content_hash() != desired_hashes[i] => {
                content_replaced += 1;
            }
            Some(_) => {}
        }
    }

    // Changed fraction over both sides: a content replace touches one slot on each side.
    let changed_slots = deletes + inserts + 2 * content_replaced;
    if changed_slots * 2 > states.len() + desired.len() {
        return StubPlan::Degenerate;
    }

    let mut by_key: HashMap<String, Arc<StubState>> =
        old_keys.into_iter().zip(states.iter().cloned()).collect();
    // Only an inserted or changed stub is cloned out of `desired`; a surviving one reuses its Arc.
    let next = new_keys
        .into_iter()
        .zip(desired.iter().zip(desired_hashes))
        .map(|(key, (stub, hash))| match by_key.remove(&key) {
            // Same key: keep the slot's cycler + slot token; only rebuild the Arc when the
            // stub content actually changed (issue #287).
            Some(state) if state.content_hash() == hash => state,
            Some(state) => Arc::new(state.with_stub_hashed(stub.clone(), hash)),
            None => Arc::new(StubState::with_hash(stub.clone(), hash)),
        })
        .collect();
    let removed_keys = by_key
        .into_values()
        .map(|state| state.sequence_key())
        .collect();
    StubPlan::Patched { next, removed_keys }
}

/// Apply [`plan_stub_reconcile`] to an owned vector, which is left untouched unless the plan
/// patches it.
#[cfg(test)]
pub(crate) fn reconcile_stub_states(
    states: &mut Vec<Arc<StubState>>,
    desired: Vec<Stub>,
) -> StubReconcile {
    match plan_stub_reconcile(states, &desired) {
        StubPlan::Unchanged => StubReconcile::Unchanged,
        StubPlan::Degenerate => StubReconcile::Degenerate,
        StubPlan::Patched { next, removed_keys } => {
            *states = next;
            StubReconcile::Patched { removed_keys }
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

    /// Build a live stub state (states are stored behind `Arc` since #287).
    fn st(stub: Stub) -> Arc<StubState> {
        Arc::new(StubState::new(stub))
    }

    /// Serve the state's next response and return its body (advances the cycler).
    fn next_body(state: &StubState) -> String {
        let resp = state.get_next_response().expect("stub has responses");
        serde_json::to_value(resp).expect("serialize response")["is"]["body"]
            .as_str()
            .expect("string body")
            .to_string()
    }

    /// A stub whose predicate and response headers are multi-key `HashMap`s — the shape whose
    /// serialization order varied per parse before issue #1256.
    const MULTI_KEY_STUB: &str = r#"{
        "predicates": [
            {"equals": {"method": "GET", "path": "/a", "query": {"q": "1", "r": "2"}, "body": "x"}},
            {"or": [{"contains": {"path": "/a", "body": "y", "method": "G"}}, {"not": {"startsWith": {"path": "/b", "body": "z"}}}]}
        ],
        "responses": [{"is": {"statusCode": 200, "headers": {"Content-Type": "application/json", "X-A": "1", "X-B": "2", "X-C": "3", "X-D": "4"}, "body": "b"}}]
    }"#;

    fn parse_multi() -> Stub {
        serde_json::from_str(MULTI_KEY_STUB).expect("multi-key stub json")
    }

    // Issue #1256: identical content must key identically however its maps were built.
    #[test]
    fn stub_key_is_stable_across_fresh_parses() {
        let first = stub_key(&parse_multi(), 0);
        for _ in 0..20 {
            assert_eq!(stub_key(&parse_multi(), 0), first);
        }
    }

    #[test]
    fn identical_multi_key_stubs_are_unchanged() {
        let mut live = vec![st(parse_multi()), st(one_resp("a"))];
        let before: Vec<_> = live.iter().map(Arc::clone).collect();
        let outcome = reconcile_stub_states(&mut live, vec![parse_multi(), one_resp("a")]);
        assert!(
            matches!(outcome, StubReconcile::Unchanged),
            "a re-parsed identical set must be Unchanged, got {outcome:?}"
        );
        assert!(live.iter().zip(&before).all(|(a, b)| Arc::ptr_eq(a, b)));
    }

    #[test]
    fn identical_multi_key_siblings_get_distinct_occurrences() {
        let keys = stub_keys(&[parse_multi(), parse_multi()]);
        assert!(keys[0].ends_with("#0"), "{keys:?}");
        assert!(keys[1].ends_with("#1"), "{keys:?}");
        assert_eq!(
            keys[0].trim_end_matches("#0"),
            keys[1].trim_end_matches("#1")
        );
    }

    // The key is process-independent (rift-cluster shares response cursors across nodes by it), so
    // its exact value is pinned: any change to the hashed form moves every id-less stub's key.
    #[test]
    fn stub_key_value_is_pinned() {
        assert_eq!(stub_key(&one_resp("a"), 0), "~d760f3e4c82afe1f#0");
    }

    #[test]
    fn stub_key_ignores_json_key_order() {
        let a = stub(json!({
            "predicates": [{"equals": {"method": "GET", "path": "/p"}}],
            "responses": [{"is": {"headers": {"A": "1", "B": "2"}, "statusCode": 200}}]
        }));
        let b = stub(json!({
            "responses": [{"is": {"statusCode": 200, "headers": {"B": "2", "A": "1"}}}],
            "predicates": [{"equals": {"path": "/p", "method": "GET"}}]
        }));
        assert_eq!(stub_key(&a, 0), stub_key(&b, 0));
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
            st(two_resp("a1", "a2")),
            st(one_resp("b")),
            st(one_resp("c")),
        ];
        assert_eq!(next_body(&states[0]), "a1");

        let desired = vec![two_resp("a1", "a2"), one_resp("b"), one_resp("c2")];
        let outcome = reconcile_stub_states(&mut states, desired);
        assert!(matches!(outcome, StubReconcile::Patched { .. }));
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
        let mut states = vec![st(two_resp("a1", "a2")), st(two_resp("b1", "b2"))];
        assert_eq!(next_body(&states[0]), "a1");
        assert_eq!(next_body(&states[1]), "b1");

        let desired = vec![two_resp("b1", "b2"), two_resp("a1", "a2")];
        let outcome = reconcile_stub_states(&mut states, desired);
        assert!(matches!(outcome, StubReconcile::Patched { .. }));
        assert_eq!(next_body(&states[0]), "b2", "moved stub keeps its cursor");
        assert_eq!(next_body(&states[1]), "a2", "moved stub keeps its cursor");
    }

    // AC2c: > 50 % of stubs changing is degenerate — no in-place mutation.

    #[test]
    fn degenerate_ratio_falls_back_without_mutating() {
        let mut states = vec![st(one_resp("a")), st(one_resp("b"))];
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
        let mut states = vec![st(one_resp("a")), st(one_resp("b"))];
        let outcome = reconcile_stub_states(&mut states, vec![one_resp("a"), one_resp("b")]);
        assert!(matches!(outcome, StubReconcile::Unchanged));
    }

    #[test]
    fn same_id_content_change_patches_in_place_keeping_cursor() {
        let mut with_id = two_resp("v1a", "v1b");
        with_id.id = Some("s1".into());
        let mut states = vec![st(with_id), st(one_resp("b")), st(one_resp("c"))];
        assert_eq!(next_body(&states[0]), "v1a");

        let mut updated = two_resp("v2a", "v2b");
        updated.id = Some("s1".into());
        let outcome =
            reconcile_stub_states(&mut states, vec![updated, one_resp("b"), one_resp("c")]);
        assert!(matches!(outcome, StubReconcile::Patched { .. }));
        assert_eq!(
            next_body(&states[0]),
            "v2b",
            "in-place id replace keeps the slot's cursor"
        );
    }

    #[test]
    fn insertion_below_threshold_patches() {
        let mut states = vec![
            st(two_resp("a1", "a2")),
            st(one_resp("b")),
            st(one_resp("c")),
        ];
        assert_eq!(next_body(&states[0]), "a1");

        let desired = vec![
            two_resp("a1", "a2"),
            one_resp("new"),
            one_resp("b"),
            one_resp("c"),
        ];
        let outcome = reconcile_stub_states(&mut states, desired);
        assert!(matches!(outcome, StubReconcile::Patched { .. }));
        assert_eq!(states.len(), 4);
        assert_eq!(next_body(&states[0]), "a2");
        assert_eq!(next_body(&states[1]), "new");
    }
}
