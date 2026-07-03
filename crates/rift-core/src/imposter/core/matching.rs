//! Request matching, flow-id / scenario-state resolution, and form parsing.
//!
//! Part of the `Imposter` implementation; see `core/mod.rs` for the struct definition.

use super::*;

impl Imposter {
    /// Find a matching stub for a request and return a cloned copy with its index.
    /// `Err` means a backend consulted during matching failed (issue #318) — the caller
    /// must surface it, never treat it as "no match".
    pub fn find_matching_stub(
        &self,
        method: &str,
        path: &str,
        headers: &hyper::HeaderMap,
        query: Option<&str>,
        body: Option<&str>,
    ) -> anyhow::Result<Option<(StubState, usize)>> {
        // Call the extended version with no client info (backward compatible). This convenience
        // wrapper still accepts a `HeaderMap` and converts once; the hot path (`handler.rs`)
        // passes an already-built header map to `find_matching_stub_with_client` directly.
        let headers_map = Self::header_map_to_hashmap(headers);
        self.find_matching_stub_with_client(method, path, &headers_map, query, body, None, None)
    }

    /// Find a matching stub with client address information (for requestFrom/ip predicates)
    #[allow(clippy::too_many_arguments)]
    pub fn find_matching_stub_with_client(
        &self,
        method: &str,
        path: &str,
        headers_map: &HashMap<String, String>,
        query: Option<&str>,
        body: Option<&str>,
        request_from: Option<&str>,
        client_ip: Option<&str>,
    ) -> anyhow::Result<Option<(StubState, usize)>> {
        let stubs = self.stubs.read();
        // `headers_map` is the single-value, Title-Case header view already built once by the
        // caller (#288) — no re-conversion from `HeaderMap` here.
        // Parse form data if Content-Type is application/x-www-form-urlencoded
        let form = Self::parse_form_data(headers_map, body);

        let imposter_port = self.config.port.unwrap_or(0);
        let flow_id = self.resolve_flow_id(headers_map);
        // Parse the request body as JSON once per request and reuse it across every stub's
        // predicates, instead of re-parsing per predicate per stub (issue #290).
        let body_json = body.and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok());
        for (index, stub_state) in stubs.iter().enumerate() {
            let stub = &stub_state.stub;
            // Correlated-isolation gate (issue #223, runs first): a space-scoped stub only
            // participates in matching when the request's resolved flow_id equals its space.
            // Unscoped stubs match any space (PerInstance default).
            if let Some(space) = &stub.space
                && flow_id != *space
            {
                continue;
            }
            // Scenario FSM eligibility gate (before predicate precedence): a stub guarded by
            // `requiredScenarioState` only participates in matching when the current
            // (flow_id, scenario) state equals it.
            if let Some(required) = &stub.required_scenario_state {
                let scenario = stub.scenario_name.as_deref().unwrap_or("");
                if self.scenario_state(&flow_id, scenario)? != *required {
                    continue;
                }
            }
            if stub_matches_inner(
                &stub.predicates,
                method,
                path,
                query,
                headers_map,
                body,
                request_from,
                client_ip,
                form.as_ref(),
                imposter_port,
                body_json.as_ref(),
            ) {
                // PERF NOTE: this deep-clones the matched `Stub` on every request. The clone is
                // load-bearing, not accidental: the caller (`handler.rs`) holds the returned
                // `StubState` across `.await` points (async proxying / response building), so the
                // `parking_lot` read guard held here MUST be released before returning — a guard
                // cannot be held across an await without blocking all writers for the request's
                // lifetime. Returning only `index` would force the caller to re-lock (racy against
                // concurrent `replace_stub`) or hold the guard across await. The response-cycling
                // state is already shared cheaply: `StubState.cycler` is an `Arc`, so advancing the
                // cycler on this clone is visible through the stored stub. Eliminating the `Stub`
                // clone cleanly would mean making `StubState.stub` an `Arc<Stub>` — a broader field
                // retype across every `.stub` access/mutation site, deferred out of this
                // behavior-preserving pass.
                return Ok(Some((stub_state.clone(), index)));
            }
        }
        Ok(None)
    }

    /// The configured `flow_id_source` (`"imposter_port"` or `"header:<Name>"`),
    /// defaulting to `"imposter_port"`.
    pub fn flow_id_source(&self) -> String {
        self.config
            .rift
            .as_ref()
            .and_then(|r| r.flow_state.as_ref())
            .and_then(|fs| fs.flow_id_source.clone())
            .unwrap_or_else(|| "imposter_port".to_string())
    }

    /// Resolve the correlation `flow_id` for a request, partitioning scenario state.
    /// `"header:<Name>"` uses that (case-insensitive) header; `"imposter_port"` (the default,
    /// and the fallback when the header is absent) uses the imposter port.
    pub fn resolve_flow_id(&self, headers: &HashMap<String, String>) -> String {
        // Live path uses the single-value header view (`headers_clone`); kept separate from the
        // multi-value `flow_id_for` (used over recorded requests) to avoid a per-request alloc.
        let port = self.config.port.unwrap_or(0);
        match self.flow_id_source().strip_prefix("header:") {
            Some(name) => headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| port.to_string()),
            None => port.to_string(),
        }
    }

    /// Resolve the correlation `flow_id` for an already-recorded request (multi-value headers).
    pub fn resolve_flow_id_recorded(&self, headers: &HashMap<String, Vec<String>>) -> String {
        Self::flow_id_for(
            &self.flow_id_source(),
            headers,
            self.config.port.unwrap_or(0),
        )
    }

    /// Pure flow_id resolution (no `&self`), so it can be reused over recorded requests.
    /// A flow id derives from a single header value; the first is taken if multi-valued (#238).
    fn flow_id_for(source: &str, headers: &HashMap<String, Vec<String>>, port: u16) -> String {
        match source.strip_prefix("header:") {
            Some(name) => headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .and_then(|(_, v)| v.first().cloned())
                .unwrap_or_else(|| port.to_string()),
            None => port.to_string(),
        }
    }

    /// Current scenario state for `(flow_id, scenario)`, or the initial state if absent.
    /// A backend read error propagates (issue #318): defaulting to the initial state on a
    /// failing store would mis-gate matching into a silent wrong match.
    pub fn scenario_state(&self, flow_id: &str, scenario: &str) -> anyhow::Result<String> {
        match self.flow_store.get(flow_id, scenario)? {
            // A non-string here means the key was overwritten out-of-band (raw flow-state
            // PUT): coercing it to the initial state would silently mis-gate matching.
            Some(v) => match v.as_str() {
                Some(state) => Ok(state.to_string()),
                None => {
                    anyhow::bail!("scenario state for {flow_id}/{scenario} is not a string: {v}")
                }
            },
            None => Ok(INITIAL_SCENARIO_STATE.to_string()),
        }
    }

    /// Set scenario state for `(flow_id, scenario)`.
    pub fn set_scenario_state(
        &self,
        flow_id: &str,
        scenario: &str,
        state: &str,
    ) -> anyhow::Result<()> {
        self.flow_store.set(
            flow_id,
            scenario,
            serde_json::Value::String(state.to_string()),
        )
    }

    /// Delete a scenario's state for a flow (so it reads back as the initial state).
    pub fn delete_scenario_state(&self, flow_id: &str, scenario: &str) -> anyhow::Result<()> {
        self.flow_store.delete(flow_id, scenario)
    }

    /// Apply a matched stub's `newScenarioState` transition (no-op if unset). A backend
    /// write error propagates (issue #318): a lost transition would silently desync the
    /// FSM, so the request must fail loudly instead.
    ///
    /// A gated stub (`requiredScenarioState`) transitions via compare-and-set expecting
    /// the state its gate observed (issue #311): if the state moved underneath — a
    /// concurrent request won — the stale write is dropped rather than clobbering the
    /// newer state. Conflict is normal concurrency, not an error. Ungated stubs keep
    /// today's unconditional overwrite (there is no gate read to race against).
    pub fn apply_scenario_transition(&self, flow_id: &str, stub: &Stub) -> anyhow::Result<()> {
        use crate::extensions::flow_state::CasOutcome;

        let Some(next) = &stub.new_scenario_state else {
            return Ok(());
        };
        let scenario = stub.scenario_name.as_deref().unwrap_or("");
        let Some(required) = &stub.required_scenario_state else {
            return self.set_scenario_state(flow_id, scenario, next);
        };

        let new_value = serde_json::Value::String(next.to_string());
        let expected = serde_json::Value::String(required.to_string());
        match self.flow_store.compare_and_set(
            flow_id,
            scenario,
            Some(&expected),
            new_value.clone(),
        )? {
            CasOutcome::Applied => Ok(()),
            // The initial state is normally stored as ABSENCE — retry expecting that
            // representation before concluding the state moved.
            CasOutcome::Conflict(None) if required == INITIAL_SCENARIO_STATE => {
                match self
                    .flow_store
                    .compare_and_set(flow_id, scenario, None, new_value)?
                {
                    CasOutcome::Applied => Ok(()),
                    CasOutcome::Conflict(current) => {
                        Self::log_dropped_transition(flow_id, scenario, required, next, &current);
                        Ok(())
                    }
                }
            }
            CasOutcome::Conflict(current) => {
                Self::log_dropped_transition(flow_id, scenario, required, next, &current);
                Ok(())
            }
        }
    }

    /// A dropped transition is correct behavior but must not be invisible: without this,
    /// "my scenario stopped advancing" under concurrency has zero diagnostic signal.
    fn log_dropped_transition(
        flow_id: &str,
        scenario: &str,
        required: &str,
        next: &str,
        current: &Option<serde_json::Value>,
    ) {
        debug!(
            "scenario transition dropped ({flow_id}/{scenario} {required} -> {next}): \
             state moved underneath, current {current:?}"
        );
    }

    /// Read a raw flow-state value (admin flow-state inspection).
    pub fn flow_get(&self, flow_id: &str, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
        self.flow_store.get(flow_id, key)
    }

    /// Set a raw flow-state value (admin flow-state arrange).
    pub fn flow_set(
        &self,
        flow_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.flow_store.set(flow_id, key, value)
    }

    /// Delete a raw flow-state value (admin flow-state teardown).
    pub fn flow_delete(&self, flow_id: &str, key: &str) -> anyhow::Result<()> {
        self.flow_store.delete(flow_id, key)
    }

    /// Distinct scenario names declared by this imposter's stubs (sorted).
    pub fn scenario_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .stubs
            .read()
            .iter()
            .filter_map(|s| s.stub.scenario_name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Stubs scoped to a given correlation space (issue #223).
    pub fn space_stubs(&self, space: &str) -> Vec<Stub> {
        self.stubs
            .read()
            .iter()
            .filter(|s| s.stub.space.as_deref() == Some(space))
            .map(|s| s.stub.clone())
            .collect()
    }

    /// Tear down a correlation space (issue #223): remove its scoped stubs, drop its recorded
    /// requests, and reset its named scenario states. Other spaces and the port are untouched.
    pub fn teardown_space(&self, space: &str) -> anyhow::Result<()> {
        // Snapshot scenario names BEFORE pruning stubs: a scenario declared only on this space's
        // stubs would otherwise vanish from scenario_names() and its state would never be reset.
        let scenarios = self.scenario_names();
        self.stubs
            .write()
            .retain(|s| s.stub.space.as_deref() != Some(space));
        // Best-effort across the slice's clears so one failure doesn't leave later scenarios
        // stale, but the first failure still surfaces (issues #318, #330) — never report a
        // clean teardown while stale recorded requests or scenario state persist in the backend.
        let mut first_err = None;
        if let Err(e) = self.journal.clear_flow(self.journal_port(), space) {
            warn!("space teardown: failed to clear recorded requests for '{space}': {e}");
            first_err.get_or_insert(e);
        }
        for scenario in scenarios {
            if let Err(e) = self.delete_scenario_state(space, &scenario) {
                warn!("space teardown: failed to reset scenario '{scenario}' for '{space}': {e}");
                first_err.get_or_insert(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Parse form-urlencoded data from body if Content-Type matches
    pub(crate) fn parse_form_data(
        headers: &HashMap<String, String>,
        body: Option<&str>,
    ) -> Option<HashMap<String, String>> {
        // Header keys are Title-Case in the pre-built map, so match Content-Type case-insensitively
        // (HeaderMap lookups were case-insensitive; preserve that).
        let content_type = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");

        if content_type.contains("application/x-www-form-urlencoded")
            && let Some(body_str) = body
        {
            let mut map = HashMap::new();
            for pair in body_str.split('&').filter(|s| !s.is_empty()) {
                let mut parts = pair.splitn(2, '=');
                if let Some(raw_key) = parts.next() {
                    let key = urlencoding::decode(raw_key)
                        .unwrap_or_default()
                        .into_owned();
                    let value = parts
                        .next()
                        .map(|v| urlencoding::decode(v).unwrap_or_default().into_owned())
                        .unwrap_or_default();
                    map.entry(key)
                        .and_modify(|existing: &mut String| {
                            existing.push(',');
                            existing.push_str(&value);
                        })
                        .or_insert(value);
                }
            }
            return Some(map);
        }
        None
    }
}
