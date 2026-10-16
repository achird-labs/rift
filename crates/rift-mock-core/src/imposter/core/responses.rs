//! Debug/preview inspection and response-mode (rift/proxy/inject) dispatch helpers.
//!
//! Part of the `Imposter` implementation; see `core/mod.rs` for the struct definition.

use super::*;

impl Imposter {
    /// Advance and return the stub's next response — via the registered sequencer when one
    /// is configured (issue #313), else the embedded per-stub cycler (today's hot path,
    /// untouched). `Err` = sequencer backend unavailable; callers surface it (#318).
    pub(crate) fn next_stub_response<'a>(
        &self,
        stub_state: &'a StubState,
    ) -> anyhow::Result<Option<&'a StubResponse>> {
        match &self.sequencer {
            None => Ok(stub_state.get_next_response()),
            Some(sequencer) => self.via_sequencer(stub_state, sequencer.as_ref(), true),
        }
    }

    /// Peek the stub's upcoming response without advancing — sequencer-aware like
    /// [`Self::next_stub_response`].
    pub(crate) fn peek_stub_response<'a>(
        &self,
        stub_state: &'a StubState,
    ) -> anyhow::Result<Option<&'a StubResponse>> {
        match &self.sequencer {
            None => Ok(stub_state.peek_response()),
            Some(sequencer) => self.via_sequencer(stub_state, sequencer.as_ref(), false),
        }
    }

    fn via_sequencer<'a>(
        &self,
        stub_state: &'a StubState,
        sequencer: &dyn crate::behaviors::ResponseSequencer,
        advance: bool,
    ) -> anyhow::Result<Option<&'a StubResponse>> {
        let responses = &stub_state.stub.responses;
        if responses.is_empty() {
            return Ok(None);
        }
        // Occurrence 0 is documented on SequenceKey; the content hash is cached on the state,
        // so this serializes the stub at most once per state, not once per decision (#1254).
        let stub_key = stub_state.sequence_key();
        let key = crate::behaviors::SequenceKey {
            port: self.config.port.unwrap_or(0),
            slot: stub_state.slot,
            stub_key: &stub_key,
            scope: stub_state.stub.space.as_deref().unwrap_or(""),
        };
        // After a mid-cycle `proxyAlways` append (#1513) the sequencer counts positions in the
        // stub's cycle, every one a single answer, and each position maps to a response.
        let cycle = stub_state.cycle();
        let (count, repeats) = sequence_shape(stub_state);
        let index = if advance {
            sequencer.next(key, count, &repeats)?
        } else {
            sequencer.peek(key, count, &repeats)?
        };
        let response = match cycle {
            None => responses.get(index),
            Some(cycle) => cycle.get(index).and_then(|&i| responses.get(i as usize)),
        };
        // An out-of-range index is a sequencer contract violation; surfacing it beats
        // silently falling through to the no-match default response (issue #313).
        let Some(response) = response else {
            anyhow::bail!(
                "sequencer returned out-of-range index {index} for stub {stub_key} ({count} responses)"
            );
        };
        Ok(Some(response))
    }

    /// `stub_state` with `extra` appended as a `proxyAlways` recording appends them (#1513):
    /// joining the tail of its current cycle. Without a sequencer, at the embedded cycler's cursor,
    /// which keeps pointing at the queue's head. With one, at its `peek` (the SPI has no repeat
    /// index, so the start of that response's repeat run), with the head at position 0: the
    /// sequencer's key changes with the content, so its cursor is restarted, by
    /// [`mutate_stubs`](Self::mutate_stubs), rather than relied on to carry over. A sequencer that
    /// cannot answer leaves the append at the cycle's start, the display order, and is logged: the
    /// recording itself must not be lost.
    pub(crate) fn appended(&self, stub_state: &StubState, extra: Vec<StubResponse>) -> StubState {
        let Some(sequencer) = &self.sequencer else {
            return stub_state.with_appended(extra, stub_state.cursor(), true);
        };
        let stub_key = stub_state.sequence_key();
        let key = crate::behaviors::SequenceKey {
            port: self.config.port.unwrap_or(0),
            slot: stub_state.slot,
            stub_key: &stub_key,
            scope: stub_state.stub.space.as_deref().unwrap_or(""),
        };
        let (count, repeats) = sequence_shape(stub_state);
        let cursor = match sequencer.peek(key, count, &repeats) {
            Ok(index) => (u32::try_from(index).unwrap_or(u32::MAX), 0),
            Err(e) => {
                tracing::error!(
                    "sequencer peek failed for stub {stub_key}; proxyAlways append served in display order: {e:#}"
                );
                (0, 0)
            }
        };
        stub_state.with_appended(extra, cursor, false)
    }

    /// Get all stubs info for debug purposes (Rift extension)
    pub fn get_all_stubs_info(&self) -> Vec<DebugStubInfo> {
        let snapshot = self.snapshot();
        let stubs = snapshot.stubs();
        stubs
            .iter()
            .map(|stub_state| &stub_state.stub)
            .enumerate()
            .map(|(index, stub)| DebugStubInfo {
                index,
                id: stub.id.clone(),
                predicates: stub.predicates.clone(),
                response_count: stub.responses.len(),
            })
            .collect()
    }

    /// Get imposter info for debug purposes (Rift extension)
    pub fn get_debug_imposter_info(&self) -> DebugImposter {
        let snapshot = self.snapshot();
        let stubs = snapshot.stubs();
        DebugImposter {
            port: self.config.port.unwrap_or(0),
            name: self.config.name.clone(),
            protocol: self.config.protocol.clone(),
            stub_count: stubs.len(),
        }
    }

    /// Create response preview from a stub (Rift extension)
    pub fn get_response_preview(
        &self,
        stub_state: &StubState,
    ) -> anyhow::Result<DebugResponsePreview> {
        // Get the current response from the cycler/sequencer
        if let Some(response) = self.peek_stub_response(stub_state)? {
            return Ok(create_response_preview(response));
        }

        Ok(DebugResponsePreview {
            response_type: "unknown".to_string(),
            status_code: None,
            headers: None,
            body_preview: None,
        })
    }

    /// Execute a stub and get the response with behaviors and rift extensions
    /// Returns (status, headers, body, behaviors, rift_extension, response_mode, is_fault)
    #[allow(clippy::type_complexity)]
    pub fn execute_stub_with_rift(
        &self,
        stub_state: &StubState,
    ) -> anyhow::Result<
        Option<(
            u16,
            HashMap<String, Vec<String>>,
            String,
            Option<std::sync::Arc<crate::behaviors::BehaviorProgram>>,
            Option<RiftResponseExtension>,
            ResponseMode,
            bool,
        )>,
    > {
        let Some(response) = self.next_stub_response(stub_state)? else {
            return Ok(None);
        };
        // This wrapper's own contract stays owned (it backs the debug/preview paths, not the
        // request hot path) — clone here rather than push a lifetime through its signature.
        Ok(execute_stub_response_with_rift(response).map(
            |(status, headers, body, behaviors, rift, mode, is_fault)| {
                (
                    status,
                    headers,
                    body,
                    behaviors,
                    rift.cloned(),
                    mode,
                    is_fault,
                )
            },
        ))
    }
}

/// The `(response_count, repeats)` a sequencer is handed for `stub_state`: the display list with
/// its `repeat`s, or the positions of a mid-cycle order with every repeat 1 (#1513).
fn sequence_shape(stub_state: &StubState) -> (usize, Vec<u32>) {
    match stub_state.cycle() {
        Some(cycle) => (cycle.len(), vec![1; cycle.len()]),
        None => (
            stub_state.stub.responses.len(),
            stub_state
                .stub
                .responses
                .iter()
                .map(|r| r.get_repeat().unwrap_or(1).max(1))
                .collect(),
        ),
    }
}
