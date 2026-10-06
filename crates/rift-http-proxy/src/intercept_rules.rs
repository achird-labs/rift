//! Intercept rules matched against decrypted requests from the forward-proxy listener
//! (epic #394, slice 4/5).
//!
//! A rule is a `(host?, predicates)` match against the intercepted request paired with an
//! [`InterceptAction`]: serve an inline stub, or forward the request to an imposter — on this
//! engine's loopback by default, or at a named `host`/`scheme` (issue #1273).
//! Rules reuse the existing Mountebank-compatible predicate engine
//! ([`rift_mock_core::imposter::predicates::stub_matches`]) so the same predicate JSON shape works
//! here as everywhere else in Rift.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rift_mock_core::imposter::stub_matches;
use rift_mock_core::proxy::intercept_ca::CertificateAuthority;
use rift_types::Predicate;
use rift_types::wire::{deserialize_optional_status_code, multi_value_headers};

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
    /// Forward the request to an imposter: `http://127.0.0.1:{port}` unless the target names a
    /// `host` or `scheme`.
    Forward(ForwardTarget),
}

/// An inline stub response for a [`InterceptAction::Serve`] rule.
///
/// Build one with [`ServeStub::new`]: `body` and its pre-rendered form must stay in step, so
/// neither is publicly writable and there is no struct-literal form outside this module.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", from = "ServeStubRaw")]
pub struct ServeStub {
    // No field-level `default`s here: `#[serde(from = "ServeStubRaw")]` means the derived
    // `Deserialize` never reads them, so the parsing and defaulting rules live on the shim below
    // and nowhere else. Only the `serialize_with` half is live.
    pub status_code: u16,
    /// Issue #936: one or many values per name, matching `is.headers` on the imposter stub path.
    /// Serialized back through the shared helper, so a single value lists as a bare string and
    /// only a genuinely multi-value header becomes an array.
    #[serde(serialize_with = "multi_value_headers::serialize")]
    pub headers: HashMap<String, Vec<String>>,
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
        headers: HashMap<String, Vec<String>>,
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
    /// Issue #936: `Option` rather than a serde default, because the number-or-string parser runs
    /// only when the field is present — the 200 default is applied in `From` below. Same shape as
    /// `StubResponseRaw::status_code` on the imposter path, which also means an explicit
    /// `"statusCode": null` now reads as absent (200) where it used to be an error; that is the
    /// imposter path's own rule, and it widens what is accepted without changing any rule that
    /// was already valid.
    #[serde(default, deserialize_with = "deserialize_optional_status_code")]
    status_code: Option<u16>,
    #[serde(default, deserialize_with = "multi_value_headers::deserialize")]
    headers: HashMap<String, Vec<String>>,
    #[serde(default)]
    body: Option<serde_json::Value>,
}

impl From<ServeStubRaw> for ServeStub {
    fn from(raw: ServeStubRaw) -> Self {
        ServeStub::new(
            raw.status_code.unwrap_or_else(default_status),
            raw.headers,
            raw.body,
        )
    }
}

fn default_status() -> u16 {
    200
}

/// Where a `forward` rule sends an intercepted request: `{scheme}://{host}:{port}`, defaulting to
/// `http://127.0.0.1` so a rule written as `{"port": N}` means what it always has (issue #1273).
///
/// Validated on the way in, at every door (admin API, FFI, config file): an unknown key is refused
/// rather than ignored — a typo such as `"hots"` used to forward silently to loopback — and so is
/// a target that cannot be dialled (port 0, a host carrying a scheme, port or path). Intercept
/// rules are never replayed from stored bytes, so refusing at decode breaks nothing stored.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", try_from = "ForwardTargetRaw")]
pub struct ForwardTarget {
    pub port: u16,
    /// A hostname, an IPv4 literal or a bracketed IPv6 literal. `None` is `127.0.0.1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// `None` is `http`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme: Option<ForwardScheme>,
}

/// The scheme a `forward` rule dials its target with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardScheme {
    Http,
    Https,
}

impl ForwardTarget {
    /// A target on the engine's own loopback, as every rule before #1273 was.
    pub fn loopback(port: u16) -> Self {
        Self {
            port,
            host: None,
            scheme: None,
        }
    }

    /// The URL of `path_and_query` on this target.
    pub fn url(&self, path_and_query: &str) -> String {
        let scheme = match self.scheme {
            Some(ForwardScheme::Https) => "https",
            Some(ForwardScheme::Http) | None => "http",
        };
        let host = self.host.as_deref().unwrap_or("127.0.0.1");
        format!("{scheme}://{host}:{}{path_and_query}", self.port)
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ForwardTargetRaw {
    port: u16,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    scheme: Option<ForwardScheme>,
}

impl TryFrom<ForwardTargetRaw> for ForwardTarget {
    type Error = String;

    fn try_from(raw: ForwardTargetRaw) -> Result<Self, String> {
        if raw.port == 0 {
            return Err("forward port must be 1-65535, not 0".to_string());
        }
        if let Some(host) = &raw.host {
            check_forward_host(host)?;
        }
        Ok(Self {
            port: raw.port,
            host: raw.host,
            scheme: raw.scheme,
        })
    }
}

/// A forward host is only the host: a name, an IPv4 literal, or an IPv6 literal in brackets. The
/// port and scheme have their own keys, so anything that looks like a URL is a mistake to report.
fn check_forward_host(host: &str) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("forward host {host:?} {why}"));
    if host.is_empty() {
        return refuse("is empty");
    }
    if let Some(inner) = host.strip_prefix('[') {
        return match inner.strip_suffix(']') {
            Some(ip) if ip.parse::<std::net::Ipv6Addr>().is_ok() => Ok(()),
            _ => refuse("is not a bracketed IPv6 literal"),
        };
    }
    if host.contains("://") {
        return refuse("carries a scheme; put it in `scheme`");
    }
    if host.contains(':') {
        return refuse(
            "carries a port (or is an unbracketed IPv6 literal); put the port in `port`",
        );
    }
    if let Some(bad) = host
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
    {
        return refuse(&format!(
            "contains {bad:?}; it must be only a hostname or IP literal"
        ));
    }
    // What reqwest will actually dial. The URL parser reads `1.2.3` as 1.2.0.3 and `0x7f.1` as
    // 127.0.0.1, and refuses `999.0.0.1` on every request, so a host is admitted only when the
    // parser reads it back as written.
    match url::Host::parse(host) {
        Ok(url::Host::Domain(domain)) if domain.eq_ignore_ascii_case(host) => Ok(()),
        Ok(url::Host::Ipv4(ip)) if ip.to_string() == host => Ok(()),
        Ok(url::Host::Ipv6(_)) => Ok(()),
        _ => refuse("is not a hostname or IP address a URL can carry as written"),
    }
}

/// Maximum number of intercept rules the store retains. `POST /intercept/rules` is unauthenticated
/// on the intercept admin surface, so an uncapped `Vec` both grows without bound and linearly slows
/// every intercepted request's `match_request` scan; the cap bounds both (issue #554).
pub const MAX_RULES: usize = 10_000;

/// Returned by [`InterceptRules::add`] / [`InterceptRules::extend`] / [`InterceptRules::replace`]
/// when the resulting set would exceed [`MAX_RULES`] rules. The admin handler maps this to
/// `429 Too Many Requests`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "intercept rule set would exceed the capacity of {limit} rules; delete rules or send fewer"
)]
pub struct RulesAtCapacity {
    pub limit: usize,
}

/// Shared, mutable rule store. Cheap to clone (an `Arc` inside) so the listener and the admin API
/// can each hold a handle to the same rules.
#[derive(Debug, Clone, Default)]
pub struct InterceptRules(Arc<RwLock<RuleSet>>);

/// The rules plus how many of them, from the front, a config file seeded (issue #1271). Kept
/// under one lock so a reload can swap exactly the seeded prefix while runtime rules stay behind
/// it. Invariant: `seeded <= rules.len()`.
#[derive(Debug, Default)]
struct RuleSet {
    rules: Vec<InterceptRule>,
    seeded: usize,
}

/// What [`InterceptRules::replace_seeded`] left in the store: the config-seeded rules now at the
/// front, and the runtime rules kept after them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SeededReplaced {
    #[serde(rename = "rulesSeeded")]
    pub seeded: usize,
    #[serde(rename = "rulesRuntime")]
    pub runtime: usize,
}

impl InterceptRules {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a config-seeded set in place of whatever is stored (issue #1271): what a listener
    /// started from a config file begins with, and what a reload later replaces. Over
    /// [`MAX_RULES`] it is refused and the store left as it was.
    pub fn seed(&self, new_rules: Vec<InterceptRule>) -> Result<(), RulesAtCapacity> {
        if new_rules.len() > MAX_RULES {
            return Err(RulesAtCapacity { limit: MAX_RULES });
        }
        let mut set = self.write();
        set.seeded = new_rules.len();
        set.rules = new_rules;
        Ok(())
    }

    /// Swap the config-seeded prefix for `new_rules`, keeping every runtime rule after it, in one
    /// write lock (issue #1271). The runtime rules count toward [`MAX_RULES`]; a set that would not
    /// fit beside them is refused and the store left as it was.
    pub fn replace_seeded(
        &self,
        new_rules: Vec<InterceptRule>,
    ) -> Result<SeededReplaced, RulesAtCapacity> {
        let mut set = self.write();
        let runtime = set.rules.len() - set.seeded;
        if new_rules.len() + runtime > MAX_RULES {
            return Err(RulesAtCapacity { limit: MAX_RULES });
        }
        let seeded = new_rules.len();
        let old_seeded = set.seeded;
        set.rules.splice(0..old_seeded, new_rules);
        set.seeded = seeded;
        Ok(SeededReplaced { seeded, runtime })
    }

    /// Append a rule, rejecting it once the store is at [`MAX_RULES`] (issue #554). The check and
    /// the push happen under the same write lock so the cap holds under concurrent adds.
    pub fn add(&self, rule: InterceptRule) -> Result<(), RulesAtCapacity> {
        let mut set = self.write();
        if set.rules.len() >= MAX_RULES {
            return Err(RulesAtCapacity { limit: MAX_RULES });
        }
        set.rules.push(rule);
        Ok(())
    }

    /// Append many rules atomically: either the whole batch fits under [`MAX_RULES`] and is added,
    /// or none of it is and the capacity error is returned (no partial batch).
    pub fn extend(&self, new_rules: Vec<InterceptRule>) -> Result<(), RulesAtCapacity> {
        let mut set = self.write();
        if set.rules.len() + new_rules.len() > MAX_RULES {
            return Err(RulesAtCapacity { limit: MAX_RULES });
        }
        set.rules.extend(new_rules);
        Ok(())
    }

    /// Replace the whole set atomically (issue #1272), returning how many rules were replaced. The
    /// swap happens under one write lock, so a request matched concurrently sees the old set or the
    /// new one, never an empty store. A batch over [`MAX_RULES`] is refused and the old set kept;
    /// an empty batch is a valid replace and leaves the store empty. Every rule it installs is a
    /// runtime rule: a later reload re-seeds the config file's rules in front of them (#1271).
    pub fn replace(&self, new_rules: Vec<InterceptRule>) -> Result<usize, RulesAtCapacity> {
        if new_rules.len() > MAX_RULES {
            return Err(RulesAtCapacity { limit: MAX_RULES });
        }
        let mut set = self.write();
        set.seeded = 0;
        Ok(std::mem::replace(&mut set.rules, new_rules).len())
    }

    /// A snapshot clone of all current rules, in insertion order.
    pub fn list(&self) -> Vec<InterceptRule> {
        self.read().rules.clone()
    }

    /// Remove all rules, seeded ones included.
    pub fn clear(&self) {
        let mut set = self.write();
        set.rules.clear();
        set.seeded = 0;
    }

    pub fn len(&self) -> usize {
        self.read().rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().rules.is_empty()
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
        headers: &HashMap<String, Vec<String>>,
        body: Option<&str>,
    ) -> Option<InterceptAction> {
        let set = self.read();
        set.rules
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
    /// holding the lock does not corrupt the set — every mutation above completes its checks before
    /// changing anything — so continuing to serve rules is safe.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, RuleSet> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, RuleSet> {
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
    /// cannot pass this gate while breaking real input. The admin API's `RuleOrRules` wrapper
    /// (untagged until #1273), the layer that turned this issue's failure into an opaque error, is
    /// private to `admin_api::handlers::intercept` and is covered by a test there.
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

    // ===== Issue #936: statusCode and headers accept the same forms as the imposter stub path ====

    // AC2: a numeric-string `statusCode` is accepted (the form Mountebank takes and `is.statusCode`
    // already handles); a number still works; genuine junk is still refused rather than defaulted.
    #[test]
    fn serve_stub_status_code_takes_number_or_numeric_string() {
        assert_eq!(
            serve_stub_from_action(serde_json::json!({ "statusCode": 418 })).status_code,
            418
        );
        assert_eq!(
            serve_stub_from_action(serde_json::json!({ "statusCode": "418" })).status_code,
            418,
            "the string form is what this issue adds"
        );
        assert_eq!(
            serve_stub_from_action(serde_json::json!({})).status_code,
            200,
            "an absent statusCode still defaults to 200"
        );
        // The one accept-set change beyond the string form: an explicit null used to be an error
        // and now reads as absent, because the shared parser treats it that way for the imposter
        // path too. Strictly widening — no previously-valid rule changed meaning — but pinned
        // here so it stays a decision rather than a surprise.
        assert_eq!(
            serve_stub_from_action(serde_json::json!({ "statusCode": null })).status_code,
            200,
            "an explicit null statusCode reads as absent, matching `is.statusCode`"
        );

        for junk in [
            serde_json::json!("abc"),
            serde_json::json!(true),
            // One past `u16::MAX` in both spellings — the out-of-range boundary, not just an
            // obviously-silly number.
            serde_json::json!("65536"),
            serde_json::json!(65536),
        ] {
            let rule = serde_json::from_value::<InterceptRule>(serde_json::json!({
                "action": { "serve": { "statusCode": junk } }
            }));
            assert!(
                rule.is_err(),
                "statusCode {junk} must be refused, not coerced or defaulted"
            );
        }
    }

    // AC5: #754 parity — Mountebank recorders emit non-string scalar header values and coerce them
    // to strings. Sharing the stub path's helper means the intercept path inherits that, and this
    // pins it deliberately rather than leaving it an accident of the shared module.
    #[test]
    fn serve_stub_coerces_scalar_header_values() {
        let stub = serve_stub_from_action(serde_json::json!({
            "headers": { "X-Retry": 3, "X-Flag": true, "X-Ratio": 1.5 }
        }));
        assert_eq!(stub.headers["X-Retry"], vec!["3".to_string()]);
        assert_eq!(stub.headers["X-Flag"], vec!["true".to_string()]);
        assert_eq!(stub.headers["X-Ratio"], vec!["1.5".to_string()]);
    }

    // AC3 (parse half) + AC4/AC6: multi-value headers survive the wire, a single value still
    // round-trips as a bare string rather than a one-element array, and the whole rule is
    // byte-identical through the `ServeStubRaw` shim — which is what catches the shim drifting
    // out of step with `ServeStub` as fields are added.
    #[test]
    fn serve_stub_round_trips_multi_value_headers_and_string_status() {
        let stub = serve_stub_from_action(serde_json::json!({
            "headers": { "set-cookie": ["a=1", "b=2"] }
        }));
        assert_eq!(
            stub.headers["set-cookie"],
            vec!["a=1".to_string(), "b=2".to_string()]
        );

        let posted = serde_json::json!({
            "host": "cdn.example.com",
            "predicates": [],
            "action": { "serve": {
                "statusCode": 503,
                "headers": { "content-type": "application/json", "set-cookie": ["a=1", "b=2"] },
                "body": { "featureX": "ON" }
            }}
        });
        let rule: InterceptRule =
            serde_json::from_value(posted.clone()).expect("the widened forms are accepted");
        assert_eq!(
            serde_json::to_value(&rule).expect("a rule serializes"),
            posted,
            "a listed rule is byte-for-byte what was posted: the number status stays a number, \
             the single-value header stays a bare string, and the multi-value one stays an array"
        );
    }

    // Issue #1039: an intercept rule reaches the same shared deserializer as an imposter stub, so
    // a POSTed rule that spells one header name two ways must serve — and list back — one header.
    //
    // `POST /intercept/rules` parses the body with `from_slice`, so the *wire* path folds under the
    // first spelling the caller wrote. This test goes through `from_value` instead (that is what
    // `serve_stub_from_action` does), where the `serde_json::Map` is key-sorted and `Set-Cookie`
    // therefore arrives before `set-cookie`. Both orders are asserted deliberately: what the fix
    // guarantees is one entry whose contents are a function of the document, not a particular
    // spelling — see the invariant on `rift_types::wire::multi_value_headers`.
    #[test]
    fn serve_stub_folds_case_variant_header_keys_into_one_entry() {
        let stub = serve_stub_from_action(serde_json::json!({
            "headers": { "set-cookie": "a=1", "Set-Cookie": ["b=2", "c=3"] }
        }));
        assert_eq!(
            stub.headers.len(),
            1,
            "two spellings of one name is one header"
        );
        assert_eq!(
            stub.headers["Set-Cookie"],
            vec!["b=2".to_string(), "c=3".to_string(), "a=1".to_string()],
            "key-sorted `Map` order: `Set-Cookie` (0x53) precedes `set-cookie` (0x73)"
        );

        // The wire path a real caller takes — bytes, so document order decides.
        let rule: InterceptRule = serde_json::from_slice(
            br#"{"host":"cdn.example.com","predicates":[],
                 "action":{"serve":{"headers":{
                     "content-type":"text/plain","Content-Type":"application/json"}}}}"#,
        )
        .expect("case-variant keys are accepted, not rejected");
        let listed = serde_json::to_value(&rule).expect("a rule serializes");
        assert_eq!(
            listed["action"]["serve"]["headers"],
            serde_json::json!({ "content-type": ["text/plain", "application/json"] }),
            "GET /intercept/rules lists one key, under the first spelling POSTed"
        );
    }

    // A string `statusCode` is normalised to the number form on the way out — the rule is stored
    // as `u16`, so listing it back as `"418"` would be inventing a shape the store does not hold.
    #[test]
    fn serve_stub_lists_a_string_status_code_back_as_a_number() {
        let rule: InterceptRule = serde_json::from_value(serde_json::json!({
            "action": { "serve": { "statusCode": "418" } }
        }))
        .expect("string status accepted");
        let listed = serde_json::to_value(&rule).expect("a rule serializes");
        assert_eq!(
            listed["action"]["serve"]["statusCode"],
            serde_json::json!(418)
        );
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
                action: InterceptAction::Forward(ForwardTarget::loopback(4545)),
            })
            .unwrap();

        let headers = HashMap::new();
        let matched =
            rules.match_request("any.example.com", "GET", "/only-this", None, &headers, None);
        assert_eq!(
            matched,
            Some(InterceptAction::Forward(ForwardTarget::loopback(4545)))
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
                action: InterceptAction::Forward(ForwardTarget::loopback(1)),
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
            action: InterceptAction::Forward(ForwardTarget::loopback(1)),
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

    // ===== Issue #1272: atomic replace-all =====

    fn serve_rule(host: &str, body: &str) -> InterceptRule {
        serde_json::from_value(serde_json::json!({
            "host": host,
            "action": { "serve": { "statusCode": 200, "body": body } }
        }))
        .expect("valid serve rule")
    }

    fn served_body(rules: &InterceptRules, host: &str) -> Option<String> {
        match rules.match_request(host, "GET", "/", None, &HashMap::new(), None)? {
            InterceptAction::Serve(stub) => Some(stub.body_str().to_string()),
            other => panic!("expected a serve action, got {other:?}"),
        }
    }

    #[test]
    fn replace_swaps_the_whole_set_and_reports_the_old_count() {
        let rules = InterceptRules::new();
        rules
            .extend(vec![serve_rule("a.test", "v1"), serve_rule("b.test", "b1")])
            .expect("seed");
        let replaced = rules
            .replace(vec![serve_rule("a.test", "v2")])
            .expect("replace fits");
        assert_eq!(replaced, 2, "the count is of the rules that were replaced");
        assert_eq!(rules.len(), 1);
        assert_eq!(served_body(&rules, "a.test").as_deref(), Some("v2"));
        assert_eq!(
            served_body(&rules, "b.test"),
            None,
            "a rule absent from the new set is gone, not kept"
        );
    }

    #[test]
    fn replace_with_an_empty_set_clears() {
        let rules = InterceptRules::new();
        rules.add(serve_rule("a.test", "v1")).expect("seed");
        assert_eq!(rules.replace(Vec::new()), Ok(1));
        assert!(rules.is_empty());
    }

    #[test]
    fn replace_over_the_cap_is_refused_and_keeps_the_old_set() {
        let rules = InterceptRules::new();
        rules.add(serve_rule("a.test", "v1")).expect("seed");
        let too_many = vec![serve_rule("x.test", "x"); MAX_RULES + 1];
        assert_eq!(
            rules.replace(too_many),
            Err(RulesAtCapacity { limit: MAX_RULES })
        );
        assert_eq!(
            rules.len(),
            1,
            "a refused replace leaves the old set intact"
        );
        assert_eq!(served_body(&rules, "a.test").as_deref(), Some("v1"));
    }

    #[test]
    fn replace_at_exactly_the_cap_is_accepted() {
        let rules = InterceptRules::new();
        rules.add(serve_rule("a.test", "v1")).expect("seed");
        let full = vec![serve_rule("x.test", "x"); MAX_RULES];
        assert_eq!(rules.replace(full), Ok(1));
        assert_eq!(rules.len(), MAX_RULES);
    }

    /// The point of replace over clear-then-add: a matcher racing the swap sees the old rule or the
    /// new one, never an empty store.
    #[test]
    fn replace_never_exposes_an_empty_set() {
        let rules = InterceptRules::new();
        rules.add(serve_rule("a.test", "v0")).expect("seed");
        let writer = {
            let rules = rules.clone();
            std::thread::spawn(move || {
                for i in 0..2_000 {
                    rules
                        .replace(vec![serve_rule("a.test", &format!("v{i}"))])
                        .expect("replace fits");
                }
            })
        };
        for _ in 0..10_000 {
            assert!(
                served_body(&rules, "a.test").is_some(),
                "a request racing replace must never fall through to no-rule"
            );
        }
        writer.join().expect("writer thread");
    }

    // ===== Issue #1271: the config-seeded prefix =====

    #[test]
    fn seed_then_add_keeps_the_seeded_prefix() {
        let rules = InterceptRules::new();
        rules.seed(vec![serve_rule("a.test", "v1")]).expect("seed");
        rules.add(serve_rule("b.test", "runtime")).expect("add");
        let outcome = rules
            .replace_seeded(vec![serve_rule("a.test", "v2"), serve_rule("c.test", "c")])
            .expect("fits");
        assert_eq!(
            outcome,
            SeededReplaced {
                seeded: 2,
                runtime: 1
            }
        );
        let hosts: Vec<_> = rules.list().into_iter().map(|r| r.host.unwrap()).collect();
        assert_eq!(hosts, vec!["a.test", "c.test", "b.test"]);
        // A second reload replaces exactly the two seeded rules, not the runtime one.
        rules
            .replace_seeded(vec![serve_rule("a.test", "v3")])
            .expect("fits");
        let hosts: Vec<_> = rules.list().into_iter().map(|r| r.host.unwrap()).collect();
        assert_eq!(hosts, vec!["a.test", "b.test"]);
        assert_eq!(served_body(&rules, "a.test").as_deref(), Some("v3"));
    }

    #[test]
    fn clear_and_replace_reset_the_seeded_prefix() {
        let rules = InterceptRules::new();
        rules.seed(vec![serve_rule("a.test", "v1")]).expect("seed");
        rules.clear();
        rules.add(serve_rule("b.test", "runtime")).expect("add");
        rules
            .replace_seeded(vec![serve_rule("a.test", "v2")])
            .expect("fits");
        let hosts: Vec<_> = rules.list().into_iter().map(|r| r.host.unwrap()).collect();
        assert_eq!(
            hosts,
            vec!["a.test", "b.test"],
            "nothing seeded survived the clear"
        );

        rules
            .replace(vec![serve_rule("x.test", "x")])
            .expect("fits");
        rules
            .replace_seeded(vec![serve_rule("a.test", "v3")])
            .expect("fits");
        let hosts: Vec<_> = rules.list().into_iter().map(|r| r.host.unwrap()).collect();
        assert_eq!(
            hosts,
            vec!["a.test", "x.test"],
            "a PUT made every rule runtime"
        );
    }

    #[test]
    fn replace_seeded_counts_runtime_rules_against_the_cap() {
        let rules = InterceptRules::new();
        rules.seed(vec![serve_rule("a.test", "v1")]).expect("seed");
        rules
            .extend(vec![serve_rule("r.test", "r"); MAX_RULES - 1])
            .expect("fill to the cap");
        // Swapping one seeded rule for one fits exactly; two do not.
        assert!(
            rules
                .replace_seeded(vec![serve_rule("a.test", "v2")])
                .is_ok()
        );
        assert_eq!(
            rules.replace_seeded(vec![serve_rule("a.test", "v3"), serve_rule("b.test", "b")]),
            Err(RulesAtCapacity { limit: MAX_RULES })
        );
        assert_eq!(
            served_body(&rules, "a.test").as_deref(),
            Some("v2"),
            "refusal keeps the set"
        );
        assert_eq!(rules.len(), MAX_RULES);
    }

    #[test]
    fn seed_over_the_cap_is_refused() {
        let rules = InterceptRules::new();
        assert!(
            rules
                .seed(vec![serve_rule("x.test", "x"); MAX_RULES + 1])
                .is_err()
        );
        assert!(rules.is_empty());
    }
}
