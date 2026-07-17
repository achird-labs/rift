//! Server-side filtering of recorded requests by `match=` query clauses.
//!
//! Supports `header:<Name>=<Value>`, `flow_id=<Value>` (the latter resolved via
//! the imposter's `flow_id_source`), `method=<Verb>`, and `path=<Path>` — the last
//! two exact-equality against the recorded request. Multiple clauses are AND'd together.

use crate::imposter::RecordedRequest;

/// Error parsing a `match=` filter clause. Its `Display` is surfaced verbatim as the
/// `400 Bad Request` body, so the wording is part of the admin API contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MatchClauseError {
    #[error("invalid match clause '{0}' (expected header:<Name>=<Value>)")]
    MissingHeaderValue(String),
    #[error("invalid match clause '{0}' (empty header name)")]
    EmptyHeaderName(String),
    #[error(
        "unsupported match clause '{0}' (expected header:<Name>=<Value>, flow_id=<Value>, method=<Verb> or path=<Path>)"
    )]
    Unsupported(String),
}

/// A single parsed `match=` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MatchClause {
    /// Match a request header by (case-insensitive) name and exact value.
    Header { name: String, value: String },
    /// Match the request's resolved `flow_id`.
    FlowId(String),
    /// Match the request's method by exact, case-sensitive equality.
    Method(String),
    /// Match the request's bare path by exact equality (`query` is a separate field).
    Path(String),
}

/// Parse every `match=` query parameter into clauses.
///
/// Returns `Err` for a `match` value outside the closed grammar (`header:<Name>=<Value>`,
/// `flow_id=<Value>`, `method=<Verb>`, `path=<Path>`): a malformed filter must not silently
/// fall back to returning every request, which would cross-contaminate correlated scenarios.
pub(crate) fn parse_match_clauses(
    query: Option<&str>,
) -> Result<Vec<MatchClause>, MatchClauseError> {
    let mut clauses = Vec::new();
    for (key, value) in query_pairs(query) {
        if key == "match" {
            clauses.push(parse_one(&value)?);
        }
    }
    Ok(clauses)
}

/// Decoded `key=value` pairs of a query string. Pairs without `=` are skipped, and a value that
/// is not valid percent-encoding is passed through raw so it fails the caller's own validation
/// rather than the decoder's.
fn query_pairs(query: Option<&str>) -> impl Iterator<Item = (&str, String)> {
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|pair| {
            let (key, raw_value) = pair.split_once('=')?;
            let value = urlencoding::decode(raw_value)
                .map(|v| v.into_owned())
                .unwrap_or_else(|_| raw_value.to_string());
            Some((key, value))
        })
}

/// Rejected `?since=` cursor. Like [`MatchClauseError`], the message is served verbatim in a
/// `400 Bad Request` body and is therefore part of the admin API contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid since cursor '{0}' (expected a non-negative integer)")]
pub(crate) struct SinceError(String);

/// Parse the `?since=<u64>` cursor (issue #603). `None` = no cursor requested, i.e. a baseline
/// read of everything retained. Unknown keys are skipped, so `since` composes with `match`.
pub(crate) fn parse_since(query: Option<&str>) -> Result<Option<u64>, SinceError> {
    let mut since = None;
    for (key, value) in query_pairs(query) {
        if key == "since" {
            since = Some(value.parse::<u64>().map_err(move |_| SinceError(value))?);
        }
    }
    Ok(since)
}

fn parse_one(value: &str) -> Result<MatchClause, MatchClauseError> {
    if let Some(rest) = value.strip_prefix("header:") {
        let (name, header_value) = rest
            .split_once('=')
            .ok_or_else(|| MatchClauseError::MissingHeaderValue(value.to_string()))?;
        if name.is_empty() {
            return Err(MatchClauseError::EmptyHeaderName(value.to_string()));
        }
        Ok(MatchClause::Header {
            name: name.to_string(),
            value: header_value.to_string(),
        })
    } else if let Some(flow_id) = value.strip_prefix("flow_id=") {
        Ok(MatchClause::FlowId(flow_id.to_string()))
    } else if let Some(method) = value.strip_prefix("method=") {
        Ok(MatchClause::Method(method.to_string()))
    } else if let Some(path) = value.strip_prefix("path=") {
        Ok(MatchClause::Path(path.to_string()))
    } else {
        Err(MatchClauseError::Unsupported(value.to_string()))
    }
}

/// Resolve a recorded request's `flow_id` from the imposter's `flow_id_source`:
/// `"imposter_port"` → the port; `"header:<Name>"` → the header value if present.
fn resolve_flow_id(req: &RecordedRequest, flow_id_source: &str, port: u16) -> Option<String> {
    if flow_id_source == "imposter_port" {
        Some(port.to_string())
    } else if let Some(name) = flow_id_source.strip_prefix("header:") {
        // A flow id derives from a single header value; take the first if multi-valued (#238).
        header_values(req, name).and_then(|values| values.first().cloned())
    } else {
        None
    }
}

/// Case-insensitive header lookup against a recorded request — returns all values for the key.
fn header_values<'a>(req: &'a RecordedRequest, name: &str) -> Option<&'a Vec<String>> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
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
            // Match if ANY recorded value for the header equals the target (#238 multi-value).
            header_values(req, name).is_some_and(|values| values.iter().any(|v| v == value))
        }
        MatchClause::FlowId(value) => {
            resolve_flow_id(req, flow_id_source, port).as_deref() == Some(value.as_str())
        }
        MatchClause::Method(value) => req.method == *value,
        MatchClause::Path(value) => req.path == *value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::ResponseMode;
    use std::collections::HashMap;

    fn req_with_header(name: &str, value: &str) -> RecordedRequest {
        let mut headers = HashMap::new();
        headers.insert(name.to_string(), vec![value.to_string()]);
        RecordedRequest {
            mode: ResponseMode::Text,
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
        // `query=`/`body=` are deliberately outside the closed grammar.
        assert!(parse_match_clauses(Some("match=query=a=b")).is_err());
        assert!(parse_match_clauses(Some("match=body=x")).is_err());
        assert!(parse_match_clauses(Some("match=header:X-Name")).is_err());
    }

    #[test]
    fn unsupported_clause_message_enumerates_the_grammar() {
        // The Display text is served verbatim as the 400 body, so it is part of the admin API
        // contract — assert it exactly.
        let err = parse_match_clauses(Some("match=query=a")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unsupported match clause 'query=a' (expected header:<Name>=<Value>, flow_id=<Value>, method=<Verb> or path=<Path>)"
        );
    }

    #[test]
    fn parses_method_and_path_clauses() {
        let clauses = parse_match_clauses(Some("match=method=POST&match=path=/orders")).unwrap();
        assert_eq!(
            clauses,
            vec![
                MatchClause::Method("POST".to_string()),
                MatchClause::Path("/orders".to_string()),
            ]
        );
    }

    #[test]
    fn parses_url_encoded_method_and_path() {
        // A path carrying structural `=`/spaces survives one level of percent-decoding intact.
        let clauses = parse_match_clauses(Some("match=path%3D%2Ffoo%20bar")).unwrap();
        assert_eq!(clauses, vec![MatchClause::Path("/foo bar".to_string())]);
        let clauses = parse_match_clauses(Some("match=path%3D%2Fa%3Db")).unwrap();
        assert_eq!(clauses, vec![MatchClause::Path("/a=b".to_string())]);
    }

    #[test]
    fn method_match_is_case_sensitive() {
        let mut req = req_with_header("X-Mock-Space", "abc");
        req.method = "GET".to_string();
        assert!(request_matches(
            &req,
            &[MatchClause::Method("GET".to_string())],
            "imposter_port",
            4545
        ));
        assert!(
            !request_matches(
                &req,
                &[MatchClause::Method("get".to_string())],
                "imposter_port",
                4545
            ),
            "method comparison is case-sensitive"
        );
    }

    #[test]
    fn path_match_is_exact_not_prefix() {
        let mut req = req_with_header("X-Mock-Space", "abc");
        req.path = "/foo".to_string();
        assert!(request_matches(
            &req,
            &[MatchClause::Path("/foo".to_string())],
            "imposter_port",
            4545
        ));
        assert!(
            !request_matches(
                &req,
                &[MatchClause::Path("/foo/bar".to_string())],
                "imposter_port",
                4545
            ),
            "path comparison is exact, not a prefix"
        );
    }

    #[test]
    fn method_path_and_header_and_together() {
        let mut req = req_with_header("X-Mock-Space", "abc");
        req.method = "POST".to_string();
        req.path = "/orders".to_string();
        let clauses = vec![
            MatchClause::Method("POST".to_string()),
            MatchClause::Path("/orders".to_string()),
            MatchClause::Header {
                name: "X-Mock-Space".to_string(),
                value: "abc".to_string(),
            },
        ];
        assert!(request_matches(&req, &clauses, "imposter_port", 4545));
        // One mismatched clause fails the whole AND.
        let clauses = vec![
            MatchClause::Method("GET".to_string()),
            MatchClause::Path("/orders".to_string()),
        ];
        assert!(!request_matches(&req, &clauses, "imposter_port", 4545));
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
        req.headers
            .insert("X-Tenant".to_string(), vec!["t1".to_string()]);
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
