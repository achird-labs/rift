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
    ) -> anyhow::Result<Option<(Arc<StubState>, usize)>> {
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
    ) -> anyhow::Result<Option<(Arc<StubState>, usize)>> {
        // Stage 1 (issue #292): the index embeds its stub snapshot, so `stubs` and the candidate
        // set are always consistent. Non-anchored stubs sit in its fallback bucket, so `candidates`
        // is a superset of the true matches, ascending — Stage-2 first-match-wins is unchanged.
        // Note: a path-anchored stub whose anchor doesn't match the request is no longer visited, so
        // its `required_scenario_state` backend read (which could `Err`, #318) is skipped for that
        // request — correct, since a stub that provably can't match should not be consulted.
        let snapshot = self.stub_index.load();
        let stubs = snapshot.stubs();
        // `headers_map` is the single-value, Title-Case header view already built once by the
        // caller (#288) — no re-conversion from `HeaderMap` here.
        // Parse form data if Content-Type is application/x-www-form-urlencoded
        let form = Self::parse_form_data(headers_map, body);

        let imposter_port = self.script_state_key();
        let flow_id = self.resolve_flow_id(headers_map);
        // Parse the request body as JSON once per request and reuse it across every stub's
        // predicates, instead of re-parsing per predicate per stub (issue #290).
        let body_json = body.and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok());
        // Likewise parse the query string once per request rather than per predicate (issue #480).
        let query_map = crate::imposter::predicates::parse_query(query);
        for stub_idx in snapshot.candidates(path) {
            let stub_state = &stubs[stub_idx];
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
                Some(&query_map),
            )? {
                // Bump the refcount instead of deep-cloning the whole `StubState` (issue #287).
                // The caller (`handler.rs`) holds the returned `Arc<StubState>` across `.await`
                // points, so the arc-swap load guard must be released before returning (issue #291);
                // the `Arc` lets it do so without a copy. Response-cycling state stays shared: the
                // `cycler` is itself an `Arc`, so advancing it via this handle is visible through
                // the stored stub, and an in-place replace swaps a new `Arc` while in-flight
                // requests keep serving their snapshot.
                return Ok(Some((Arc::clone(stub_state), stub_idx)));
            }
        }
        Ok(None)
    }

    /// As [`Self::find_matching_stub_with_client`], but bounded (issue #476): when the stub
    /// snapshot contains an `inject` predicate — synchronous Boa JavaScript evaluated deep inside
    /// the matcher — the whole matching pass runs on `spawn_blocking` under a wall-clock deadline,
    /// so a slow or runaway predicate script cannot stall a tokio worker. Scriptless snapshots
    /// (the overwhelmingly common case, gated by the precomputed `StubIndex::has_inject` flag)
    /// take the exact inline path — no clones, no blocking-pool hop, no deadline.
    ///
    /// No abort flag: Boa has no per-instruction interrupt, so after a timeout the loop-iteration
    /// cap (issue #327) is what eventually frees the blocking thread.
    #[allow(clippy::too_many_arguments)]
    pub async fn find_matching_stub_with_client_bounded(
        self: &Arc<Self>,
        method: &str,
        path: &str,
        headers_map: &HashMap<String, String>,
        query: Option<&str>,
        body: Option<&str>,
        request_from: Option<&str>,
        client_ip: Option<&str>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Option<(Arc<StubState>, usize)>> {
        let snapshot = self.stub_index.load();
        let has_inject = snapshot.has_inject();
        // A scenario-gated stub reads flow state inside the matching pass; on a blocking backend
        // (Redis) that read must not run on the tokio worker either (issue #475). A scenario-free
        // snapshot on a blocking backend still takes the inline fast path — no gate read happens.
        let needs_offload =
            has_inject || (snapshot.has_scenario_gate() && self.flow_store.is_blocking());
        drop(snapshot);
        if !needs_offload {
            return self.find_matching_stub_with_client(
                method,
                path,
                headers_map,
                query,
                body,
                request_from,
                client_ip,
            );
        }

        let this = Arc::clone(self);
        let method = method.to_string();
        let path = path.to_string();
        let headers_map = headers_map.clone();
        let query = query.map(str::to_string);
        let body = body.map(str::to_string);
        let request_from = request_from.map(str::to_string);
        let client_ip = client_ip.map(str::to_string);
        let handle = tokio::task::spawn_blocking(move || {
            this.find_matching_stub_with_client(
                &method,
                &path,
                &headers_map,
                query.as_deref(),
                body.as_deref(),
                request_from.as_deref(),
                client_ip.as_deref(),
            )
        });
        // The wall-clock deadline exists to bound a runaway inject *script*. A blocking flow-store
        // read is bounded by the backend's own connection/command timeout, so when the offload is
        // purely for the scenario gate (no inject) we await the task without the script deadline.
        if !has_inject {
            return match handle.await {
                Ok(result) => result,
                Err(join_err) => Err(anyhow::anyhow!("matching task panicked: {join_err}")),
            };
        }
        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(result)) => result,
            Ok(Err(join_err)) => Err(anyhow::anyhow!("matching task panicked: {join_err}")),
            Err(_elapsed) => {
                tracing::warn!(
                    "predicate inject matching timed out after {}ms",
                    timeout.as_millis()
                );
                // This route is only taken when the snapshot contains an inject predicate, so
                // the deadline firing is attributable to predicate injection — shape it as
                // `ScriptTimeoutError` (issue #499) so the handler serves a 504 that a client can
                // tell apart from a genuinely broken predicate (which stays a Mountebank-style 400).
                Err(crate::scripting::ScriptTimeoutError {
                    hook: "predicate inject",
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                }
                .into())
            }
        }
    }

    /// Reference implementation: the pre-#292 linear scan over *all* stubs. Shares every request
    /// derivation and gate with the indexed path above; only the iteration differs. Used solely by
    /// the differential test to prove the index preserves Mountebank first-match-wins exactly.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn find_matching_stub_linear(
        &self,
        method: &str,
        path: &str,
        headers_map: &HashMap<String, String>,
        query: Option<&str>,
        body: Option<&str>,
        request_from: Option<&str>,
        client_ip: Option<&str>,
    ) -> anyhow::Result<Option<(Arc<StubState>, usize)>> {
        let stubs = self.stubs.load();
        let form = Self::parse_form_data(headers_map, body);
        let imposter_port = self.script_state_key();
        let flow_id = self.resolve_flow_id(headers_map);
        let body_json = body.and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok());
        let query_map = crate::imposter::predicates::parse_query(query);
        for (index, stub_state) in stubs.iter().enumerate() {
            let stub = &stub_state.stub;
            if let Some(space) = &stub.space
                && flow_id != *space
            {
                continue;
            }
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
                Some(&query_map),
            )? {
                return Ok(Some((Arc::clone(stub_state), index)));
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

    /// Run a blocking flow-store closure off the tokio worker when the backend actually blocks
    /// (Redis), otherwise inline. This keeps a slow or pool-exhausted backend from
    /// head-of-line-blocking the worker thread every request is multiplexed on (issue #475),
    /// while adding zero overhead for the non-blocking in-memory store (the common case — the
    /// closure runs directly on the caller with no task hop).
    pub(crate) async fn run_flow_blocking<T, F>(self: &Arc<Self>, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Self) -> anyhow::Result<T> + Send + 'static,
    {
        if self.flow_store.is_blocking() {
            let imp = Arc::clone(self);
            tokio::task::spawn_blocking(move || f(&imp))
                .await
                .map_err(|e| anyhow::anyhow!("flow-store task panicked: {e}"))?
        } else {
            f(self)
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

    /// Clear every key under a flow (issue #530) — backs `DELETE /admin/imposters/:port/flow-state/
    /// :flow_id`. Idempotent: clearing an absent flow succeeds.
    pub fn flow_clear(&self, flow_id: &str) -> anyhow::Result<()> {
        self.flow_store.clear_flow(flow_id)
    }

    /// Distinct scenario names declared by this imposter's stubs (sorted).
    pub fn scenario_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .stubs
            .load()
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
            .load()
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
        self.mutate_stubs(|stubs| stubs.retain(|s| s.stub.space.as_deref() != Some(space)));
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
                    let key = crate::util::decode_or_raw(raw_key);
                    let value = parts
                        .next()
                        .map(crate::util::decode_or_raw)
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

// =============================================================================================
// Issue #476: predicate `inject` runs deep inside the synchronous matcher, so the bounded path
// wraps the WHOLE matching pass in spawn_blocking + timeout — but only for imposters whose stub
// set actually contains an inject predicate (StubIndex::has_inject), so scriptless imposters
// keep the exact inline fast path.
// =============================================================================================
#[cfg(test)]
mod bounded_matching_tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn imposter(stubs: serde_json::Value) -> Arc<Imposter> {
        let cfg = serde_json::from_value(json!({ "port": 0, "protocol": "http", "stubs": stubs }))
            .expect("valid imposter config");
        Arc::new(Imposter::new(cfg).expect("test imposter"))
    }

    fn no_headers() -> HashMap<String, String> {
        HashMap::new()
    }

    // Issue #611: an undecodable percent-sequence in a form body must pass through raw rather than
    // blank the key or value — the same decode convention the rest of the repo follows.
    #[test]
    fn parse_form_data_passes_through_undecodable_sequences() {
        let headers = HashMap::from([(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        )]);

        let form = Imposter::parse_form_data(&headers, Some("k=%FF")).expect("form parsed");
        assert_eq!(
            form.get("k"),
            Some(&"%FF".to_string()),
            "an undecodable value must pass through raw, not become empty"
        );

        let form = Imposter::parse_form_data(&headers, Some("%FF=v")).expect("form parsed");
        assert_eq!(
            form.get("%FF"),
            Some(&"v".to_string()),
            "an undecodable key must pass through raw, not collapse to an empty key"
        );

        let form = Imposter::parse_form_data(&headers, Some("k=hello%20world")).expect("parsed");
        assert_eq!(
            form.get("k"),
            Some(&"hello world".to_string()),
            "valid sequences must still decode"
        );
    }

    // Issue #475: run_flow_blocking must be transparent — on the default (non-blocking) backend it
    // runs the closure inline and returns its result faithfully (Ok value, Err propagated), and it
    // hands the closure a usable &Imposter so a real flow-store call works through it.
    #[tokio::test]
    async fn run_flow_blocking_is_transparent_inline() {
        let imp = imposter(json!([]));
        assert!(
            !imp.flow_store.is_blocking(),
            "default backend is non-blocking"
        );

        let value = imp
            .run_flow_blocking(|_| Ok(42_i64))
            .await
            .expect("closure ok");
        assert_eq!(value, 42);

        let err = imp
            .run_flow_blocking(|_| Err::<i64, _>(anyhow::anyhow!("boom")))
            .await
            .expect_err("closure err propagates");
        assert!(err.to_string().contains("boom"));

        // The closure receives a working &Imposter: an unset scenario reads the initial state.
        let state = imp
            .run_flow_blocking(|i| i.scenario_state("flow-1", "sc"))
            .await
            .expect("scenario_state through helper");
        assert_eq!(state, INITIAL_SCENARIO_STATE);
    }

    // Issue #480: the query is parsed once per request in the hot path (find_matching_stub_with_client)
    // and threaded into predicate matching. Drive a VALUE-based query predicate through that real
    // entry point to prove the threaded map carries the right keys AND values — not merely presence.
    #[test]
    fn hot_path_value_query_predicate_matches() {
        let imp = imposter(json!([
            { "predicates": [{ "equals": { "query": { "status": "active" } } }],
              "responses": [{ "is": { "statusCode": 200 } }] }
        ]));
        let headers = no_headers();

        let hit = imp
            .find_matching_stub_with_client(
                "GET",
                "/x",
                &headers,
                Some("status=active"),
                None,
                None,
                None,
            )
            .expect("no backend error");
        assert_eq!(
            hit.map(|(_, i)| i),
            Some(0),
            "a value-based query predicate must match via the once-per-request hoisted query map"
        );

        let miss = imp
            .find_matching_stub_with_client(
                "GET",
                "/x",
                &headers,
                Some("status=inactive"),
                None,
                None,
                None,
            )
            .expect("no backend error");
        assert!(
            miss.is_none(),
            "the hoisted map must carry query VALUES, not just keys — a wrong value must not match"
        );
    }

    /// A FlowStore that reports `is_blocking() == true` (delegating storage to an in-memory store)
    /// so the spawn_blocking dispatch path and the blocking-backend offload decision are exercised
    /// without a real Redis (issue #475).
    struct BlockingProbeStore {
        inner: crate::backends::inmemory::InMemoryFlowStore,
    }

    impl BlockingProbeStore {
        fn new() -> Self {
            Self {
                inner: crate::backends::inmemory::InMemoryFlowStore::new(300),
            }
        }
    }

    impl crate::extensions::flow_state::FlowStore for BlockingProbeStore {
        fn is_blocking(&self) -> bool {
            true
        }
        fn get(&self, flow_id: &str, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
            self.inner.get(flow_id, key)
        }
        fn set(&self, flow_id: &str, key: &str, value: serde_json::Value) -> anyhow::Result<()> {
            self.inner.set(flow_id, key, value)
        }
        fn exists(&self, flow_id: &str, key: &str) -> anyhow::Result<bool> {
            self.inner.exists(flow_id, key)
        }
        fn delete(&self, flow_id: &str, key: &str) -> anyhow::Result<()> {
            self.inner.delete(flow_id, key)
        }
        fn increment(&self, flow_id: &str, key: &str) -> anyhow::Result<i64> {
            self.inner.increment(flow_id, key)
        }
        fn set_ttl(&self, flow_id: &str, ttl_seconds: i64) -> anyhow::Result<()> {
            self.inner.set_ttl(flow_id, ttl_seconds)
        }
        fn compare_and_set(
            &self,
            flow_id: &str,
            key: &str,
            expected: Option<&serde_json::Value>,
            new: serde_json::Value,
        ) -> anyhow::Result<crate::extensions::flow_state::CasOutcome> {
            self.inner.compare_and_set(flow_id, key, expected, new)
        }
    }

    fn imposter_with_store(
        stubs: serde_json::Value,
        store: Arc<dyn crate::extensions::flow_state::FlowStore>,
    ) -> Arc<Imposter> {
        let cfg = serde_json::from_value(json!({ "port": 0, "protocol": "http", "stubs": stubs }))
            .expect("valid imposter config");
        let mut imp = Imposter::new(cfg).expect("test imposter");
        imp.flow_store = store;
        Arc::new(imp)
    }

    // Issue #475: on a blocking backend, run_flow_blocking must dispatch the closure to a
    // spawn_blocking pool thread (off the caller) and still round-trip its result / propagate a
    // panic as a JoinError-shaped error. This covers the spawn_blocking arm the inline test can't.
    #[tokio::test]
    async fn run_flow_blocking_dispatches_off_thread_on_blocking_backend() {
        let imp = imposter_with_store(json!([]), Arc::new(BlockingProbeStore::new()));
        assert!(imp.flow_store.is_blocking());

        let caller_thread = std::thread::current().id();
        let ran_on = imp
            .run_flow_blocking(|_| Ok(std::thread::current().id()))
            .await
            .expect("ok");
        assert_ne!(
            ran_on, caller_thread,
            "a blocking backend must run the closure off the caller thread"
        );

        assert_eq!(imp.run_flow_blocking(|_| Ok(7_i64)).await.expect("ok"), 7);

        let err = imp
            .run_flow_blocking(|_| -> anyhow::Result<i64> { panic!("boom-in-task") })
            .await
            .expect_err("panic in the blocking task must surface as an error");
        assert!(
            err.to_string().contains("flow-store task panicked"),
            "got: {err}"
        );
    }

    // Issue #475: a scenario-gated stub on a blocking backend must still match — the bounded
    // matcher's `has_scenario_gate && is_blocking` decision offloads the whole pass (incl. the gate
    // read) to spawn_blocking, taking the no-deadline branch. This is the crux the fix protects.
    #[tokio::test]
    async fn scenario_gated_stub_matches_through_blocking_offload() {
        let imp = imposter_with_store(
            json!([{
                "predicates": [{ "equals": { "path": "/x" } }],
                "scenarioName": "sc",
                "requiredScenarioState": INITIAL_SCENARIO_STATE,
                "responses": [{ "is": { "statusCode": 200 } }]
            }]),
            Arc::new(BlockingProbeStore::new()),
        );
        assert!(imp.stub_index.load().has_scenario_gate());
        assert!(imp.flow_store.is_blocking());

        let matched = imp
            .find_matching_stub_with_client_bounded(
                "GET",
                "/x",
                &no_headers(),
                None,
                None,
                None,
                None,
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("matching ok");
        assert!(
            matched.is_some(),
            "scenario-gated stub at its initial state must match via the blocking offload path"
        );
    }

    // AC3 (happy): an inject predicate matches/misses correctly through the bounded path.
    #[cfg(feature = "javascript")]
    #[tokio::test]
    async fn inject_predicate_matching_bounded_matches() {
        let imp = imposter(json!([{
            "predicates": [{ "inject": "function (config) { return config.request.path === '/hit'; }" }],
            "responses": [{ "is": { "statusCode": 200 } }]
        }]));
        let matched = imp
            .find_matching_stub_with_client_bounded(
                "GET",
                "/hit",
                &no_headers(),
                None,
                None,
                None,
                None,
                Duration::from_millis(60_000),
            )
            .await
            .expect("matcher must not error");
        assert!(
            matched.is_some(),
            "inject predicate returning true must match"
        );

        let missed = imp
            .find_matching_stub_with_client_bounded(
                "GET",
                "/miss",
                &no_headers(),
                None,
                None,
                None,
                None,
                Duration::from_millis(60_000),
            )
            .await
            .expect("matcher must not error");
        assert!(
            missed.is_none(),
            "inject predicate returning false must not match"
        );
    }

    // AC3: a runaway inject predicate times out near the deadline instead of blocking a
    // runtime worker for its full duration.
    #[cfg(feature = "javascript")]
    #[tokio::test]
    async fn inject_predicate_matching_times_out() {
        let imp = imposter(json!([{
            "predicates": [{ "inject": "function (config) { var i = 0; while (i < 100000000) { i += 1; } return true; }" }],
            "responses": [{ "is": { "statusCode": 200 } }]
        }]));
        let start = std::time::Instant::now();
        let res = imp
            .find_matching_stub_with_client_bounded(
                "GET",
                "/hang",
                &no_headers(),
                None,
                None,
                None,
                None,
                Duration::from_millis(25),
            )
            .await;
        let Err(err) = res else {
            panic!("a runaway inject predicate must error, not hang the matching pass")
        };
        let timeout = err
            .downcast_ref::<crate::scripting::ScriptTimeoutError>()
            .unwrap_or_else(|| {
                panic!(
                    "a matching timeout must be shaped as ScriptTimeoutError so the handler \
                     serves a 504 the client can tell apart from a broken predicate; got: {err}"
                )
            });
        assert_eq!(timeout.hook, "predicate inject");
        assert_eq!(timeout.timeout_ms, 25, "reports the configured deadline");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "must return near the configured deadline, not after the loop cap"
        );
    }

    // AC3 (fast path): a scriptless imposter never routes through spawn_blocking — the gate is
    // the precomputed has_inject flag, and matching succeeds through the bounded entry point.
    #[tokio::test]
    async fn scriptless_matching_bounded_stays_inline() {
        let imp = imposter(json!([{
            "predicates": [{ "equals": { "path": "/plain" } }],
            "responses": [{ "is": { "statusCode": 200 } }]
        }]));
        assert!(
            !imp.stub_index.load().has_inject(),
            "a scriptless stub set must not set the has_inject gate"
        );
        let matched = imp
            .find_matching_stub_with_client_bounded(
                "GET",
                "/plain",
                &no_headers(),
                None,
                None,
                None,
                None,
                Duration::from_millis(60_000),
            )
            .await
            .expect("matcher must not error");
        assert!(matched.is_some());
    }
}
