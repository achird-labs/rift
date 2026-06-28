//! Server-side filtering of recorded requests by `match=` query clauses.
//!
//! Supports `header:<Name>=<Value>` and `flow_id=<Value>` (the latter resolved
//! via the imposter's `flow_id_source`). Multiple clauses are AND'd together.

use crate::imposter::RecordedRequest;

/// A single parsed `match=` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MatchClause {
    /// Match a request header by (case-insensitive) name and exact value.
    Header { name: String, value: String },
    /// Match the request's resolved `flow_id`.
    FlowId(String),
}

/// Parse every `match=` query parameter into clauses.
///
/// Returns `Err` for a `match` value that is neither `header:<Name>=<Value>` nor
/// `flow_id=<Value>`: a malformed filter must not silently fall back to returning
/// every request, which would cross-contaminate correlated scenarios.
pub(crate) fn parse_match_clauses(query: Option<&str>) -> Result<Vec<MatchClause>, String> {
    let mut clauses = Vec::new();
    let Some(q) = query else {
        return Ok(clauses);
    };
    for pair in q.split('&') {
        let Some((key, raw_value)) = pair.split_once('=') else {
            continue;
        };
        if key != "match" {
            continue;
        }
        let value = urlencoding::decode(raw_value)
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| raw_value.to_string());
        clauses.push(parse_one(&value)?);
    }
    Ok(clauses)
}

fn parse_one(value: &str) -> Result<MatchClause, String> {
    if let Some(rest) = value.strip_prefix("header:") {
        let (name, header_value) = rest.split_once('=').ok_or_else(|| {
            format!("invalid match clause '{value}' (expected header:<Name>=<Value>)")
        })?;
        if name.is_empty() {
            return Err(format!(
                "invalid match clause '{value}' (empty header name)"
            ));
        }
        Ok(MatchClause::Header {
            name: name.to_string(),
            value: header_value.to_string(),
        })
    } else if let Some(flow_id) = value.strip_prefix("flow_id=") {
        Ok(MatchClause::FlowId(flow_id.to_string()))
    } else {
        Err(format!(
            "unsupported match clause '{value}' (expected header:<Name>=<Value> or flow_id=<Value>)"
        ))
    }
}

/// Resolve a recorded request's `flow_id` from the imposter's `flow_id_source`:
/// `"imposter_port"` → the port; `"header:<Name>"` → the header value if present.
fn resolve_flow_id(req: &RecordedRequest, flow_id_source: &str, port: u16) -> Option<String> {
    if flow_id_source == "imposter_port" {
        Some(port.to_string())
    } else if let Some(name) = flow_id_source.strip_prefix("header:") {
        header_value(req, name)
    } else {
        None
    }
}

/// Case-insensitive header lookup against a recorded request.
fn header_value(req: &RecordedRequest, name: &str) -> Option<String> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// Does `req` satisfy ALL clauses (AND)?
pub(crate) fn request_matches(
    req: &RecordedRequest,
    clauses: &[MatchClause],
    flow_id_source: &str,
    port: u16,
) -> bool {
    clauses.iter().all(|clause| match clause {
        MatchClause::Header { name, value } => {
            header_value(req, name).as_deref() == Some(value.as_str())
        }
        MatchClause::FlowId(value) => {
            resolve_flow_id(req, flow_id_source, port).as_deref() == Some(value.as_str())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn req_with_header(name: &str, value: &str) -> RecordedRequest {
        let mut headers = HashMap::new();
        headers.insert(name.to_string(), value.to_string());
        RecordedRequest {
            request_from: "127.0.0.1".to_string(),
            method: "GET".to_string(),
            path: "/".to_string(),
            query: HashMap::new(),
            headers,
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn parses_header_and_flow_id_clauses() {
        let clauses =
            parse_match_clauses(Some("match=header:X-Mock-Space=abc&match=flow_id=xyz")).unwrap();
        assert_eq!(
            clauses,
            vec![
                MatchClause::Header {
                    name: "X-Mock-Space".to_string(),
                    value: "abc".to_string()
                },
                MatchClause::FlowId("xyz".to_string()),
            ]
        );
    }

    #[test]
    fn parses_url_encoded_clause() {
        let clauses = parse_match_clauses(Some("match=header%3AX-Mock-Space%3Dabc")).unwrap();
        assert_eq!(
            clauses,
            vec![MatchClause::Header {
                name: "X-Mock-Space".to_string(),
                value: "abc".to_string()
            }]
        );
    }

    #[test]
    fn no_match_param_is_empty_clause_set() {
        assert!(parse_match_clauses(None).unwrap().is_empty());
        assert!(parse_match_clauses(Some("foo=bar")).unwrap().is_empty());
    }

    #[test]
    fn rejects_unsupported_clause() {
        assert!(parse_match_clauses(Some("match=path=/foo")).is_err());
        assert!(parse_match_clauses(Some("match=header:X-Name")).is_err());
    }

    #[test]
    fn header_match_is_case_insensitive_on_name() {
        let req = req_with_header("X-Mock-Space", "abc");
        let clauses = vec![MatchClause::Header {
            name: "x-mock-space".to_string(),
            value: "abc".to_string(),
        }];
        assert!(request_matches(&req, &clauses, "imposter_port", 4545));
    }

    #[test]
    fn flow_id_resolves_via_header_source() {
        let req = req_with_header("X-Mock-Space", "abc");
        let clauses = vec![MatchClause::FlowId("abc".to_string())];
        assert!(request_matches(&req, &clauses, "header:X-Mock-Space", 4545));
        let clauses = vec![MatchClause::FlowId("other".to_string())];
        assert!(!request_matches(
            &req,
            &clauses,
            "header:X-Mock-Space",
            4545
        ));
    }

    #[test]
    fn flow_id_resolves_via_imposter_port() {
        let req = req_with_header("X-Mock-Space", "abc");
        assert!(request_matches(
            &req,
            &[MatchClause::FlowId("4545".to_string())],
            "imposter_port",
            4545
        ));
        assert!(!request_matches(
            &req,
            &[MatchClause::FlowId("9999".to_string())],
            "imposter_port",
            4545
        ));
    }

    #[test]
    fn multiple_clauses_are_anded() {
        let mut req = req_with_header("X-Mock-Space", "abc");
        req.headers.insert("X-Tenant".to_string(), "t1".to_string());
        let clauses = vec![
            MatchClause::Header {
                name: "X-Mock-Space".to_string(),
                value: "abc".to_string(),
            },
            MatchClause::Header {
                name: "X-Tenant".to_string(),
                value: "t1".to_string(),
            },
        ];
        assert!(request_matches(&req, &clauses, "imposter_port", 4545));
        let clauses = vec![
            MatchClause::Header {
                name: "X-Mock-Space".to_string(),
                value: "abc".to_string(),
            },
            MatchClause::Header {
                name: "X-Tenant".to_string(),
                value: "WRONG".to_string(),
            },
        ];
        assert!(!request_matches(&req, &clauses, "imposter_port", 4545));
    }
}
