//! Server-side verification (issue #494): count — and optionally return — recorded requests
//! matching a predicate set, with an optional closest non-match diff.
//!
//! This reuses the one true predicate evaluator (`stub_matches_inner`) and the recorded-request
//! store rather than reimplementing matching per SDK, so `verify(match, times(n))` in every SDK
//! can defer to the engine instead of shipping the whole journal over the wire and re-evaluating
//! predicates client-side (where `xpath`/`inject` are impractical).

use super::Imposter;
use crate::imposter::predicates::stub_matches_inner;
use crate::imposter::types::{Predicate, PredicateOperation, RecordedRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// Verification request — the body of `POST /imposters/{port}/verify`. `predicates` are AND'd
/// (the same implicit-AND the request hot path applies to a stub's predicates).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyOptions {
    #[serde(default)]
    pub predicates: Vec<Predicate>,
    /// Scope the count to one space, resolved via the imposter's `flow_id_source` — the same
    /// scoping `GET /savedRequests?match=flow_id=…` applies.
    #[serde(default)]
    pub flow_id: Option<String>,
    /// Return the matching requests, not just the count.
    #[serde(default)]
    pub include_requests: bool,
    /// Return the best-scoring non-match with per-clause failure details (for diff rendering).
    #[serde(default)]
    pub include_closest: bool,
}

/// Verification result. `requests`/`closest` are present only when the corresponding option was set.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyOutcome {
    /// Recorded requests (in flow scope) matching every predicate.
    pub matched: usize,
    /// Total recorded requests in flow scope, matched or not.
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests: Option<Vec<RecordedRequest>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closest: Option<ClosestMatch>,
}

/// The non-matching request that satisfied the most predicate clauses, with the clauses it failed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosestMatch {
    pub request: RecordedRequest,
    pub failed_predicates: Vec<FailedPredicate>,
}

/// One predicate the closest request failed, paired with the request's actual value(s) for the
/// fields that predicate references — the raw material for a readable diff.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedPredicate {
    pub predicate: Predicate,
    pub actual: Value,
}

impl Imposter {
    /// Count (and optionally return) recorded requests matching every predicate, scoped to
    /// `flow_id` when set. Reuses the live predicate evaluator and recorded-request store — no new
    /// state. An `inject` predicate that fails propagates as `Err` (the issue #440 contract the
    /// request hot path also honors) rather than being silently counted as a non-match.
    pub fn verify(&self, opts: &VerifyOptions) -> anyhow::Result<VerifyOutcome> {
        let scoped = self.get_recorded_requests_filtered(|r| match &opts.flow_id {
            Some(flow_id) => self.resolve_flow_id_recorded(&r.headers) == *flow_id,
            None => true,
        });
        let total = scoped.len();

        let mut matched = Vec::new();
        let mut non_matches = Vec::new();
        for req in scoped {
            if self.request_matches_predicates(&req, &opts.predicates)? {
                matched.push(req);
            } else if opts.include_closest {
                non_matches.push(req);
            }
        }

        let closest = if opts.include_closest {
            self.closest_non_match(non_matches, &opts.predicates)?
        } else {
            None
        };

        let matched_count = matched.len();
        Ok(VerifyOutcome {
            matched: matched_count,
            total,
            requests: opts.include_requests.then_some(matched),
            closest,
        })
    }

    /// Evaluate all `predicates` (implicit AND) against a recorded request, adapting the stored
    /// shape back to the matcher's inputs: the multi-value header map collapses to the single-value
    /// view live matching uses, and the already-parsed query map is threaded directly so no query
    /// string is re-encoded.
    fn request_matches_predicates(
        &self,
        req: &RecordedRequest,
        predicates: &[Predicate],
    ) -> anyhow::Result<bool> {
        let headers = collapse_headers(&req.headers);
        let form = Self::parse_form_data(&headers, req.body.as_deref());
        let body_json = req
            .body
            .as_deref()
            .and_then(|b| serde_json::from_str::<Value>(b).ok());
        let client_ip = client_ip_of(req);
        stub_matches_inner(
            predicates,
            &req.method,
            &req.path,
            None,
            &headers,
            req.body.as_deref(),
            Some(&req.request_from),
            client_ip.as_deref(),
            form.as_ref(),
            self.script_state_key(),
            body_json.as_ref(),
            Some(&req.query),
        )
    }

    /// The non-matching request satisfying the most top-level predicate clauses (ties → most
    /// recent, i.e. latest in the oldest-first journal), with the per-clause failure details a
    /// caller renders as a diff. `None` when there are no non-matches or no predicates to score.
    fn closest_non_match(
        &self,
        non_matches: Vec<RecordedRequest>,
        predicates: &[Predicate],
    ) -> anyhow::Result<Option<ClosestMatch>> {
        if non_matches.is_empty() || predicates.is_empty() {
            return Ok(None);
        }

        let mut best: Option<(usize, RecordedRequest)> = None;
        for req in non_matches {
            let mut satisfied = 0;
            for predicate in predicates {
                if self.request_matches_predicates(&req, std::slice::from_ref(predicate))? {
                    satisfied += 1;
                }
            }
            // `>=` so a later (more recent — the journal is oldest-first) request wins on a tie.
            let replace = match &best {
                None => true,
                Some((best_score, _)) => satisfied >= *best_score,
            };
            if replace {
                best = Some((satisfied, req));
            }
        }

        let (_, request) = best.expect("non_matches is non-empty");
        let mut failed_predicates = Vec::new();
        for predicate in predicates {
            if !self.request_matches_predicates(&request, std::slice::from_ref(predicate))? {
                failed_predicates.push(FailedPredicate {
                    predicate: predicate.clone(),
                    actual: actual_projection(&request, predicate),
                });
            }
        }
        Ok(Some(ClosestMatch {
            request,
            failed_predicates,
        }))
    }
}

/// The client IP an `ip` predicate matches against, recovered from the recorded `request_from`
/// (`ip:port`). Live matching passes `client_addr.ip()` separately from `request_from`; the journal
/// stores only the combined `ip:port`, so parse it back to a `SocketAddr` and take the IP. `None`
/// (compared against `""`, never matching) only when `request_from` isn't a parseable socket
/// address — the same "no client info" outcome the convenience matcher path produces.
fn client_ip_of(req: &RecordedRequest) -> Option<String> {
    req.request_from
        .parse::<std::net::SocketAddr>()
        .ok()
        .map(|addr| addr.ip().to_string())
}

/// Collapse the recorded multi-value header map to the single-value view the matcher expects,
/// taking the last value per header to mirror how live matching's single-value map is built (a
/// `HashMap` collect over the request headers keeps the last of duplicate-named headers).
fn collapse_headers(headers: &HashMap<String, Vec<String>>) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| v.last().map(|last| (k.clone(), last.clone())))
        .collect()
}

/// The request's actual values for the fields a failed predicate references, as a JSON object —
/// the raw material for a readable diff. For a field-based op (`equals`/`contains`/…) only the
/// referenced fields are projected; for a compound (`and`/`or`/`not`), an `inject`, or a
/// selector-based predicate no single field is implicated, so the whole request is returned.
fn actual_projection(req: &RecordedRequest, predicate: &Predicate) -> Value {
    if predicate.parameters.selector.is_some() {
        return request_view(req);
    }
    let fields = match &predicate.operation {
        PredicateOperation::Equals(f)
        | PredicateOperation::DeepEquals(f)
        | PredicateOperation::Contains(f)
        | PredicateOperation::StartsWith(f)
        | PredicateOperation::EndsWith(f)
        | PredicateOperation::Matches(f)
        | PredicateOperation::Exists(f) => f,
        PredicateOperation::Not(_)
        | PredicateOperation::Or(_)
        | PredicateOperation::And(_)
        | PredicateOperation::Inject(_) => return request_view(req),
    };
    let mut out = Map::new();
    for key in fields.keys() {
        if let Some(value) = request_field(req, key) {
            out.insert(key.clone(), value);
        }
    }
    Value::Object(out)
}

/// A canonical JSON view of the whole recorded request (used when a predicate implicates no single
/// field).
fn request_view(req: &RecordedRequest) -> Value {
    let mut m = Map::new();
    m.insert("method".to_string(), json!(req.method));
    m.insert("path".to_string(), json!(req.path));
    m.insert("query".to_string(), json!(req.query));
    m.insert("headers".to_string(), json!(req.headers));
    m.insert("requestFrom".to_string(), json!(req.request_from));
    if let Some(body) = &req.body {
        m.insert("body".to_string(), json!(body));
    }
    Value::Object(m)
}

/// The recorded request's value for a single predicate field key (Mountebank field names).
fn request_field(req: &RecordedRequest, key: &str) -> Option<Value> {
    match key {
        "method" => Some(json!(req.method)),
        "path" => Some(json!(req.path)),
        "query" => Some(json!(req.query)),
        "headers" => Some(json!(req.headers)),
        "body" => Some(json!(req.body)),
        "requestFrom" => Some(json!(req.request_from)),
        // The `ip` predicate matches against the bare IP (`client_ip_of`), so report that as the
        // actual — not the `ip:port` `request_from` the matcher never compares against.
        "ip" => Some(json!(client_ip_of(req).unwrap_or_default())),
        "form" => {
            let headers = collapse_headers(&req.headers);
            Some(json!(
                Imposter::parse_form_data(&headers, req.body.as_deref()).unwrap_or_default()
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::types::ImposterConfig;
    use serde_json::json;

    fn imposter(flow_id_source: Option<&str>) -> Imposter {
        let mut cfg = json!({ "port": 0, "protocol": "http", "recordRequests": true, "stubs": [] });
        if let Some(src) = flow_id_source {
            cfg["_rift"] = json!({ "flowState": { "flowIdSource": src } });
        }
        let config: ImposterConfig = serde_json::from_value(cfg).expect("config");
        Imposter::new(config).expect("imposter")
    }

    fn rec(
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> RecordedRequest {
        let mut hs: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in headers {
            hs.entry((*k).to_string())
                .or_default()
                .push((*v).to_string());
        }
        RecordedRequest {
            request_from: "127.0.0.1:5000".to_string(),
            method: method.to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            headers: hs,
            body: body.map(str::to_string),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn preds(v: Value) -> Vec<Predicate> {
        serde_json::from_value(v).expect("predicates")
    }

    #[test]
    fn matched_and_total_count_all_predicates_anded() {
        let imp = imposter(None);
        imp.record_request(&rec("GET", "/a", &[], None));
        imp.record_request(&rec("POST", "/a", &[], None));
        imp.record_request(&rec("GET", "/b", &[], None));

        let opts = VerifyOptions {
            predicates: preds(json!([{ "equals": { "method": "GET", "path": "/a" } }])),
            ..Default::default()
        };
        let out = imp.verify(&opts).expect("verify");
        assert_eq!(out.total, 3);
        assert_eq!(out.matched, 1);
        assert!(out.requests.is_none(), "requests omitted unless requested");
        assert!(out.closest.is_none(), "closest omitted unless requested");
    }

    #[test]
    fn empty_predicates_match_everything() {
        let imp = imposter(None);
        imp.record_request(&rec("GET", "/a", &[], None));
        imp.record_request(&rec("GET", "/b", &[], None));
        let out = imp.verify(&VerifyOptions::default()).expect("verify");
        assert_eq!(out.matched, 2);
        assert_eq!(out.total, 2);
    }

    #[test]
    fn include_requests_returns_the_matching_requests() {
        let imp = imposter(None);
        imp.record_request(&rec("GET", "/keep", &[], None));
        imp.record_request(&rec("GET", "/drop", &[], None));
        imp.record_request(&rec("GET", "/keep", &[], None));

        let opts = VerifyOptions {
            predicates: preds(json!([{ "equals": { "path": "/keep" } }])),
            include_requests: true,
            ..Default::default()
        };
        let out = imp.verify(&opts).expect("verify");
        let reqs = out.requests.expect("requests present");
        assert_eq!(reqs.len(), 2);
        assert!(reqs.iter().all(|r| r.path == "/keep"));
    }

    #[test]
    fn closest_picks_most_satisfied_clauses_and_reports_failures() {
        let imp = imposter(None);
        // Satisfies neither clause.
        imp.record_request(&rec("DELETE", "/x", &[], None));
        // Satisfies method only (1 of 2) — the closest.
        imp.record_request(&rec("GET", "/x", &[], None));

        let opts = VerifyOptions {
            predicates: preds(json!([
                { "equals": { "method": "GET" } },
                { "equals": { "path": "/wanted" } }
            ])),
            include_closest: true,
            ..Default::default()
        };
        let out = imp.verify(&opts).expect("verify");
        assert_eq!(out.matched, 0);
        let closest = out.closest.expect("closest present");
        assert_eq!(closest.request.path, "/x");
        assert_eq!(closest.request.method, "GET");
        // Only the path clause failed; the method clause was satisfied.
        assert_eq!(closest.failed_predicates.len(), 1);
        assert_eq!(closest.failed_predicates[0].actual, json!({ "path": "/x" }));
    }

    #[test]
    fn closest_breaks_ties_toward_the_most_recent() {
        let imp = imposter(None);
        // Both satisfy the method clause (1 of 1 failing overall since path never matches),
        // so the tie is broken toward the later-recorded request.
        imp.record_request(&rec("GET", "/first", &[], None));
        imp.record_request(&rec("GET", "/second", &[], None));

        let opts = VerifyOptions {
            predicates: preds(json!([
                { "equals": { "method": "GET" } },
                { "equals": { "path": "/never" } }
            ])),
            include_closest: true,
            ..Default::default()
        };
        let out = imp.verify(&opts).expect("verify");
        let closest = out.closest.expect("closest present");
        assert_eq!(
            closest.request.path, "/second",
            "ties resolve to most recent"
        );
    }

    #[test]
    fn flow_id_scopes_total_and_matched() {
        let imp = imposter(Some("header:X-Space"));
        imp.record_request(&rec("GET", "/a", &[("X-Space", "blue")], None));
        imp.record_request(&rec("GET", "/a", &[("X-Space", "green")], None));
        imp.record_request(&rec("GET", "/a", &[("X-Space", "blue")], None));

        let opts = VerifyOptions {
            predicates: preds(json!([{ "equals": { "path": "/a" } }])),
            flow_id: Some("blue".to_string()),
            ..Default::default()
        };
        let out = imp.verify(&opts).expect("verify");
        assert_eq!(out.total, 2, "total is scoped to the flow");
        assert_eq!(out.matched, 2);
    }

    #[test]
    fn flow_id_scopes_under_the_default_imposter_port_source() {
        // With no flow_id_source declared, every request's flow is the imposter port ("0" here).
        let imp = imposter(None);
        imp.record_request(&rec("GET", "/a", &[], None));
        imp.record_request(&rec("GET", "/a", &[], None));

        let matching = VerifyOptions {
            flow_id: Some("0".to_string()),
            ..Default::default()
        };
        assert_eq!(imp.verify(&matching).expect("verify").total, 2);

        let other = VerifyOptions {
            flow_id: Some("999".to_string()),
            ..Default::default()
        };
        let out = imp.verify(&other).expect("verify");
        assert_eq!(out.total, 0, "a non-matching flow scopes everything out");
        assert_eq!(out.matched, 0);
    }

    #[test]
    fn ip_predicate_matches_against_the_recorded_client_ip() {
        // Regression: the `ip` field must compare against the bare IP recovered from `request_from`
        // (`ip:port`), not an empty string — otherwise every `ip` predicate silently mis-counts.
        let imp = imposter(None);
        imp.record_request(&rec("GET", "/a", &[], None)); // request_from = 127.0.0.1:5000

        let opts = VerifyOptions {
            predicates: preds(json!([{ "equals": { "ip": "127.0.0.1" } }])),
            ..Default::default()
        };
        assert_eq!(imp.verify(&opts).expect("verify").matched, 1);
    }

    #[test]
    fn multi_value_header_collapses_to_the_last_value() {
        // A header recorded with two values collapses to its last, mirroring live matching's
        // single-value view.
        let imp = imposter(None);
        imp.record_request(&rec(
            "GET",
            "/a",
            &[("X-Dup", "first"), ("X-Dup", "second")],
            None,
        ));

        let last = VerifyOptions {
            predicates: preds(json!([{ "equals": { "headers": { "X-Dup": "second" } } }])),
            ..Default::default()
        };
        assert_eq!(imp.verify(&last).expect("verify").matched, 1);

        let first = VerifyOptions {
            predicates: preds(json!([{ "equals": { "headers": { "X-Dup": "first" } } }])),
            ..Default::default()
        };
        assert_eq!(
            imp.verify(&first).expect("verify").matched,
            0,
            "the shadowed first value must not match"
        );
    }

    #[test]
    fn closest_actual_falls_back_to_the_whole_request_for_a_compound_predicate() {
        // A compound (`or`) predicate implicates no single field, so `actual` is the whole-request
        // view rather than a field-keyed projection.
        let imp = imposter(None);
        imp.record_request(&rec("GET", "/c", &[], None));

        let opts = VerifyOptions {
            predicates: preds(json!([
                { "or": [{ "equals": { "path": "/a" } }, { "equals": { "path": "/b" } }] }
            ])),
            include_closest: true,
            ..Default::default()
        };
        let closest = imp.verify(&opts).expect("verify").closest.expect("closest");
        assert_eq!(closest.failed_predicates.len(), 1);
        let actual = &closest.failed_predicates[0].actual;
        assert!(
            actual.get("method").is_some() && actual.get("path").is_some(),
            "compound predicate reports the whole request view, got {actual}"
        );
    }

    #[test]
    fn closest_actual_falls_back_to_the_whole_request_for_a_selector_predicate() {
        // A jsonpath/xpath selector rewrites the effective body, so no single request field is
        // implicated — `actual` is the whole-request view.
        let imp = imposter(None);
        imp.record_request(&rec("POST", "/a", &[], Some(r#"{"foo":"bar"}"#)));

        let opts = VerifyOptions {
            predicates: preds(json!([
                { "equals": { "body": "nope" }, "jsonpath": { "selector": "$.foo" } }
            ])),
            include_closest: true,
            ..Default::default()
        };
        let closest = imp.verify(&opts).expect("verify").closest.expect("closest");
        let actual = &closest.failed_predicates[0].actual;
        assert!(
            actual.get("method").is_some() && actual.get("headers").is_some(),
            "selector predicate reports the whole request view, got {actual}"
        );
    }
}
