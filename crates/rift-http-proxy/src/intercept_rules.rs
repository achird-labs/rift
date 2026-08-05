//! Intercept rules matched against decrypted requests from the forward-proxy listener
//! (epic #394, slice 4/5).
//!
//! A rule is a `(host?, predicates)` match against the intercepted request paired with an
//! [`InterceptAction`]: serve an inline stub, or forward the request to a named imposter port.
//! Rules reuse the existing Mountebank-compatible predicate engine
//! ([`rift_mock_core::imposter::predicates::stub_matches`]) so the same predicate JSON shape works
//! here as everywhere else in Rift.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rift_mock_core::imposter::stub_matches;
use rift_mock_core::proxy::intercept_ca::CertificateAuthority;
use rift_types::Predicate;

/// A single intercept rule: an optional host filter plus predicates (AND-ed together), and the
/// action to take when both match.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InterceptRule {
    /// Exact-match intercepted host (case-insensitive). `None` matches any host.
    #[serde(default)]
    pub host: Option<String>,
    /// Predicates matched against the decrypted request (implicit AND, same as stub matching).
    #[serde(default)]
    pub predicates: Vec<Predicate>,
    pub action: InterceptAction,
}

/// What to do with an intercepted request that matches a rule.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InterceptAction {
    /// Answer inline with a fixed stub response.
    Serve(ServeStub),
    /// Forward the request to a named imposter port on localhost.
    Forward(ForwardTarget),
}

/// An inline stub response for a [`InterceptAction::Serve`] rule.
///
/// Build one with [`ServeStub::new`]: `body` and its pre-rendered form must stay in step, so
/// neither is publicly writable and there is no struct-literal form outside this module.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", from = "ServeStubRaw")]
pub struct ServeStub {
    #[serde(default = "default_status")]
    pub status_code: u16,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Issue #933: any JSON value, matching `is.body` on the imposter stub path. A string body is
    /// served verbatim; any other value is pre-rendered into `rendered_body`. Read it through
    /// [`Self::body`]; writing it directly would strand the rendering.
    #[serde(default)]
    body: Option<serde_json::Value>,
    /// Issue #933: compact JSON rendering of a non-string [`Self::body`], computed ONCE at
    /// rule-insert time so the intercept request path never re-serializes it — the same
    /// render-once shape as `StubResponse::Is::rendered_body` (issue #479). `None` for a string
    /// body (served as-is) or no body at all. A derived cache, not part of the wire format.
    #[serde(skip)]
    rendered_body: Option<Arc<str>>,
}

impl ServeStub {
    pub fn new(
        status_code: u16,
        headers: HashMap<String, String>,
        body: Option<serde_json::Value>,
    ) -> Self {
        // Only a non-string body needs rendering; a string body is served as-is. `Display` on a
        // `Value` *is* its compact serialization and is total, so there is no failure to swallow
        // here — unlike `to_string(&T)`, which would hand back a `Result` we could only default.
        let rendered_body = body
            .as_ref()
            .filter(|b| !b.is_string())
            .map(|b| Arc::from(b.to_string().as_str()));
        Self {
            status_code,
            headers,
            body,
            rendered_body,
        }
    }

    /// The body as posted, in its original JSON shape — this is what `GET /intercept/rules`
    /// lists. Use [`Self::body_str`] for the bytes to put on the wire.
    #[must_use]
    pub fn body(&self) -> Option<&serde_json::Value> {
        self.body.as_ref()
    }

    /// The response body to serve: a string body verbatim, a non-string body's pre-rendered
    /// compact JSON, or empty when there is no body. Never serializes — the rendering happened at
    /// construction.
    #[must_use]
    pub fn body_str(&self) -> &str {
        match (&self.rendered_body, &self.body) {
            (Some(rendered), _) => rendered,
            (None, Some(serde_json::Value::String(s))) => s,
            (None, _) => "",
        }
    }
}

/// Deserialization shim for [`ServeStub`]: the derive would default the `#[serde(skip)]`
/// `rendered_body` to `None`, silently pushing the rendering back onto the request path. Routing
/// deserialization through [`ServeStub::new`] is what keeps the render-once guarantee true for
/// rules that arrive over the admin API (which is all of them).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServeStubRaw {
    #[serde(default = "default_status")]
    status_code: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: Option<serde_json::Value>,
}

impl From<ServeStubRaw> for ServeStub {
    fn from(raw: ServeStubRaw) -> Self {
        ServeStub::new(raw.status_code, raw.headers, raw.body)
    }
}

fn default_status() -> u16 {
    200
}

/// A localhost imposter port to forward an intercepted request to.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ForwardTarget {
    pub port: u16,
}

/// Maximum number of intercept rules the store retains. `POST /intercept/rules` is unauthenticated
/// on the intercept admin surface, so an uncapped `Vec` both grows without bound and linearly slows
/// every intercepted request's `match_request` scan; the cap bounds both (issue #554).
pub const MAX_RULES: usize = 10_000;

/// Returned by [`InterceptRules::add`] / [`InterceptRules::extend`] when the store already holds
/// [`MAX_RULES`] rules. The admin handler maps this to `429 Too Many Requests`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("intercept rule store is at capacity ({limit} rules); delete rules before adding more")]
pub struct RulesAtCapacity {
    pub limit: usize,
}

/// Shared, mutable rule store. Cheap to clone (an `Arc` inside) so the listener and the admin API
/// can each hold a handle to the same rules.
#[derive(Debug, Clone, Default)]
pub struct InterceptRules(Arc<RwLock<Vec<InterceptRule>>>);

impl InterceptRules {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a rule, rejecting it once the store is at [`MAX_RULES`] (issue #554). The check and
    /// the push happen under the same write lock so the cap holds under concurrent adds.
    pub fn add(&self, rule: InterceptRule) -> Result<(), RulesAtCapacity> {
        let mut rules = self.write();
        if rules.len() >= MAX_RULES {
            return Err(RulesAtCapacity { limit: MAX_RULES });
        }
        rules.push(rule);
        Ok(())
    }

    /// Append many rules atomically: either the whole batch fits under [`MAX_RULES`] and is added,
    /// or none of it is and the capacity error is returned (no partial batch).
    pub fn extend(&self, new_rules: Vec<InterceptRule>) -> Result<(), RulesAtCapacity> {
        let mut rules = self.write();
        if rules.len() + new_rules.len() > MAX_RULES {
            return Err(RulesAtCapacity { limit: MAX_RULES });
        }
        rules.extend(new_rules);
        Ok(())
    }

    /// A snapshot clone of all current rules, in insertion order.
    pub fn list(&self) -> Vec<InterceptRule> {
        self.read().clone()
    }

    /// Remove all rules.
    pub fn clear(&self) {
        self.write().clear();
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// The action of the first rule whose host matches (or has no host filter) AND whose
    /// predicates all match the given request. `None` if no rule matches.
    #[allow(clippy::too_many_arguments)]
    pub fn match_request(
        &self,
        host: &str,
        method: &str,
        path: &str,
        query: Option<&str>,
        headers: &HashMap<String, String>,
        body: Option<&str>,
    ) -> Option<InterceptAction> {
        let rules = self.read();
        rules
            .iter()
            .find(|rule| {
                let host_matches = rule
                    .host
                    .as_deref()
                    .is_none_or(|h| h.eq_ignore_ascii_case(host));
                host_matches
                    && (rule.predicates.is_empty()
                        || stub_matches(
                            &rule.predicates,
                            method,
                            path,
                            query,
                            headers,
                            body,
                            None,
                            None,
                            None,
                            0,
                        )
                        // A predicate `inject` error (e.g. a throwing script) is out of scope for
                        // intercept-rule fail-loud handling (issue #440 only covers imposter stub
                        // matching) — log and treat the rule as non-matching rather than panic the
                        // intercept listener on a bad script.
                        .unwrap_or_else(|e| {
                            tracing::warn!(error = %format_args!("{e:#}"), "intercept rule predicate match failed");
                            false
                        }))
            })
            .map(|rule| rule.action.clone())
    }

    /// Recover a poisoned lock rather than propagate the panic — a reader/writer panicking while
    /// holding the lock does not corrupt the `Vec`, so continuing to serve rules is safe.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<InterceptRule>> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<InterceptRule>> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// Shared control-plane state for the intercept feature: the rule store the listener matches
/// against, and the CA the admin API exports (cert + truststores).
#[derive(Clone)]
pub struct InterceptState {
    pub rules: InterceptRules,
    pub ca: Arc<CertificateAuthority>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn predicate_path_equals(path: &str) -> Predicate {
        let value = serde_json::json!({ "equals": { "path": path } });
        serde_json::from_value(value).expect("valid predicate JSON")
    }

    // ===== Issue #933: `serve` bodies accept any JSON value, like `is.body` on the stub path =====

    /// Deserialize a whole rule rather than a bare `ServeStub`, so a rule-level serde change
    /// cannot pass this gate while breaking real input. The admin API's untagged `RuleOrRules`
    /// wrapper — the layer that turned this issue's failure into an opaque error — is private to
    /// `admin_api::handlers::intercept` and is covered by a test there.
    fn serve_stub_from_action(action: serde_json::Value) -> ServeStub {
        let rule: InterceptRule =
            serde_json::from_value(serde_json::json!({ "action": { "serve": action } }))
                .expect("a serve rule with any JSON body deserializes");
        match rule.action {
            InterceptAction::Serve(stub) => stub,
            other => panic!("expected a serve action, got {other:?}"),
        }
    }

    fn serve_stub_with_body(body: serde_json::Value) -> ServeStub {
        serve_stub_from_action(serde_json::json!({ "statusCode": 200, "body": body }))
    }

    // AC1: the shape the issue reports — an object body — deserializes and serves the compact
    // rendering, not a serde error.
    #[test]
    fn serve_stub_object_body_renders_compact_json() {
        let stub = serve_stub_with_body(serde_json::json!({ "featureX": "ON", "n": 1 }));
        assert_eq!(
            stub.body_str(),
            r#"{"featureX":"ON","n":1}"#,
            "an object body renders as compact JSON, matching the stub path's `is.body`"
        );
        assert_eq!(stub.status_code, 200);
    }

    // AC2: every non-string JSON value renders, not just objects.
    #[test]
    fn serve_stub_non_string_body_variants_render() {
        for (body, expected) in [
            (serde_json::json!([1, 2, 3]), "[1,2,3]"),
            (serde_json::json!(42), "42"),
            (serde_json::json!(true), "true"),
            (
                serde_json::json!({ "a": { "b": [1, null] } }),
                r#"{"a":{"b":[1,null]}}"#,
            ),
        ] {
            assert_eq!(
                serve_stub_with_body(body.clone()).body_str(),
                expected,
                "body {body} renders compactly"
            );
        }
    }

    // AC3: the widening is strictly additive — a string body is still served verbatim, never
    // re-quoted or re-escaped. This is the wire-compatibility guarantee the issue promises.
    #[test]
    fn serve_stub_string_body_is_served_verbatim() {
        let stub = serve_stub_with_body(serde_json::json!(r#"{"featureX":"ON"}"#));
        assert_eq!(
            stub.body_str(),
            r#"{"featureX":"ON"}"#,
            "a string body is served as-is; widening must not add a layer of JSON quoting"
        );
        assert_eq!(
            serve_stub_with_body(serde_json::json!("hi")).body_str(),
            "hi"
        );
    }

    // AC4: absent and explicit-null bodies keep today's empty-body behaviour.
    #[test]
    fn serve_stub_absent_and_null_body_are_empty() {
        let absent = serve_stub_from_action(serde_json::json!({ "statusCode": 204 }));
        assert_eq!(absent.body(), None, "an omitted body stays absent");
        assert_eq!(absent.body_str(), "");
        assert_eq!(absent.status_code, 204);

        let null = serve_stub_with_body(serde_json::Value::Null);
        assert_eq!(
            null.body(),
            None,
            "an explicit null body deserializes to None exactly as it did when body was a String"
        );
        assert_eq!(null.body_str(), "");
    }

    // `body()` reports the body as posted, which is a different question from `body_str()`'s "what
    // goes on the wire" — for a non-string body the two deliberately disagree, and an embedder
    // inspecting a rule wants the former.
    #[test]
    fn serve_stub_body_reports_the_posted_json_not_the_rendering() {
        let stub = serve_stub_with_body(serde_json::json!({ "featureX": "ON" }));
        assert_eq!(
            stub.body(),
            Some(&serde_json::json!({ "featureX": "ON" })),
            "the posted JSON value is preserved, not replaced by its rendering"
        );
        assert_eq!(stub.body_str(), r#"{"featureX":"ON"}"#);

        let string_body = serve_stub_with_body(serde_json::json!("hi"));
        assert_eq!(string_body.body(), Some(&serde_json::json!("hi")));
        assert_eq!(
            string_body.body_str(),
            "hi",
            "for a string body the two agree apart from JSON quoting"
        );
    }

    // AC5: the non-string rendering happens ONCE, at rule-insert time, and is *not* recomputed per
    // request. Pointer equality across calls proves the value is a stored cache rather than a fresh
    // serialization — and doing it on a *deserialized* stub proves the cache survives the serde
    // path (a `#[serde(skip)]` field that defaults to `None` would silently reintroduce hot-path
    // serialization).
    #[test]
    fn serve_stub_renders_body_once_at_construction() {
        let stub = serve_stub_with_body(serde_json::json!({ "featureX": "ON" }));
        let first = stub.body_str();
        let second = stub.body_str();
        assert_eq!(first, second);
        assert!(
            std::ptr::eq(first.as_ptr(), second.as_ptr()),
            "the rendered body is cached at construction, not re-serialized per request"
        );

        // The cache must also survive the clone `match_request` hands to the request path.
        let cloned = stub.clone();
        assert_eq!(cloned.body_str(), r#"{"featureX":"ON"}"#);
    }

    // AC6: `GET /intercept/rules` must give back what was posted — an object body round-trips as an
    // object, not as its rendered string.
    //
    // Asserted as *whole-value* equality, not just on `body`: `#[serde(from = "ServeStubRaw")]`
    // makes `ServeStub`'s own deserialize attributes dead, so the field list now lives in two
    // places with nothing coupling them. A field added to one and forgotten in the other would be
    // silently dropped from every inbound rule; only a full round-trip catches that.
    #[test]
    fn serve_stub_round_trips_object_body() {
        let posted = serde_json::json!({
            "host": "cdn.example.com",
            "predicates": [],
            "action": { "serve": {
                "statusCode": 503,
                "headers": { "content-type": "application/json" },
                "body": { "featureX": "ON" }
            }}
        });
        let rule: InterceptRule =
            serde_json::from_value(posted.clone()).expect("object serve body is accepted");
        let listed = serde_json::to_value(&rule).expect("a rule serializes");
        assert_eq!(
            listed, posted,
            "a listed rule is byte-for-byte what was posted — every field survives the \
             ServeStubRaw shim, and the render-once cache never leaks onto the wire"
        );
    }

    // The rendered body must reach the request path through the store, which is what
    // `match_request` clones out.
    #[test]
    fn object_serve_body_survives_the_rule_store() {
        let rules = InterceptRules::new();
        rules
            .add(InterceptRule {
                host: None,
                predicates: vec![predicate_path_equals("/config.json")],
                action: InterceptAction::Serve(ServeStub::new(
                    200,
                    HashMap::new(),
                    Some(serde_json::json!({ "featureX": "ON" })),
                )),
            })
            .unwrap();

        match rules.match_request(
            "any.example.com",
            "GET",
            "/config.json",
            None,
            &HashMap::new(),
            None,
        ) {
            Some(InterceptAction::Serve(stub)) => {
                assert_eq!(stub.body_str(), r#"{"featureX":"ON"}"#)
            }
            other => panic!("expected the serve rule to match, got {other:?}"),
        }
    }

    #[test]
    fn rules_crud_roundtrip() {
        let rules = InterceptRules::new();
        assert!(rules.is_empty());
        assert_eq!(rules.len(), 0);

        let rule = InterceptRule {
            host: Some("cdn.example.com".to_string()),
            predicates: vec![],
            action: InterceptAction::Serve(ServeStub::new(
                200,
                HashMap::new(),
                Some(serde_json::json!("hi")),
            )),
        };
        rules.add(rule.clone()).unwrap();
        assert_eq!(rules.len(), 1);
        assert!(!rules.is_empty());
        assert_eq!(rules.list(), vec![rule]);

        rules.clear();
        assert!(rules.is_empty());
        assert_eq!(rules.list(), Vec::new());
    }

    #[test]
    fn predicate_narrows_match() {
        let rules = InterceptRules::new();
        rules
            .add(InterceptRule {
                host: None,
                predicates: vec![predicate_path_equals("/only-this")],
                action: InterceptAction::Forward(ForwardTarget { port: 4545 }),
            })
            .unwrap();

        let headers = HashMap::new();
        let matched =
            rules.match_request("any.example.com", "GET", "/only-this", None, &headers, None);
        assert_eq!(
            matched,
            Some(InterceptAction::Forward(ForwardTarget { port: 4545 }))
        );

        let unmatched =
            rules.match_request("any.example.com", "GET", "/other", None, &headers, None);
        assert_eq!(unmatched, None);
    }

    #[test]
    fn host_filter_is_case_insensitive_and_none_matches_any() {
        let rules = InterceptRules::new();
        rules
            .add(InterceptRule {
                host: Some("CDN.example.com".to_string()),
                predicates: vec![],
                action: InterceptAction::Forward(ForwardTarget { port: 1 }),
            })
            .unwrap();
        let headers = HashMap::new();
        assert!(
            rules
                .match_request("cdn.example.com", "GET", "/", None, &headers, None)
                .is_some()
        );
        assert!(
            rules
                .match_request("other.example.com", "GET", "/", None, &headers, None)
                .is_none()
        );
    }

    fn any_rule() -> InterceptRule {
        InterceptRule {
            host: None,
            predicates: vec![],
            action: InterceptAction::Forward(ForwardTarget { port: 1 }),
        }
    }

    // Issue #554: the rule store must not grow without bound — repeated adds are rejected once the
    // cap is reached, so per-request match latency stays bounded too.
    #[test]
    fn add_rejects_at_capacity() {
        let rules = InterceptRules::new();
        for _ in 0..MAX_RULES {
            rules.add(any_rule()).expect("under the cap");
        }
        assert_eq!(rules.len(), MAX_RULES);
        assert_eq!(
            rules.add(any_rule()),
            Err(RulesAtCapacity { limit: MAX_RULES }),
            "adding past the cap is rejected"
        );
        assert_eq!(rules.len(), MAX_RULES, "the rejected rule was not stored");
    }

    #[test]
    fn extend_at_capacity_is_atomic() {
        let rules = InterceptRules::new();
        rules
            .extend(vec![any_rule(); MAX_RULES - 1])
            .expect("fits under the cap");
        assert_eq!(rules.len(), MAX_RULES - 1);

        // A batch of 2 would exceed the cap by 1 — the whole batch is rejected, none added.
        assert_eq!(
            rules.extend(vec![any_rule(); 2]),
            Err(RulesAtCapacity { limit: MAX_RULES }),
            "a batch that would exceed the cap is rejected atomically"
        );
        assert_eq!(
            rules.len(),
            MAX_RULES - 1,
            "no rule from the batch was added"
        );

        // Boundary: a batch that lands *exactly* at the cap fits (only `>` rejects).
        rules
            .extend(vec![any_rule(); 1])
            .expect("filling exactly to the cap is allowed");
        assert_eq!(rules.len(), MAX_RULES);
    }
}
