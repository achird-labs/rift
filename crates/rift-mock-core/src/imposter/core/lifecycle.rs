//! Stub lifecycle (add/replace/delete/query) and enabled-state management.
//!
//! Part of the `Imposter` implementation; see `core/mod.rs` for the struct definition.

use super::*;
use crate::imposter::reconcile::{StubReconcile, reconcile_stub_states};

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
            let state = stubs.remove(from);
            stubs.insert(to, state);
            Ok(())
        })
    }

    /// Reconcile live stubs toward `desired` under one write critical section (issue #316).
    pub(crate) fn reconcile_stubs(&self, desired: Vec<Stub>) -> StubReconcile {
        self.mutate_stubs(|stubs| reconcile_stub_states(stubs, desired))
    }

    /// Add a stub at a specific index
    pub fn add_stub(&self, stub: Stub, index: Option<usize>) {
        self.mutate_stubs(|stubs| {
            let idx = index.unwrap_or(stubs.len()).min(stubs.len());
            stubs.insert(idx, Arc::new(StubState::new(stub)));
        });
    }

    /// Add a stub, rejecting it if its `id` duplicates an existing stub (issue #202). The
    /// duplicate check and the insert happen under one write critical section so concurrent adds
    /// can't race. Returns `false` (not added) on id conflict — the caller holds the id for the error.
    #[must_use]
    pub fn add_stub_unique(&self, stub: Stub, index: Option<usize>) -> bool {
        self.mutate_stubs(|stubs| {
            if let Some(id) = stub.id.as_deref()
                && stubs.iter().any(|s| s.stub.id.as_deref() == Some(id))
            {
                return false;
            }
            let idx = index.unwrap_or(stubs.len()).min(stubs.len());
            stubs.insert(idx, Arc::new(StubState::new(stub)));
            true
        })
    }

    /// Replace the stub with `id` in place (position preserved). The replacement keeps `id` as its
    /// addressable id regardless of the supplied stub's `id`. Returns `false` if no such id.
    #[must_use]
    pub fn replace_stub_by_id(&self, id: &str, mut stub: Stub) -> bool {
        self.mutate_stubs(|stubs| {
            match stubs.iter().position(|s| s.stub.id.as_deref() == Some(id)) {
                Some(i) => {
                    stub.id = Some(id.to_string());
                    // Swap a fresh Arc that reuses the slot's cycler + slot token, so the slot's
                    // response-cycling state is kept and in-flight requests holding the old Arc keep
                    // serving their snapshot (issue #287).
                    stubs[i] = Arc::new(stubs[i].with_stub(stub));
                    true
                }
                None => false,
            }
        })
    }

    /// Delete the stub with `id`. Returns `false` if no such id.
    #[must_use]
    pub fn delete_stub_by_id(&self, id: &str) -> bool {
        self.mutate_stubs(|stubs| {
            match stubs.iter().position(|s| s.stub.id.as_deref() == Some(id)) {
                Some(i) => {
                    stubs.remove(i);
                    true
                }
                None => false,
            }
        })
    }

    /// Get a clone of the stub with `id`, if present.
    pub fn get_stub_by_id(&self, id: &str) -> Option<Stub> {
        self.stubs
            .load()
            .iter()
            .find(|s| s.stub.id.as_deref() == Some(id))
            .map(|s| s.stub.clone())
    }

    /// Replace a stub at a specific index
    pub fn replace_stub(&self, index: usize, stub: Stub) -> Result<(), ImposterError> {
        self.mutate_stubs(|stubs| {
            if index >= stubs.len() {
                return Err(ImposterError::StubIndexOutOfBounds(index));
            }
            // Reuse the slot's cycler + slot token (issue #287); see `replace_stub_by_id`.
            stubs[index] = Arc::new(stubs[index].with_stub(stub));
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
            Ok(())
        })
    }

    /// Get all stubs
    pub fn get_stubs(&self) -> Vec<Stub> {
        self.stubs
            .load()
            .iter()
            .map(|stub_state| stub_state.stub.clone())
            .collect()
    }

    /// Get a specific stub by index
    pub fn get_stub(&self, index: usize) -> Option<Stub> {
        let stubs = self.stubs.load();
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
