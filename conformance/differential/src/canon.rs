//! Canonical form of an admin-API document, so two engines' stored imposters can be diffed.
//!
//! Only what is *not* a contract is removed, and only at the structural level it lives at — never
//! inside a response body or a predicate, where the same key names are user data:
//!
//! - imposter: `_links`, and the Rift-only `stubCount`, `enabled` and `_rift` (`numberOfRequests`
//!   is kept: it is a contract, and the retired suite asserted it);
//! - stub: `_links`, and the Rift-only `enabled` and `_rift`;
//! - response: the Rift-only `_rift`; inside `is`, the proxy timing `_proxyResponseTime`, and — in a
//!   stub a proxy *recorded* only — the headers [`crate::diff::IGNORED_HEADERS`] lists: a recording captures
//!   the upstream's `Date` and framing headers, the same clock and transport artefacts the wire
//!   diff ignores — and a numeric `wait` there, which `addWaitBehavior` measured. A stub the case
//!   wrote keeps every header and wait it was given;
//! - recorded request: everything but `method`, `path`, `query`, `body`, `headers`; header *names*
//!   are case-folded because Rift Title-Cases recorded names (`imposter/headers.rs`), the
//!   [`crate::diff::IGNORED_HEADERS`] transport headers are dropped (a request that came through a proxy carries
//!   the proxy client's `Connection` choice, not the case's), and values are compared as recorded.
//!
//! Object keys come out sorted because `serde_json::Map` is a `BTreeMap` here (no
//! `preserve_order`), so JSON key order is never a difference in an admin document.

use crate::diff::is_ignored_header;
use serde_json::{Map, Value};
use std::collections::BTreeSet;

const IMPOSTER_DROP: &[&str] = &["_links", "stubCount", "enabled", "_rift"];
const STUB_DROP: &[&str] = &["_links", "enabled", "_rift"];
const RESPONSE_DROP: &[&str] = &["_rift"];
const RECORDED_KEEP: &[&str] = &["method", "path", "query", "body", "headers", "form"];

/// The indices of the stubs a proxy recorded: Rift marks them with `recordedFrom`, Mountebank's
/// plain form with `_proxyResponseTime` on the recorded `is`.
#[must_use]
pub fn recorded_stubs(imposter: &Value) -> BTreeSet<usize> {
    let Some(Value::Array(stubs)) = imposter.get("stubs") else {
        return BTreeSet::new();
    };
    stubs
        .iter()
        .enumerate()
        .filter(|(_, stub)| {
            stub.get("recordedFrom").is_some()
                || stub
                    .get("responses")
                    .and_then(Value::as_array)
                    .is_some_and(|responses| {
                        responses
                            .iter()
                            .any(|r| r.pointer("/is/_proxyResponseTime").is_some())
                    })
        })
        .map(|(index, _)| index)
        .collect()
}

/// Canonicalises any admin response body: an imposter, `{"imposters": [...]}`, a stub, or other.
#[must_use]
pub fn admin_document(value: &Value) -> Value {
    let Value::Object(map) = value else {
        return value.clone();
    };
    if let Some(Value::Array(imposters)) = map.get("imposters") {
        let mut out = without(map, &["_links"]);
        out.insert(
            "imposters".to_string(),
            Value::Array(
                imposters
                    .iter()
                    .map(|i| imposter(i, &BTreeSet::new()))
                    .collect(),
            ),
        );
        return Value::Object(out);
    }
    if map.contains_key("protocol") || map.contains_key("stubs") {
        return imposter(value, &recorded_stubs(value));
    }
    if map.contains_key("responses") || map.contains_key("predicates") {
        return stub(value, false);
    }
    Value::Object(without(map, &["_links"]))
}

/// Canonicalises one imposter document; `recorded` names the stubs a proxy recorded.
#[must_use]
pub fn imposter(value: &Value, recorded: &BTreeSet<usize>) -> Value {
    let Value::Object(map) = value else {
        return value.clone();
    };
    let mut out = without(map, IMPOSTER_DROP);
    if let Some(Value::Array(stubs)) = map.get("stubs") {
        out.insert(
            "stubs".to_string(),
            Value::Array(
                stubs
                    .iter()
                    .enumerate()
                    .map(|(index, s)| stub(s, recorded.contains(&index)))
                    .collect(),
            ),
        );
    }
    if let Some(Value::Array(requests)) = map.get("requests") {
        out.insert(
            "requests".to_string(),
            Value::Array(requests.iter().map(recorded_request).collect()),
        );
    }
    Value::Object(out)
}

fn stub(value: &Value, recorded: bool) -> Value {
    let Value::Object(map) = value else {
        return value.clone();
    };
    let mut out = without(map, STUB_DROP);
    if let Some(Value::Array(responses)) = map.get("responses") {
        out.insert(
            "responses".to_string(),
            Value::Array(responses.iter().map(|r| response(r, recorded)).collect()),
        );
    }
    Value::Object(out)
}

fn response(value: &Value, recorded: bool) -> Value {
    let Value::Object(map) = value else {
        return value.clone();
    };
    let mut out = without(map, RESPONSE_DROP);
    if let Some(Value::Object(is)) = map.get("is") {
        let mut is = without(is, &["_proxyResponseTime"]);
        if let Some(Value::Object(headers)) = is.get("headers").filter(|_| recorded) {
            let kept = headers
                .iter()
                .filter(|(name, _)| !is_ignored_header(name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            is.insert("headers".to_string(), Value::Object(kept));
        }
        out.insert("is".to_string(), Value::Object(is));
    }
    if recorded {
        // `addWaitBehavior` records the upstream's measured time: noise, like `Date`.
        for key in ["behaviors", "_behaviors"] {
            if let Some(behaviors) = out.get_mut(key) {
                measured_waits(behaviors);
            }
        }
    }
    Value::Object(out)
}

fn measured_waits(behaviors: &mut Value) {
    match behaviors {
        Value::Array(steps) => steps.iter_mut().for_each(measured_waits),
        Value::Object(step) => {
            if let Some(wait) = step.get_mut("wait").filter(|w| w.is_number()) {
                *wait = Value::String("(measured)".to_string());
            }
        }
        _ => {}
    }
}

fn recorded_request(value: &Value) -> Value {
    let Value::Object(map) = value else {
        return value.clone();
    };
    let mut out = Map::new();
    for key in RECORDED_KEEP {
        if let Some(field) = map.get(*key) {
            out.insert((*key).to_string(), field.clone());
        }
    }
    if let Some(Value::Object(headers)) = map.get("headers") {
        out.insert(
            "headers".to_string(),
            Value::Object(
                headers
                    .iter()
                    .map(|(name, v)| (name.to_ascii_lowercase(), v.clone()))
                    .filter(|(name, _)| !is_ignored_header(name))
                    .collect(),
            ),
        );
    }
    Value::Object(out)
}

fn without(map: &Map<String, Value>, drop: &[&str]) -> Map<String, Value> {
    map.iter()
        .filter(|(key, _)| !drop.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_non_contract_fields_at_their_structural_level_only() {
        let stored = json!({
            "port": 4545, "protocol": "http", "numberOfRequests": 3, "stubCount": 1, "enabled": true,
            "_links": {"self": {"href": "x"}}, "_rift": {"flowState": {}},
            "stubs": [{
                "_links": {"self": {"href": "y"}}, "enabled": true, "_rift": {"id": "s"},
                "predicates": [{"equals": {"body": {"_links": 1, "enabled": 2}}}],
                "responses": [{"is": {"statusCode": 200, "_proxyResponseTime": 7,
                                      "headers": {"Date": "x", "content-length": "2", "X-Kept": "k"},
                                      "body": {"numberOfRequests": 9, "_rift": 1}},
                               "_rift": {"fault": {}}}]
            }]
        });
        assert_eq!(
            imposter(&stored, &BTreeSet::from([0])),
            json!({
                "port": 4545, "protocol": "http", "numberOfRequests": 3,
                "stubs": [{
                    "predicates": [{"equals": {"body": {"_links": 1, "enabled": 2}}}],
                    "responses": [{"is": {"statusCode": 200, "headers": {"X-Kept": "k"},
                                          "body": {"numberOfRequests": 9, "_rift": 1}}}]
                }]
            })
        );
    }

    #[test]
    fn a_stub_the_case_wrote_keeps_its_transport_headers() {
        let stored = json!({"protocol": "http", "stubs": [
            {"responses": [{"is": {"headers": {"Date": "fixed", "Connection": "close"}}}]},
            {"recordedFrom": "http://localhost:1", "responses": [{"is": {"headers": {"Date": "now"}},
                                                                   "behaviors": [{"wait": 102}]}]}
        ]});
        assert_eq!(recorded_stubs(&stored), BTreeSet::from([1]));
        assert_eq!(
            imposter(&stored, &recorded_stubs(&stored)),
            json!({"protocol": "http", "stubs": [
                {"responses": [{"is": {"headers": {"Date": "fixed", "Connection": "close"}}}]},
                {"recordedFrom": "http://localhost:1", "responses": [{"is": {"headers": {}},
                                                                     "behaviors": [{"wait": "(measured)"}]}]}
            ]})
        );
    }

    #[test]
    fn mountebank_marks_a_recorded_stub_with_its_response_time() {
        let plain = json!({"stubs": [
            {"responses": [{"is": {"body": "authored"}}]},
            {"responses": [{"is": {"body": "recorded", "_proxyResponseTime": 3}}]},
            {"responses": [{"proxy": {"to": "http://localhost:1"}}]}
        ]});
        assert_eq!(recorded_stubs(&plain), BTreeSet::from([1]));
    }

    #[test]
    fn recorded_requests_keep_the_contract_fields_with_folded_header_names() {
        let stored = json!({"protocol": "http", "requests": [{
            "method": "GET", "path": "/a", "query": {"q": "1"}, "body": "",
            "headers": {"X-Custom": "Value", "host": "h", "Connection": "keep-alive"},
            "timestamp": "2026-10-08T00:00:00Z", "requestFrom": "127.0.0.1:5", "ip": "127.0.0.1"
        }]});
        assert_eq!(
            imposter(&stored, &BTreeSet::new()),
            json!({"protocol": "http", "requests": [{
                "method": "GET", "path": "/a", "query": {"q": "1"}, "body": "",
                "headers": {"x-custom": "Value", "host": "h"}
            }]})
        );
    }

    #[test]
    fn admin_document_recognises_lists_imposters_and_stubs() {
        let list =
            json!({"_links": {}, "imposters": [{"protocol": "http", "port": 1, "_links": {}}]});
        assert_eq!(
            admin_document(&list),
            json!({"imposters": [{"protocol": "http", "port": 1}]})
        );
        let stub = json!({"_links": {}, "enabled": true, "responses": [{"is": {}}]});
        assert_eq!(admin_document(&stub), json!({"responses": [{"is": {}}]}));
        let other = json!({"_links": {"imposters": {}}, "version": "x"});
        assert_eq!(admin_document(&other), json!({"version": "x"}));
    }
}
