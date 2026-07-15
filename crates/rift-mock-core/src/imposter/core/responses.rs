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
        // stub_key is computed per decision (not cached) because in-place replaces swap
        // `stub` under the same StubState; occurrence 0 is documented on SequenceKey.
        let stub_key = crate::imposter::reconcile::stub_key(&stub_state.stub, 0);
        let key = crate::behaviors::SequenceKey {
            port: self.config.port.unwrap_or(0),
            slot: stub_state.slot,
            stub_key: &stub_key,
            scope: stub_state.stub.space.as_deref().unwrap_or(""),
        };
        let repeats: Vec<u32> = responses
            .iter()
            .map(|r| r.get_repeat().unwrap_or(1).max(1))
            .collect();
        let index = if advance {
            sequencer.next(key, responses.len(), &repeats)?
        } else {
            sequencer.peek(key, responses.len(), &repeats)?
        };
        // An out-of-range index is a sequencer contract violation; surfacing it beats
        // silently falling through to the no-match default response (issue #313).
        let Some(response) = responses.get(index) else {
            anyhow::bail!(
                "sequencer returned out-of-range index {index} for stub {stub_key} ({} responses)",
                responses.len()
            );
        };
        Ok(Some(response))
    }

    /// Get all stubs info for debug purposes (Rift extension)
    pub fn get_all_stubs_info(&self) -> Vec<DebugStubInfo> {
        let stubs = self.stubs.load();
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
        let stubs = self.stubs.load();
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

    /// Convert hyper HeaderMap to HashMap<String, String>
    /// Uses Title-Case for header keys to match Mountebank's convention.
    pub(crate) fn header_map_to_hashmap(headers: &hyper::HeaderMap) -> HashMap<String, String> {
        headers
            .iter()
            .map(|(k, v)| {
                (
                    crate::behaviors::header_to_title_case(k.as_str()),
                    v.to_str().unwrap_or("").to_string(),
                )
            })
            .collect()
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
            Option<std::sync::Arc<crate::behaviors::ResponseBehaviors>>,
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
