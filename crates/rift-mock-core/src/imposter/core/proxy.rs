//! Proxy record/replay: predicate generation, stub insertion, and upstream proxying.
//!
//! Part of the `Imposter` implementation; see `core/mod.rs` for the struct definition.

use super::*;
use crate::imposter::behavior_pipeline::{BehaviorOutcome, BehaviorRun, ServedParts};
use crate::imposter::predicates::regex_cache::cached_regex;
use crate::recording::{ClaimToken, ProxyStoreError, StubPlacement, StubPublication};
use std::hash::BuildHasher;

/// Parts read from a successful upstream proxy response, before recording:
/// `(status, headers, body, latency_ms)`.
type ForwardedResponse = (u16, Vec<(String, String)>, bytes::Bytes, u64);

/// The request body as the handler already holds it (issue #1321). `raw` is what the client sent:
/// it is forwarded and keyed on, and is a `Bytes` handle so forwarding it is a refcount bump.
/// `text` is the form the journal and predicates use: the body as-is when it is UTF-8, its base64
/// otherwise (issue #636). A generated `body` predicate must use `text`, because the matcher
/// compares it against the next request's text form.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProxyBody<'a> {
    pub raw: &'a bytes::Bytes,
    pub text: &'a str,
}

/// A proxied response ready to serve: the upstream's, or a recorded one replayed.
#[derive(Debug)]
pub(crate) struct ProxiedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The upstream latency, when `addWaitBehavior` asked for it.
    pub latency_ms: Option<u64>,
}

/// What proxying a request produced.
#[derive(Debug)]
#[must_use]
pub(crate) enum ProxyOutcome {
    Served(ProxiedResponse),
    /// A behavior on the proxy response failed under `strictBehaviors` (#375): serve this instead.
    StrictFailure(hyper::Response<http_body_util::Full<bytes::Bytes>>),
}

/// The upstream response after the proxy response's own behaviors ran on it (issue #1189).
enum Transformed {
    Applied(u16, Vec<(String, String)>, bytes::Bytes),
    /// A behavior failed under the lenient contract: serve these parts, record nothing.
    Degraded(u16, Vec<(String, String)>, bytes::Bytes),
    StrictFailure(hyper::Response<http_body_util::Full<bytes::Bytes>>),
}

/// A won proxy-recording claim (issue #1193). It is released on drop unless settled — which covers
/// the one exit no `return` can: the request future being dropped mid-await, when the client
/// disconnects or the imposter stops. Before, such a claim stayed held for the life of the
/// imposter, and every later identical request was forwarded as `InFlight` and never recorded.
///
/// Releasing from `Drop` is sound because `release_claim` is synchronous and token-checked: a
/// guard whose claim was already re-taken frees nothing.
struct HeldClaim<'a> {
    store: &'a dyn ProxyRecordingStore,
    port: u16,
    signature: &'a RequestSignature,
    /// `Some` until the claim is settled or released.
    token: Option<ClaimToken>,
}

impl<'a> HeldClaim<'a> {
    fn new(
        store: &'a dyn ProxyRecordingStore,
        port: u16,
        signature: &'a RequestSignature,
        token: ClaimToken,
    ) -> Self {
        Self {
            store,
            port,
            signature,
            token: Some(token),
        }
    }

    /// Settle the claim, offering the generated stub when one exists.
    ///
    /// A failed settle releases the claim so the signature stays retryable instead of wedging as
    /// Recorded with nothing behind it (issue #315, and the publication-failure case of #910).
    /// The caller keeps its upstream response either way: the upstream call succeeded, only
    /// recording failed.
    fn settle(mut self, resp: RecordedResponse, publication: Option<&StubPublication<'_>>) {
        let Some(token) = self.token.take() else {
            return;
        };
        // The path is named in the warn below: for a publishing store a `complete` failure is a
        // failed *publication*, which an operator has to correlate differently from a plain
        // recording failure.
        let (path, settled) = match publication {
            Some(publication) => (
                "complete",
                self.store
                    .complete(self.port, self.signature.clone(), token, resp, publication),
            ),
            None => (
                "record",
                self.store
                    .record(self.port, self.signature.clone(), token, resp),
            ),
        };
        if let Err(e) = settled {
            warn!(
                "Failed to settle proxy recording via {path}(), releasing claim so it stays \
                 retryable: {e}"
            );
            self.store.release_claim(self.port, self.signature, token);
        }
    }
}

impl Drop for HeldClaim<'_> {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.store.release_claim(self.port, self.signature, token);
        }
    }
}

/// Run `run`'s behaviors on an upstream response. A body that is not UTF-8 goes through as base64
/// and is decoded afterwards, as a binary `is` body is. `content-length` is dropped because the
/// body may have changed length; the server derives it from the body served.
async fn transform_upstream<SH: BuildHasher>(
    run: &BehaviorRun<'_, SH>,
    status: u16,
    headers: Vec<(String, String)>,
    body: bytes::Bytes,
) -> Transformed {
    let (text, binary) = match String::from_utf8(body.to_vec()) {
        Ok(text) => (text, false),
        Err(_) => {
            use base64::Engine;
            (
                base64::engine::general_purpose::STANDARD.encode(&body),
                true,
            )
        }
    };
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for (k, v) in headers {
        grouped.entry(k).or_default().push(v);
    }
    let (parts, mut degraded) = match run
        .apply(ServedParts {
            status,
            // The upstream's body is the author's choice of text (issue #1203): a proxy `lookup`
            // (#1189) exists to expand tokens the upstream returns.
            headers: crate::behaviors::authored_headers(grouped),
            body: crate::behaviors::Spliced::authored(text),
        })
        .await
    {
        BehaviorOutcome::Applied { parts, degraded } => (parts, degraded),
        BehaviorOutcome::StrictFailure(response) => return Transformed::StrictFailure(response),
    };
    let mut headers: Vec<(String, String)> = parts
        .headers
        .into_iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("content-length"))
        .flat_map(|(k, values)| values.into_iter().map(move |v| (k.clone(), v.into_text())))
        .collect();
    let body = if binary {
        match crate::imposter::handler::decode_binary_body(parts.body.into_text(), run.strict) {
            Ok(crate::imposter::handler::BinaryBody::Decoded(bytes)) => bytes,
            Ok(crate::imposter::handler::BinaryBody::RawFallback(bytes)) => {
                headers.push(("x-rift-binary-error".to_string(), "true".to_string()));
                degraded = true;
                bytes
            }
            Err(strict_failure) => return Transformed::StrictFailure(*strict_failure),
        }
    } else {
        bytes::Bytes::from(parts.body.into_text())
    };
    if degraded {
        Transformed::Degraded(parts.status, headers, body)
    } else {
        Transformed::Applied(parts.status, headers, body)
    }
}

/// The predicate-generator keys this engine reads (issue #1327). Any other key is accepted and
/// reported as unread, never refused: Mountebank configs carry keys Rift may not implement yet.
const GENERATOR_KEYS: [&str; 8] = [
    "inject",
    "matches",
    "caseSensitive",
    "predicateOperator",
    "except",
    "jsonpath",
    "xpath",
    "ignore",
];

/// The keys of a predicate generator this engine does not read, sorted.
pub(crate) fn unread_generator_keys(generator: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = generator
        .as_object()
        .map(|generator| {
            generator
                .keys()
                .filter(|key| !GENERATOR_KEYS.contains(&key.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    keys.sort_unstable();
    keys
}

/// The generator's `jsonpath` (else `xpath`) body selector, when it is well formed and compiles. A
/// selector that does not compile is not carried: a recorded stub holding it could not be loaded
/// again (#1220). rift-lint reports a malformed shape (W021); a selector that does not compile is
/// only logged here.
fn generator_selector(
    generator: &serde_json::Map<String, serde_json::Value>,
) -> Option<crate::imposter::PredicateSelector> {
    use crate::behaviors::{validate_jsonpath_selector, validate_xpath_selector};
    use crate::imposter::PredicateSelector;

    let (key, value) = generator
        .get_key_value("jsonpath")
        .or_else(|| generator.get_key_value("xpath"))?;
    let selector: PredicateSelector =
        match serde_json::from_value(serde_json::json!({ key: value })) {
            Ok(selector) => selector,
            Err(e) => {
                tracing::warn!(
                    "predicateGenerator `{key}` is malformed ({e}); the body is captured whole"
                );
                return None;
            }
        };
    let compiles = match &selector {
        PredicateSelector::JsonPath { selector } => validate_jsonpath_selector(selector),
        PredicateSelector::XPath { selector, .. } => validate_xpath_selector(selector),
    };
    match compiles {
        Ok(()) => Some(selector),
        Err(e) => {
            tracing::warn!(
                "predicateGenerator `{key}` selector does not compile ({e}); the body is captured whole"
            );
            None
        }
    }
}

/// What `selector` selects from `body`, as Mountebank's generator captures it: one value as a
/// string, several as an array, nothing as `""`.
fn selected_body(
    selector: &crate::imposter::PredicateSelector,
    body: &str,
    except: Option<&regex::Regex>,
) -> serde_json::Value {
    use crate::behaviors::{Selection, jsonpath_selection, xpath_selection_in};
    use crate::imposter::PredicateSelector;

    let selection = match selector {
        PredicateSelector::JsonPath { selector } => serde_json::from_str(body)
            .ok()
            .and_then(|json| jsonpath_selection(&json, selector)),
        PredicateSelector::XPath {
            selector,
            namespaces,
        } => xpath_selection_in(body, selector, namespaces.as_ref()),
    };
    match selection.unwrap_or(Selection::One(String::new())) {
        Selection::One(value) => serde_json::Value::String(match except {
            Some(re) => re.replace_all(&value, "").into_owned(),
            None => value,
        }),
        Selection::Many(values) => {
            serde_json::Value::Array(values.into_iter().map(serde_json::Value::String).collect())
        }
    }
}

/// Mountebank's `objFilter`: a string removes that key, an array removes each named key, an object
/// recurses into the object-valued fields it names.
fn remove_ignored(
    captured: &mut serde_json::Map<String, serde_json::Value>,
    filter: &serde_json::Value,
) {
    match filter {
        serde_json::Value::String(key) => {
            captured.remove(key);
        }
        serde_json::Value::Array(keys) => {
            for key in keys.iter().filter_map(serde_json::Value::as_str) {
                captured.remove(key);
            }
        }
        serde_json::Value::Object(nested) => {
            for (key, filter) in nested {
                if let Some(serde_json::Value::Object(inner)) = captured.get_mut(key) {
                    remove_ignored(inner, filter);
                }
            }
        }
        _ => {}
    }
}

impl Imposter {
    /// Generate predicates from request based on predicateGenerators config.
    ///
    /// Returns `Err` when an `inject` generator could not produce predicates (script/pool/output
    /// failure) so the caller can skip auto-stub creation instead of silently recording a match-all
    /// stub (issue #498). An `Ok(empty)` list means the generators legitimately produced nothing.
    pub(crate) fn generate_predicates_from_request<SH: BuildHasher>(
        &self,
        generators: &[serde_json::Value],
        method: &str,
        path: &str,
        headers: &HashMap<String, Vec<String>, SH>,
        body: Option<&str>,
        query: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, crate::scripting::PredicateGeneratorError> {
        Self::generate_predicates_impl(generators, method, path, headers, body, query)
    }

    /// [`Self::generate_predicates_from_request`] without `&self`, so the proxy-recording path
    /// can run it on `spawn_blocking` (issue #476) — a `predicateGenerators.inject` script must
    /// not execute (and block on the MB script pool) on a tokio async worker.
    fn generate_predicates_impl<SH: BuildHasher>(
        generators: &[serde_json::Value],
        method: &str,
        path: &str,
        headers: &HashMap<String, Vec<String>, SH>,
        body: Option<&str>,
        query: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, crate::scripting::PredicateGeneratorError> {
        let mut predicates = Vec::new();

        for r#gen in generators {
            let Some(gen_obj) = r#gen.as_object() else {
                continue;
            };

            // Handle inject predicateGenerator — calls a JS function with the request and
            // predicates built so far; the function returns additional predicate objects.
            if let Some(inject_fn) = gen_obj.get("inject").and_then(|v| v.as_str()) {
                #[cfg(feature = "javascript")]
                {
                    use crate::scripting::{MountebankRequest, execute_predicate_generator_inject};
                    let query_map = query
                        .map(crate::imposter::parse_query_string)
                        .unwrap_or_default();
                    let mb_request = MountebankRequest {
                        method: method.to_string(),
                        path: path.to_string(),
                        query: query_map.into_iter().collect(),
                        // `MountebankRequest.headers` is the fixed single-value scripting boundary
                        // (out of scope for #704/#1025), so a repeated header exposes only its
                        // first value to a predicateGenerator inject script.
                        headers: headers
                            .iter()
                            .filter_map(|(k, v)| v.first().map(|first| (k.clone(), first.clone())))
                            .collect(),
                        // `body` is already the classified string from the caller (base64 for a
                        // binary request body, issue #636); this path doesn't thread the mode
                        // flag through separately, so default to `Text`.
                        body: body.map(|b| b.to_string()),
                        mode: None,
                    };
                    let inject_preds =
                        execute_predicate_generator_inject(inject_fn, &mb_request, &predicates)?;
                    predicates.extend(inject_preds);
                }
                #[cfg(not(feature = "javascript"))]
                {
                    tracing::warn!(
                        "predicateGenerator inject requires the 'javascript' feature; generator ignored"
                    );
                    let _ = inject_fn;
                }
                continue;
            }

            // Get the matches config
            let Some(matches) = gen_obj.get("matches").and_then(|m| m.as_object()) else {
                continue;
            };

            // Get options
            let case_sensitive = gen_obj
                .get("caseSensitive")
                .and_then(|c| c.as_bool())
                .unwrap_or(true);
            let predicate_operator = gen_obj
                .get("predicateOperator")
                .and_then(|p| p.as_str())
                .unwrap_or("equals");
            let except_pattern = gen_obj.get("except").and_then(|e| e.as_str());
            let selector = generator_selector(gen_obj);
            let ignore = |field: &str| gen_obj.get("ignore").and_then(|ignore| ignore.get(field));

            // Build predicate values
            let mut pred_values = serde_json::Map::new();

            // Handle path
            if matches
                .get("path")
                .and_then(|p| p.as_bool())
                .unwrap_or(false)
            {
                let mut path_val = path.to_string();
                // Apply except pattern if present
                if let Some(pattern) = except_pattern
                    && let Some(re) = cached_regex(pattern, false)
                {
                    path_val = re.replace_all(&path_val, "").to_string();
                }
                pred_values.insert("path".to_string(), serde_json::Value::String(path_val));
            }

            // Handle method
            if matches
                .get("method")
                .and_then(|m| m.as_bool())
                .unwrap_or(false)
            {
                let mut method_val = method.to_string();
                if let Some(pattern) = except_pattern
                    && let Some(re) = cached_regex(pattern, false)
                {
                    method_val = re.replace_all(&method_val, "").to_string();
                }
                pred_values.insert("method".to_string(), serde_json::Value::String(method_val));
            }

            // Handle query
            if matches
                .get("query")
                .and_then(|q| q.as_bool())
                .unwrap_or(false)
                && let Some(query_str) = query
            {
                let mut query_json: serde_json::Map<String, serde_json::Value> =
                    crate::imposter::parse_query_string(query_str)
                        .into_iter()
                        .map(|(k, v)| (k, serde_json::Value::String(v)))
                        .collect();
                if let Some(filter) = ignore("query") {
                    remove_ignored(&mut query_json, filter);
                }
                if !query_json.is_empty() {
                    pred_values.insert("query".to_string(), serde_json::Value::Object(query_json));
                }
            }

            // Handle headers
            if let Some(header_matches) = matches.get("headers").and_then(|h| h.as_object()) {
                let mut header_preds = serde_json::Map::new();
                for (header_name, should_match) in header_matches {
                    if should_match.as_bool().unwrap_or(false)
                        // A generated predicate is single-valued (issue #1025): the first value of
                        // a repeated header is what gets baked into the generated stub.
                        && let Some(header_value) =
                            headers.get(header_name).and_then(|values| values.first())
                    {
                        header_preds.insert(
                            header_name.clone(),
                            serde_json::Value::String(header_value.clone()),
                        );
                    }
                }
                if let Some(filter) = ignore("headers") {
                    remove_ignored(&mut header_preds, filter);
                }
                if !header_preds.is_empty() {
                    pred_values.insert(
                        "headers".to_string(),
                        serde_json::Value::Object(header_preds),
                    );
                }
            }

            // Handle body
            if matches
                .get("body")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
                && let Some(body_str) = body
            {
                let except = except_pattern.and_then(|pattern| cached_regex(pattern, false));
                let body_val = match &selector {
                    Some(selector) => selected_body(selector, body_str, except.as_deref()),
                    None => serde_json::Value::String(match &except {
                        Some(re) => re.replace_all(body_str, "").into_owned(),
                        None => body_str.to_string(),
                    }),
                };
                pred_values.insert("body".to_string(), body_val);
            }

            if pred_values.is_empty() {
                continue;
            }

            let captured_body = pred_values.contains_key("body");

            // Build the predicate with the operator
            let mut predicate = serde_json::Map::new();
            predicate.insert(
                predicate_operator.to_string(),
                serde_json::Value::Object(pred_values),
            );

            // The selector scopes the match the same way it scoped the capture (Mountebank copies
            // the generator's `jsonpath`/`xpath` onto the predicate).
            if captured_body
                && let Some(serde_json::Value::Object(selector)) =
                    selector.as_ref().and_then(|s| serde_json::to_value(s).ok())
            {
                predicate.extend(selector);
            }

            // Always write caseSensitive so the matcher sees the generator's intent
            predicate.insert(
                "caseSensitive".to_string(),
                serde_json::Value::Bool(case_sensitive),
            );

            predicates.push(serde_json::Value::Object(predicate));
        }

        Ok(predicates)
    }

    /// Resolve a proxy mode string to the placement its recorded stubs take.
    ///
    /// The single source of truth for that mapping: the engine's own insertion and the
    /// [`StubPublication`] handed to a publishing store both go through here, so the position a
    /// publisher is told to reproduce can never drift from the one the engine would use.
    fn placement_for_mode(proxy_mode: &str) -> StubPlacement {
        // Parsed like every other reader of the mode (issue #1314): `proxyalways` used to get a
        // proxyAlways store but proxyOnce placement.
        if ProxyMode::parse(proxy_mode).unwrap_or_default() == ProxyMode::ProxyAlways {
            StubPlacement::AfterProxyMerging
        } else {
            StubPlacement::BeforeProxy
        }
    }

    /// Insert or append a generated stub based on proxy mode.
    ///
    /// Instead of trusting a previously-obtained stub index (which may be stale
    /// if concurrent requests modified the stub list), this method re-locates the
    /// proxy stub under the write lock using `proxy_to` as identifier.
    ///
    /// For proxyOnce: Insert new stub BEFORE the proxy stub (so it matches first next time)
    /// For proxyAlways: Append response to existing stub AFTER proxy stub, or insert new AFTER proxy
    pub fn insert_or_append_proxy_stub(&self, stub: Stub, proxy_to: &str, proxy_mode: &str) {
        let placement = Self::placement_for_mode(proxy_mode);
        self.mutate_stubs(|stubs| {
            // Re-locate the proxy stub inside the write critical section to avoid stale-index races.
            let proxy_stub_index = stubs
                .iter()
                .position(|s| {
                    s.stub
                        .responses
                        .iter()
                        .any(|r| matches!(r, StubResponse::Proxy { proxy, .. } if proxy.to == proxy_to))
                })
                .unwrap_or(stubs.len());

            if placement == StubPlacement::AfterProxyMerging {
                // For proxyAlways, recorded stubs go AFTER the proxy stub
                // This ensures proxy always runs first and records each request

                // Try to find existing stub with matching predicates (after the proxy stub)
                let matching_stub_idx = stubs
                    .iter()
                    .map(|stub_state| &stub_state.stub)
                    .enumerate()
                    .skip(proxy_stub_index + 1) // Only look after the proxy stub
                    .find(|(_, existing)| {
                        // Structural comparison, not serialized JSON (issue #611): a predicate's
                        // operands are `HashMap`s, which serialize in iteration order, so two
                        // semantically equal multi-key predicate sets reliably produced different
                        // strings and dedup appended a duplicate stub instead of merging into it.
                        existing.predicates == stub.predicates && !existing.predicates.is_empty()
                    })
                    .map(|(idx, _)| idx);

                if let Some(idx) = matching_stub_idx {
                    // Append responses to the existing stub. States live behind `Arc` (issue #287),
                    // so rebuild the entry from a stub with the extended responses while reusing the
                    // slot's cycler + slot token.
                    let mut merged = stubs[idx].stub.clone();
                    merged.responses.extend(stub.responses);
                    let total = merged.responses.len();
                    // Recording stays on the wall clock, not the load clock (issue #1301): it builds
                    // a state per proxied request, and one stamp each would push the clock a second
                    // ahead per request. But never stamp earlier than the stub already served — an
                    // admin change may have left that stamp ahead of the wall clock.
                    let at = stubs[idx].loaded_at().max(chrono::Utc::now());
                    stubs[idx] = Arc::new(stubs[idx].with_stub(merged).with_loaded_at(at));
                    debug!(
                        "Appended response to existing stub at index {idx} (proxyAlways mode, {total} total responses)"
                    );
                    return;
                }

                // No matching stub found: insert new stub AFTER the proxy stub
                let insert_index = (proxy_stub_index + 1).min(stubs.len());
                stubs.insert(insert_index, Arc::new(StubState::new(stub)));
                debug!(
                    "Inserted generated stub at index {} after proxy (proxyAlways mode)",
                    insert_index
                );
            } else {
                // For proxyOnce: insert new stub BEFORE the proxy stub
                // This ensures the recorded stub matches first on subsequent requests
                let index = proxy_stub_index.min(stubs.len());
                stubs.insert(index, Arc::new(StubState::new(stub)));
                debug!(
                    "Inserted generated stub at index {} before proxy (proxyOnce mode)",
                    index
                );
            }
        });
    }

    /// Forward a request through proxy and optionally record the response.
    ///
    /// `behaviors` are the proxy response's own behaviors. They run on the upstream response before
    /// anything is recorded, so the client, the recording and the generated stub all get the
    /// transformed response, as in Mountebank's `proxyAndRecord` (issue #1189). A replay of a
    /// recording runs none of them — it was recorded transformed. A behavior that failed records
    /// nothing.
    pub(crate) async fn handle_proxy_request<SH>(
        &self,
        proxy_config: &ProxyResponse,
        method: &str,
        uri: &hyper::Uri,
        headers: &HashMap<String, Vec<String>, SH>,
        body: Option<ProxyBody<'_>>,
        behaviors: Option<&BehaviorRun<'_, SH>>,
    ) -> anyhow::Result<ProxyOutcome>
    where
        // `Clone + Send + 'static`: an inject predicateGenerator clones `headers` into a
        // `spawn_blocking` `'static` closure below.
        SH: BuildHasher + Clone + Send + 'static,
    {
        let client = super::resolve_upstream_client(self.upstream_client.as_ref())?;
        let client = &*client;

        info!(
            "Proxy config - addDecorateBehavior: {:?}, addWaitBehavior: {}, predicateGenerators: {:?}",
            proxy_config.add_decorate_behavior,
            proxy_config.add_wait_behavior,
            proxy_config.predicate_generators
        );

        // Build the proxy URL, applying path rewrite if configured
        let original_path = uri.path();
        let rewritten_path = if let Some(ref rewrite) = proxy_config.path_rewrite {
            original_path.replacen(&rewrite.from, &rewrite.to, 1)
        } else {
            original_path.to_string()
        };

        let target_url = format!(
            "{}{}{}",
            proxy_config.to,
            rewritten_path,
            uri.query().map(|q| format!("?{q}")).unwrap_or_default()
        );

        if proxy_config.path_rewrite.is_some() {
            debug!(
                "Proxy request to: {} (path rewritten from '{}')",
                target_url, original_path
            );
        } else {
            debug!("Proxy request to: {}", target_url);
        }

        // Create request signature for recording
        // Without predicateGenerators nothing names the request's identity, so the replay key is
        // method, path, query and body (issue #1317); headers stay out. With generators the user
        // chose the identity and the key is unchanged.
        let mut signature = RequestSignature::new(method, uri.path(), uri.query(), &[]);
        if proxy_config.predicate_generators.is_empty() {
            signature = signature.with_body(body.map_or(&[][..], |b| b.raw.as_ref()));
        }
        let port = self.journal_port();

        // Consult the proxy-recording gate. `AlreadyRecorded` replays; `Claimed` grants the
        // right to record; `InFlight` (a concurrent proxyOnce loser) and an unavailable
        // store proxy upstream without recording.
        // The store's mode is imposter-wide (from the first proxy stub), so a response that is
        // itself `proxyTransparent` — or a `defaultForward` — must not consult it, or it would
        // replay what another stub's `proxyOnce` recorded (issue #1314).
        let transparent = proxy_config.mode().unwrap_or_default() == ProxyMode::ProxyTransparent;
        let claim = if transparent {
            None
        } else {
            match self.proxy_store.try_claim(port, &signature) {
                Ok(ClaimOutcome::AlreadyRecorded) => {
                    if let Some(recorded) = self.proxy_store.lookup(port, &signature) {
                        debug!("Returning recorded proxy response (proxyOnce mode)");
                        return Ok(ProxyOutcome::Served(ProxiedResponse {
                            status: recorded.status,
                            headers: recorded.headers,
                            body: recorded.body,
                            latency_ms: recorded.latency_ms,
                        }));
                    }
                    // AlreadyRecorded but nothing to replay: a race (concurrent clear) or a
                    // misbehaving backend. Forward without recording rather than fail, but leave
                    // a trace since this is not an expected outcome.
                    warn!(
                        "Proxy store reported AlreadyRecorded but lookup found nothing; forwarding without recording"
                    );
                    None
                }
                Ok(ClaimOutcome::InFlight) => None,
                Ok(ClaimOutcome::Claimed(token)) => Some(HeldClaim::new(
                    self.proxy_store.as_ref(),
                    port,
                    &signature,
                    token,
                )),
                // The store arbitrates exactly-once and could not answer: fail the request rather
                // than forward it. Forwarding here would call the upstream while *nothing* is
                // serializing claims, so the duplicate is bounded by the outage, not by one racing
                // window — the outcome `proxyOnce` exists to prevent. The `BackendUnavailable` rides
                // the chain so the response boundary can answer 503 through the #318 door.
                Err(ProxyStoreError::Refused(cause)) => {
                    return Err(anyhow::Error::new(cause)
                        .context("proxyOnce claim refused; request not forwarded"));
                }
                Err(e) => {
                    warn!("Proxy recording store unavailable; forwarding without recording: {e}");
                    None
                }
            }
        };

        // Forward the request. Isolated so a failure returns early, dropping (releasing) the claim
        // (issue #315): a proxyOnce signature must stay retryable, not wedge because the upstream
        // call errored.
        let start = Instant::now();
        let forwarded: anyhow::Result<ForwardedResponse> = async {
            let mut request = match method.to_uppercase().as_str() {
                "GET" => client.get(&target_url),
                "POST" => client.post(&target_url),
                "PUT" => client.put(&target_url),
                "DELETE" => client.delete(&target_url),
                "PATCH" => client.patch(&target_url),
                "HEAD" => client.head(&target_url),
                _ => client.get(&target_url),
            };

            // Copy headers (excluding host). One `.header()` call PER VALUE, in send order
            // (issue #1025) — reqwest appends rather than replacing on a repeated call, so a
            // repeated header is forwarded in full instead of collapsing to whichever value used
            // to survive the old single-value map.
            for (key, values) in headers {
                let key_lower = key.to_lowercase();
                if key_lower != "host" && key_lower != "content-length" {
                    for value in values {
                        request = request.header(key, value);
                    }
                }
            }

            // Add inject headers
            for (key, value) in &proxy_config.inject_headers {
                request = request.header(key, value);
            }

            // Add body if present
            if let Some(proxy_body) = body {
                request = request.body(proxy_body.raw.clone());
            }

            // Send request
            let response = request
                .send()
                .await
                .with_context(|| format!("Failed to send proxy request to {target_url}"))?;
            let latency_ms = start.elapsed().as_millis() as u64;

            let status = response.status().as_u16();
            // Issue #999: the upstream hop, observed off the timing the `addWaitBehavior` path
            // already captures — so this measures the forward itself, not rift's own handling.
            crate::extensions::metrics::record_upstream_duration(method, status, latency_ms as f64);
            // This one list feeds three destinations — the client response, the
            // `RecordedResponse`, and the stub `proxyOnce`/`proxyAlways` generates — so a value
            // mangled here is mangled in all three, and the stub persists it (issue #1041).
            let response_headers = crate::imposter::headers::collect_response_headers(
                response.headers(),
            );
            // Check Content-Length before reading the full body to reject obviously oversized responses
            if let Some(content_length) = response.content_length()
                && content_length as usize > MAX_PROXY_RESPONSE_BODY_SIZE
            {
                anyhow::bail!(
                    "Proxy response body from {target_url} exceeds maximum size ({content_length} > {MAX_PROXY_RESPONSE_BODY_SIZE} bytes)"
                );
            }

            let body_bytes = response
                .bytes()
                .await
                .with_context(|| format!("Failed to read response body from {target_url}"))?;

            if body_bytes.len() > MAX_PROXY_RESPONSE_BODY_SIZE {
                anyhow::bail!(
                    "Proxy response body from {} exceeds maximum size ({} > {} bytes)",
                    target_url,
                    body_bytes.len(),
                    MAX_PROXY_RESPONSE_BODY_SIZE
                );
            }

            Ok((status, response_headers, body_bytes, latency_ms))
        }
        .await;

        let (status, response_headers, body_bytes, latency_ms) = match forwarded {
            Ok(parts) => parts,
            // Returning drops `claim`, which releases it: the signature stays retryable (#315).
            Err(e) => return Err(e),
        };
        let recorded_latency = proxy_config.add_wait_behavior.then_some(latency_ms);

        // After the forward, so a `wait` never counts toward the latency `addWaitBehavior` records;
        // before the recording, so what is recorded is what the client got.
        let (status, mut response_headers, body_bytes) = match behaviors {
            None => (status, response_headers, body_bytes),
            Some(run) => {
                match transform_upstream(run, status, response_headers, body_bytes).await {
                    Transformed::Applied(status, headers, body) => (status, headers, body),
                    // A failed behavior records nothing; returning drops (releases) the claim.
                    Transformed::Degraded(status, headers, body) => {
                        return Ok(ProxyOutcome::Served(ProxiedResponse {
                            status,
                            headers,
                            body: body.to_vec(),
                            latency_ms: recorded_latency,
                        }));
                    }
                    Transformed::StrictFailure(response) => {
                        return Ok(ProxyOutcome::StrictFailure(response));
                    }
                }
            }
        };

        // Build the recording if we hold a claim, but do NOT settle the claim yet: a store that
        // publishes stubs must be able to make "Recorded" conditional on publishing the stub, and
        // the stub does not exist until predicate generation below has run (issue #910).
        let mut recording = claim.map(|claim| {
            (
                claim,
                RecordedResponse {
                    status,
                    headers: response_headers.clone(),
                    body: body_bytes.to_vec(),
                    latency_ms: recorded_latency,
                    timestamp_secs: crate::util::unix_timestamp(),
                },
            )
        });

        // Generate and insert stub if predicateGenerators, addWaitBehavior, or addDecorateBehavior is configured
        // (Mountebank generates stubs automatically when these are enabled) — never for
        // `proxyTransparent`, which forwards every request and records nothing.
        if proxy_config.mode().unwrap_or_default() != ProxyMode::ProxyTransparent
            && (!proxy_config.predicate_generators.is_empty()
                || proxy_config.add_wait_behavior
                || proxy_config.add_decorate_behavior.is_some())
        {
            // An `inject` generator executes a JS script; run the generator pass off the async
            // worker under the script deadline (issue #476). Script-free generator lists (the
            // common case) keep the inline path — pure predicate building, no script pool.
            let has_inject_generator = proxy_config
                .predicate_generators
                .iter()
                .any(|g| g.as_object().is_some_and(|o| o.contains_key("inject")));
            // `Ok(preds)` = predicates generated (possibly legitimately empty); `Err((token, detail))`
            // = generation failed and predicates are unknown. On failure we must NOT record a stub:
            // an empty/partial predicate list matches every future request (issue #498). The failure
            // token is a short, header-safe category surfaced to the client; `detail` goes to the log.
            let generation: Result<Vec<serde_json::Value>, (&'static str, String)> =
                if !proxy_config.predicate_generators.is_empty() {
                    if has_inject_generator {
                        let generators = proxy_config.predicate_generators.clone();
                        let method = method.to_string();
                        let path = uri.path().to_string();
                        let headers = headers.clone();
                        let body = body.map(|b| b.text.to_string());
                        let query = uri.query().map(str::to_string);
                        let timeout = std::time::Duration::from_millis(
                            crate::scripting::resolve_script_timeout_ms(&self.config),
                        );
                        // Plain `spawn_blocking`, not `spawn_blocking_annotated` (issue #987):
                        // `execute_predicate_generator_inject` never installs a flow store, so a
                        // predicate generator has no `ctx.state` and nothing here can annotate.
                        let handle = tokio::task::spawn_blocking(move || {
                            Self::generate_predicates_impl(
                                &generators,
                                &method,
                                &path,
                                &headers,
                                body.as_deref(),
                                query.as_deref(),
                            )
                        });
                        match tokio::time::timeout(timeout, handle).await {
                            Ok(Ok(Ok(preds))) => Ok(preds),
                            Ok(Ok(Err(gen_err))) => Err((gen_err.kind(), gen_err.to_string())),
                            Ok(Err(join_err)) => {
                                Err(("task-panic", format!("generator task panicked: {join_err}")))
                            }
                            Err(_elapsed) => Err((
                                "timeout",
                                format!("timed out after {}ms", timeout.as_millis()),
                            )),
                        }
                    } else {
                        self.generate_predicates_from_request(
                            &proxy_config.predicate_generators,
                            method,
                            uri.path(),
                            headers,
                            body.map(|b| b.text),
                            uri.query(),
                        )
                        .map_err(|e| (e.kind(), e.to_string()))
                    }
                } else {
                    // No predicateGenerators, generate empty predicates (matches all requests)
                    Ok(vec![])
                };

            match generation {
                Ok(predicates) => {
                    // `addDecorateBehavior` is written into the SAVED stub's behaviors and not
                    // applied to this live response, as in Mountebank; it runs when the saved
                    // stub replays. The proxy response's own behaviors already ran above, and the
                    // stub holds their result — never the behaviors themselves, so nothing runs
                    // twice (Mountebank's `newIsResponse`).
                    let new_stub = create_stub_from_proxy_response(
                        predicates,
                        status,
                        &response_headers,
                        &body_bytes,
                        recorded_latency,
                        proxy_config.add_decorate_behavior.clone(),
                        Some(proxy_config.to.clone()),
                    );

                    // Insert or append the stub based on proxy mode
                    // proxyOnce: Insert new stub before the proxy stub
                    // proxyAlways: Append response to existing stub with matching predicates
                    // `placement_for_mode` parses it, so the string is passed as written.
                    let mode = proxy_config.mode.as_str();

                    // Settle the claim now that the stub exists, and before it is published, so a
                    // publishing store can refuse to commit "Recorded" if publication fails.
                    let settled = if let Some((claim, resp)) = recording.take() {
                        let publication = StubPublication {
                            stub: &new_stub,
                            placement: Self::placement_for_mode(mode),
                            proxy_to: &proxy_config.to,
                        };
                        claim.settle(resp, Some(&publication));
                        true
                    } else {
                        false
                    };

                    // A store that publishes stubs is the only publisher; inserting locally too
                    // would double-publish the recording (issue #910). When no claim was won there
                    // was no `complete` call either, so nobody publishes this stub — say so, rather
                    // than logging that the store took ownership of something it never saw.
                    if self.proxy_store.publishes_stubs() {
                        debug!(
                            "Skipping local stub insertion for path {} (proxy store publishes \
                             stubs; handed over: {settled})",
                            uri.path()
                        );
                    } else {
                        self.insert_or_append_proxy_stub(new_stub, &proxy_config.to, mode);
                    }
                    debug!(
                        "Generated stub from proxy response for path {} (mode: {})",
                        uri.path(),
                        mode
                    );
                }
                Err((token, detail)) => {
                    // Predicate generation failed — record nothing rather than a match-all stub,
                    // and mark the proxied response so the failure is client-visible, not a
                    // server-only warn (issue #498).
                    warn!(
                        "predicate generation failed for path {} ({detail}); skipping auto-stub to \
                         avoid recording a match-all stub",
                        uri.path()
                    );
                    response_headers
                        .push(("x-rift-generator-error".to_string(), token.to_string()));
                }
            }
        }

        // No stub was generated — nothing configured to generate one, or generation failed — so
        // there is nothing to publish and the claim settles through `record` exactly as before.
        if let Some((claim, resp)) = recording {
            claim.settle(resp, None);
        }

        Ok(ProxyOutcome::Served(ProxiedResponse {
            status,
            headers: response_headers,
            body: body_bytes.to_vec(),
            latency_ms: recorded_latency,
        }))
    }
}

#[cfg(test)]
mod proxy_dedup_tests {
    use super::*;
    use serde_json::json;

    fn imposter_with_proxy(to: &str) -> Imposter {
        let cfg = serde_json::from_value(json!({
            "port": 0,
            "protocol": "http",
            "stubs": [{ "responses": [{ "proxy": { "to": to, "mode": "proxyAlways" } }] }],
        }))
        .expect("valid imposter config");
        Imposter::new(cfg).expect("test imposter")
    }

    /// A stub whose single predicate matches on several fields at once — the shape a Mountebank
    /// `predicateGenerators: [{matches: {method, path, query}}]` produces.
    fn multi_key_stub(body: &str) -> Stub {
        serde_json::from_value(json!({
            "predicates": [{ "equals": { "method": "GET", "path": "/x", "query": { "a": "1" } } }],
            "responses": [{ "is": { "statusCode": 200, "body": body } }],
        }))
        .expect("valid stub")
    }

    // Issue #611: dedup compared *serialized* predicates, but a predicate's operands are `HashMap`s
    // that serialize in iteration order — so two semantically equal multi-key predicate sets
    // produced different strings and proxyAlways appended a duplicate stub instead of merging the
    // recorded response into the existing one.
    #[test]
    fn proxy_always_merges_responses_for_equal_multi_key_predicates() {
        let imposter = imposter_with_proxy("http://upstream");

        imposter.insert_or_append_proxy_stub(
            multi_key_stub("first"),
            "http://upstream",
            "proxyAlways",
        );
        imposter.insert_or_append_proxy_stub(
            multi_key_stub("second"),
            "http://upstream",
            "proxyAlways",
        );

        let stubs = imposter.get_stubs();
        assert_eq!(
            stubs.len(),
            2,
            "equal predicate sets must merge into one recorded stub alongside the proxy stub, \
             not append a duplicate"
        );
        assert_eq!(
            stubs[1].responses.len(),
            2,
            "both recorded responses must land on the single matching stub"
        );
    }
}

/// Issue #1193: a won claim is released when the request future is dropped, and only then.
#[cfg(test)]
mod held_claim_tests {
    use super::*;
    use crate::recording::{LocalProxyStore, ProxyMode, ProxyRecordingStore};

    fn signature() -> RequestSignature {
        RequestSignature::new("GET", "/p", None, &[])
    }

    fn recorded() -> RecordedResponse {
        RecordedResponse {
            status: 200,
            headers: vec![],
            body: b"up".to_vec(),
            latency_ms: None,
            timestamp_secs: 0,
        }
    }

    fn claim(store: &LocalProxyStore, sig: &RequestSignature) -> ClaimToken {
        match store.try_claim(1, sig).expect("store answers") {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected a claim, got {other:?}"),
        }
    }

    #[test]
    fn dropping_a_held_claim_releases_it() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let sig = signature();
        let held = HeldClaim::new(&store, 1, &sig, claim(&store, &sig));
        assert_eq!(
            store.try_claim(1, &sig).expect("answers"),
            ClaimOutcome::InFlight
        );
        drop(held);
        assert!(
            matches!(
                store.try_claim(1, &sig).expect("answers"),
                ClaimOutcome::Claimed(_)
            ),
            "a dropped claim must be free to take again"
        );
    }

    #[test]
    fn settling_a_held_claim_records_it_and_releases_nothing() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let sig = signature();
        HeldClaim::new(&store, 1, &sig, claim(&store, &sig)).settle(recorded(), None);
        assert_eq!(
            store.try_claim(1, &sig).expect("answers"),
            ClaimOutcome::AlreadyRecorded
        );
        assert_eq!(store.lookup(1, &sig).map(|r| r.body), Some(b"up".to_vec()));
    }

    /// The store ignores a stale token, so a guard dropped after its claim was re-taken (e.g. by a
    /// `clear`) cannot free the new holder.
    #[test]
    fn a_stale_guard_does_not_release_a_newer_claim() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let sig = signature();
        let stale = claim(&store, &sig);
        store.release_claim(1, &sig, stale);
        let _current = claim(&store, &sig);
        drop(HeldClaim::new(&store, 1, &sig, stale));
        assert_eq!(
            store.try_claim(1, &sig).expect("answers"),
            ClaimOutcome::InFlight
        );
    }
}

/// Issue #1327: Mountebank's `jsonpath`, `xpath` and `ignore` predicate-generator keys.
#[cfg(test)]
mod predicate_generator_selector_tests {
    use super::*;
    use serde_json::json;

    fn generate(
        generator: serde_json::Value,
        body: Option<&str>,
        query: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Vec<serde_json::Value> {
        let headers: HashMap<String, Vec<String>> = headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), vec![(*v).to_string()]))
            .collect();
        Imposter::generate_predicates_impl(&[generator], "POST", "/orders", &headers, body, query)
            .expect("predicate generation succeeds")
    }

    #[test]
    fn jsonpath_captures_the_selected_value_and_scopes_the_predicate() {
        let predicates = generate(
            json!({ "matches": { "body": true }, "jsonpath": { "selector": "$.id" } }),
            Some(r#"{"id": 42, "ts": "2026-10-08T10:00:00Z"}"#),
            None,
            &[],
        );
        assert_eq!(
            predicates,
            vec![json!({
                "equals": { "body": "42" },
                "jsonpath": { "selector": "$.id" },
                "caseSensitive": true
            })]
        );
    }

    #[test]
    fn jsonpath_with_several_matches_captures_an_array() {
        let predicates = generate(
            json!({ "matches": { "body": true }, "jsonpath": { "selector": "$.items[*].sku" } }),
            Some(r#"{"items": [{"sku": "a"}, {"sku": "b"}]}"#),
            None,
            &[],
        );
        assert_eq!(predicates[0]["equals"], json!({ "body": ["a", "b"] }));
    }

    #[test]
    fn jsonpath_selecting_nothing_captures_the_empty_string() {
        let predicates = generate(
            json!({ "matches": { "body": true }, "jsonpath": { "selector": "$.missing" } }),
            Some(r#"{"id": 1}"#),
            None,
            &[],
        );
        assert_eq!(predicates[0]["equals"], json!({ "body": "" }));
        assert_eq!(
            predicates[0]["jsonpath"],
            json!({ "selector": "$.missing" })
        );
    }

    #[test]
    fn xpath_with_ns_captures_the_selected_value() {
        let predicates = generate(
            json!({
                "matches": { "body": true },
                "xpath": { "selector": "//a:id", "ns": { "a": "urn:a" } },
                "caseSensitive": false
            }),
            Some(r#"<a:order xmlns:a="urn:a"><a:id>7</a:id><a:ts>now</a:ts></a:order>"#),
            None,
            &[],
        );
        assert_eq!(
            predicates,
            vec![json!({
                "equals": { "body": "7" },
                "xpath": { "selector": "//a:id", "ns": { "a": "urn:a" } },
                "caseSensitive": false
            })]
        );
    }

    #[test]
    fn except_still_applies_to_a_selected_scalar() {
        let predicates = generate(
            json!({ "matches": { "body": true }, "jsonpath": { "selector": "$.ref" }, "except": "-\\d+$" }),
            Some(r#"{"ref": "order-123"}"#),
            None,
            &[],
        );
        assert_eq!(predicates[0]["equals"], json!({ "body": "order" }));
    }

    /// A selector that does not compile would make the recorded stub unloadable (#1220), so it is
    /// not carried: the body is captured whole, as before #1327.
    #[test]
    fn a_selector_that_does_not_compile_is_not_carried() {
        let predicates = generate(
            json!({ "matches": { "body": true }, "jsonpath": { "selector": "$[[[bad" } }),
            Some(r#"{"id": 1}"#),
            None,
            &[],
        );
        assert_eq!(
            predicates,
            vec![json!({ "equals": { "body": r#"{"id": 1}"# }, "caseSensitive": true })]
        );
    }

    #[test]
    fn ignore_drops_query_keys_in_every_form() {
        for ignore in [json!("ts"), json!(["ts", "nonce"])] {
            let predicates = generate(
                json!({ "matches": { "query": true }, "ignore": { "query": ignore } }),
                None,
                Some("a=1&ts=99&nonce=x"),
                &[],
            );
            let expected = if ignore.is_string() {
                json!({ "query": { "a": "1", "nonce": "x" } })
            } else {
                json!({ "query": { "a": "1" } })
            };
            assert_eq!(predicates[0]["equals"], expected, "ignore {ignore}");
            assert!(predicates[0].get("ignore").is_none());
        }
    }

    #[test]
    fn ignore_drops_a_header() {
        let predicates = generate(
            json!({
                "matches": { "headers": { "X-Tenant": true, "X-Request-Id": true } },
                "ignore": { "headers": "X-Request-Id" }
            }),
            None,
            None,
            &[("X-Tenant", "acme"), ("X-Request-Id", "r-1")],
        );
        assert_eq!(
            predicates[0]["equals"],
            json!({ "headers": { "X-Tenant": "acme" } })
        );
    }

    /// Mountebank recurses an object filter into object-valued fields; Rift's captured fields hold
    /// strings below the first level, so a nested filter has nothing to remove there.
    #[test]
    fn ignore_object_form_recurses_only_into_objects() {
        let predicates = generate(
            json!({ "matches": { "query": true }, "ignore": { "query": { "a": "x" } } }),
            None,
            Some("a=1&b=2"),
            &[],
        );
        assert_eq!(
            predicates[0]["equals"],
            json!({ "query": { "a": "1", "b": "2" } })
        );
    }

    #[test]
    fn unread_generator_keys_are_listed() {
        assert_eq!(
            unread_generator_keys(
                &json!({ "matchs": { "path": true }, "matches": {}, "keyCaseSensitive": true })
            ),
            vec!["keyCaseSensitive", "matchs"]
        );
        assert!(
            unread_generator_keys(&json!({
                "matches": {}, "caseSensitive": true, "predicateOperator": "equals",
                "except": "x", "jsonpath": {}, "xpath": {}, "ignore": {}, "inject": "f"
            }))
            .is_empty()
        );
    }
}
