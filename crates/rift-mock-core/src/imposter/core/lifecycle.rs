//! Stub lifecycle (add/replace/delete/query) and enabled-state management.
//!
//! Part of the `Imposter` implementation; see `core/mod.rs` for the struct definition.

use super::*;
use crate::imposter::reconcile::{StubPlan, StubReconcile, plan_stub_reconcile};

impl Imposter {
    /// Move the stub at `from` to position `to`, carrying its cycling state with it
    /// (issue #316). Stub order is match priority, so this changes matching precedence.
    pub fn move_stub(&self, from: usize, to: usize) -> Result<(), ImposterError> {
        self.mutate_stubs(|stubs| {
            if from >= stubs.len() {
                return Err(ImposterError::StubIndexOutOfBounds(from));
            }
            if to >= stubs.len() {
                return Err(ImposterError::StubIndexOutOfBounds(to));
            }
            if from != to {
                let state = stubs.remove(from);
                stubs.insert(to, state);
                // Every stub between the two positions now has a different stub set ahead of it
                // (issue #1303); those past the range do not.
                restamp(stubs, from.min(to)..=from.max(to), self.load_clock.next());
            }
            Ok(())
        })
    }

    /// [`Self::reconcile_stubs_from`] by a door with the imposter's own access.
    #[cfg(test)]
    pub(crate) fn reconcile_stubs(&self, desired: &[Stub]) -> StubReconcile {
        let access = self.script_file_access();
        self.reconcile_stubs_from(desired, &access)
    }

    /// Reconcile live stubs toward `desired` under one write critical section (issue #316).
    ///
    /// Only a patch stores a new snapshot: `Unchanged` and `Degenerate` leave the current one, and
    /// its match index, in place rather than rebuilding an identical copy (issue #1254), which is
    /// why this does not go through [`mutate_stubs`](Self::mutate_stubs).
    ///
    /// `door` is what the writing door's scripts get (issue #1645). After an `Unchanged` or
    /// `Patched` outcome every stub is the door's, so the imposter's access becomes `door`, set in
    /// the same critical section and before the patched stubs are stored. A `door` that allows more
    /// than the current access is refused in place (`Widens`): until the patch lands, the stubs
    /// another door wrote would serve with it, so only a replace, whose every stub is new, may
    /// widen it.
    pub(crate) fn reconcile_stubs_from(
        &self,
        desired: &[Stub],
        door: &crate::imposter::ScriptFileAccess,
    ) -> StubReconcile {
        let (outcome, swapped) = {
            let _writer = self.stubs_write.lock();
            if !self.script_files.load().admits(door) {
                return StubReconcile::Widens;
            }
            let current = self.stubs_snapshot.load_full();
            match plan_stub_reconcile(current.stubs(), desired, &self.load_clock) {
                StubPlan::Unchanged => {
                    self.script_files.store(Arc::new(door.clone()));
                    (StubReconcile::Unchanged, None)
                }
                StubPlan::Degenerate => (StubReconcile::Degenerate, None),
                StubPlan::Patched { next, removed_keys } => {
                    self.script_files.store(Arc::new(door.clone()));
                    let after = Arc::new(StubSnapshot::build(next));
                    self.stubs_snapshot.store(Arc::clone(&after));
                    self.stub_warnings.store(None);
                    (
                        StubReconcile::Patched { removed_keys },
                        Some((current, after)),
                    )
                }
            }
        };
        if let Some((before, after)) = swapped {
            self.forget_orphaned_claims(before.stubs(), after.stubs());
        }
        outcome
    }

    /// Add a stub at a specific index
    pub fn add_stub(&self, stub: Stub, index: Option<usize>) {
        self.mutate_stubs(|stubs| {
            let idx = index.unwrap_or(stubs.len()).min(stubs.len());
            stubs.insert(
                idx,
                Arc::new(StubState::new(stub).with_loaded_at(self.load_clock.next())),
            );
        });
    }

    /// Add a stub, rejecting it if its `id` duplicates an existing stub (issue #202). The
    /// duplicate check and the insert happen under one write critical section so concurrent adds
    /// can't race. Returns `false` (not added) on id conflict — the caller holds the id for the error.
    #[must_use]
    pub fn add_stub_unique(&self, stub: Stub, index: Option<usize>) -> bool {
        self.add_stub_unique_from(None, stub, index)
    }

    /// [`Self::add_stub_unique`] for a door that writes scripts: see
    /// [`Self::mutate_stubs_from`] (issue #1645).
    pub(crate) fn add_stub_unique_from(
        &self,
        door: Option<&crate::imposter::ScriptFileAccess>,
        stub: Stub,
        index: Option<usize>,
    ) -> bool {
        self.mutate_stubs_from(door, |stubs| {
            if let Some(id) = stub.id.as_deref()
                && stubs.iter().any(|s| s.stub.id.as_deref() == Some(id))
            {
                return false;
            }
            let idx = index.unwrap_or(stubs.len()).min(stubs.len());
            stubs.insert(
                idx,
                Arc::new(StubState::new(stub).with_loaded_at(self.load_clock.next())),
            );
            true
        })
    }

    /// Replace the stub with `id` in place (position preserved). The replacement keeps `id` as its
    /// addressable id regardless of the supplied stub's `id`. Returns `false` if no such id.
    ///
    /// Unlike [`replace_stub`](Self::replace_stub), the stub keeps its response cycle: this is not
    /// a Mountebank route, so parity does not reach it, and rift-cluster drives `proxyAlways`
    /// merges through it, which must not restart a recording's cycle (#1515).
    #[must_use]
    pub fn replace_stub_by_id(&self, id: &str, stub: Stub) -> bool {
        self.replace_stub_by_id_from(None, id, stub)
    }

    /// [`Self::replace_stub_by_id`] for a door that writes scripts: see
    /// [`Self::mutate_stubs_from`] (issue #1645).
    pub(crate) fn replace_stub_by_id_from(
        &self,
        door: Option<&crate::imposter::ScriptFileAccess>,
        id: &str,
        mut stub: Stub,
    ) -> bool {
        self.mutate_stubs_from(door, |stubs| {
            match stubs.iter().position(|s| s.stub.id.as_deref() == Some(id)) {
                Some(i) => {
                    stub.id = Some(id.to_string());
                    // Swap a fresh Arc that reuses the slot's cycler + slot token, so the slot's
                    // response-cycling state is kept and in-flight requests holding the old Arc keep
                    // serving their snapshot (issue #287).
                    replace_at(stubs, i, stub, &self.load_clock, OnReplace::KeepCycle);
                    true
                }
                None => false,
            }
        })
    }

    /// Append `responses` to the stub with `id` as a `proxyAlways` recording appends them: they
    /// join the tail of its current response cycle, as Mountebank's rotating queue does (issue
    /// #1513), and its slot and cursor are kept. For an embedder that places recordings itself and
    /// would otherwise replay the merge as a replace, which serves them in display order. Returns
    /// `false` if no such id.
    #[must_use]
    pub fn append_stub_responses_by_id(&self, id: &str, responses: Vec<StubResponse>) -> bool {
        self.mutate_stubs(|stubs| {
            let Some(i) = stubs.iter().position(|s| s.stub.id.as_deref() == Some(id)) else {
                return false;
            };
            // Stamped like the engine's own recording (issue #1301): wall clock, never earlier
            // than the stub already served.
            let at = stubs[i].loaded_at().max(chrono::Utc::now());
            stubs[i] = Arc::new(self.appended(&stubs[i], responses).with_loaded_at(at));
            true
        })
    }

    /// After removing the stub at `i`, stamp the stubs that moved up into its place: they may now
    /// answer requests it answered (issue #1303). Nothing behind the last stub, nothing stamped.
    fn restamp_exposed(&self, stubs: &mut [Arc<StubState>], i: usize) {
        if i < stubs.len() {
            restamp(stubs, i.., self.load_clock.next());
        }
    }

    /// Delete the stub with `id`. Returns `false` if no such id.
    #[must_use]
    pub fn delete_stub_by_id(&self, id: &str) -> bool {
        self.mutate_stubs(|stubs| {
            match stubs.iter().position(|s| s.stub.id.as_deref() == Some(id)) {
                Some(i) => {
                    stubs.remove(i);
                    self.restamp_exposed(stubs, i);
                    true
                }
                None => false,
            }
        })
    }

    /// Get a clone of the stub with `id`, if present.
    pub fn get_stub_by_id(&self, id: &str) -> Option<Stub> {
        self.snapshot()
            .stubs()
            .iter()
            .find(|s| s.stub.id.as_deref() == Some(id))
            .map(|s| s.stub.clone())
    }

    /// Replace a stub at a specific index. The stub starts its response cycle over, even with
    /// identical content, as Mountebank's `overwriteAtIndex` rewraps it (#1515).
    pub fn replace_stub(&self, index: usize, stub: Stub) -> Result<(), ImposterError> {
        self.replace_stub_from(None, index, stub)
    }

    /// [`Self::replace_stub`] for a door that writes scripts: see [`Self::mutate_stubs_from`]
    /// (issue #1645).
    pub(crate) fn replace_stub_from(
        &self,
        door: Option<&crate::imposter::ScriptFileAccess>,
        index: usize,
        stub: Stub,
    ) -> Result<(), ImposterError> {
        self.mutate_stubs_from(door, |stubs| {
            if index >= stubs.len() {
                return Err(ImposterError::StubIndexOutOfBounds(index));
            }
            replace_at(stubs, index, stub, &self.load_clock, OnReplace::ResetCycle);
            Ok(())
        })
    }

    /// Delete a stub at a specific index
    pub fn delete_stub(&self, index: usize) -> Result<(), ImposterError> {
        self.mutate_stubs(|stubs| {
            if index >= stubs.len() {
                return Err(ImposterError::StubIndexOutOfBounds(index));
            }
            stubs.remove(index);
            self.restamp_exposed(stubs, index);
            Ok(())
        })
    }

    /// Give every stub a fresh response cycle (issue #1518), keeping its content, load stamp and
    /// recorded claim, as a Mountebank recreate does. A sequencer keyed by the old slots is the
    /// caller's to reset.
    pub(crate) fn restart_cycles(&self) {
        self.mutate_stubs(|stubs| {
            for state in stubs.iter_mut() {
                *state = Arc::new(state.with_fresh_cycle());
            }
        });
    }

    /// The imposter's config carrying the stubs it serves *now*. `config.stubs` is only what it
    /// was created with, so anything exporting the imposter — a replayable view, a delete's
    /// response (issues #1219, #1229) — reads this instead, or loses every stub added, replaced
    /// or recorded since.
    pub fn current_config(&self) -> ImposterConfig {
        ImposterConfig {
            stubs: self.get_stubs(),
            ..self.config.clone()
        }
    }

    /// Get all stubs
    pub fn get_stubs(&self) -> Vec<Stub> {
        self.snapshot()
            .stubs()
            .iter()
            .map(|stub_state| stub_state.stub.clone())
            .collect()
    }

    /// Number of stubs, without cloning them (list/summary endpoints poll this on a tick).
    pub fn stub_count(&self) -> usize {
        self.snapshot().stubs().len()
    }

    /// Get a specific stub by index
    pub fn get_stub(&self, index: usize) -> Option<Stub> {
        let snapshot = self.snapshot();
        let stubs = snapshot.stubs();
        stubs.get(index).map(|stub_state| stub_state.stub.clone())
    }

    /// Set enabled state
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// Check if enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }
}

/// Replace the stub at `i` with `stub`. Identical content keeps the state, so an admin replace that
/// changes nothing moves neither `Last-Modified: load` nor the load clock (issue #1294). Otherwise
/// the slot keeps its cycler and slot token (issue #287) and takes a stamp from `clock` (issue
/// #1301) — and when the new stub is eligible for different requests, so do the stubs behind it,
/// which may now answer requests it answered (issue #1303).
fn replace_at(
    stubs: &mut [Arc<StubState>],
    i: usize,
    stub: Stub,
    clock: &LoadClock,
    on_replace: OnReplace,
) {
    let hash = crate::imposter::reconcile::content_hash(&stub);
    if stubs[i].content_hash() == hash {
        if on_replace == OnReplace::ResetCycle {
            stubs[i] = Arc::new(stubs[i].with_fresh_cycle());
        }
        return;
    }
    let at = clock.next();
    let exposes = !matches_alike(&stubs[i].stub, &stub);
    let state = match on_replace {
        OnReplace::KeepCycle => stubs[i].with_stub_hashed(stub, hash),
        OnReplace::ResetCycle => StubState::with_hash(stub, hash),
    };
    stubs[i] = Arc::new(state.with_loaded_at(at));
    if exposes {
        restamp(stubs, i + 1.., at);
    }
}

/// What a replace does to the slot's response cycle (#1515).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnReplace {
    /// Mountebank's `overwriteAtIndex`: the stub starts its cycle over, even with identical content
    /// (a new slot, so a sequencer keyed by slot starts over too).
    ResetCycle,
    /// Rift's id-addressed replace: the slot keeps its cycler and slot token (#287), and an
    /// identical replace keeps the state outright (#1294).
    KeepCycle,
}
