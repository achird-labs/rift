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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeStub {
    #[serde(default = "default_status")]
    pub status_code: u16,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
}

fn default_status() -> u16 {
    200
}

/// A localhost imposter port to forward an intercepted request to.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ForwardTarget {
    pub port: u16,
}

/// Shared, mutable rule store. Cheap to clone (an `Arc` inside) so the listener and the admin API
/// can each hold a handle to the same rules.
#[derive(Debug, Clone, Default)]
pub struct InterceptRules(Arc<RwLock<Vec<InterceptRule>>>);

impl InterceptRules {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a rule.
    pub fn add(&self, rule: InterceptRule) {
        self.write().push(rule);
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
                            tracing::warn!(error = %e, "intercept rule predicate match failed");
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

    #[test]
    fn rules_crud_roundtrip() {
        let rules = InterceptRules::new();
        assert!(rules.is_empty());
        assert_eq!(rules.len(), 0);

        let rule = InterceptRule {
            host: Some("cdn.example.com".to_string()),
            predicates: vec![],
            action: InterceptAction::Serve(ServeStub {
                status_code: 200,
                headers: HashMap::new(),
                body: Some("hi".to_string()),
            }),
        };
        rules.add(rule.clone());
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
        rules.add(InterceptRule {
            host: None,
            predicates: vec![predicate_path_equals("/only-this")],
            action: InterceptAction::Forward(ForwardTarget { port: 4545 }),
        });

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
        rules.add(InterceptRule {
            host: Some("CDN.example.com".to_string()),
            predicates: vec![],
            action: InterceptAction::Forward(ForwardTarget { port: 1 }),
        });
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
}
