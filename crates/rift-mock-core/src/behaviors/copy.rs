//! Copy behavior - copy request fields into response.

use super::extraction::ExtractionMethod;
use super::request::RequestContext;
use super::spliced::{Spliced, SplicedHeaders, authored_headers, plain_headers};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

/// Copy behavior - copy request fields into response
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CopyBehavior {
    /// Request field to copy from
    pub from: CopySource,
    /// Response token to replace (e.g., "${NAME}")
    pub into: String,
    /// Extraction method
    #[serde(rename = "using")]
    pub extraction: ExtractionMethod,
}

/// Source of data to copy from request
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum CopySource {
    /// Simple field: "path", "body", "method"
    Simple(String),
    /// Nested field: {"query": "name"} or {"headers": "Content-Type"}
    Nested(HashMap<String, String>),
}

impl CopySource {
    /// Extract value from request data
    pub fn extract(&self, request: &RequestContext) -> Option<String> {
        match self {
            CopySource::Simple(field) => match field.as_str() {
                "path" => Some(request.path.clone()),
                "method" => Some(request.method.clone()),
                // A request without a body has the body `""` in Mountebank (issue #1365), so a
                // selector that matches nothing still runs against it.
                "body" => Some(request.body.clone().unwrap_or_default()),
                _ => None,
            },
            CopySource::Nested(map) => {
                if let Some(param_name) = map.get("query") {
                    // A repeated key gives its first value, as Mountebank's `getFrom` does for a
                    // multi-valued request field (issue #1553).
                    request
                        .query
                        .get(param_name)
                        .and_then(|v| v.first())
                        .cloned()
                } else if let Some(header_name) = map.get("headers") {
                    // Case-insensitive header lookup since HTTP headers are case-insensitive
                    let lower_name = header_name.to_lowercase();
                    request
                        .headers
                        .iter()
                        .find(|(k, _)| k.to_lowercase() == lower_name)
                        .map(|(_, v)| v.clone())
                } else {
                    None
                }
            }
        }
    }
}

/// Apply copy behaviors to response body. Treats `body` and `headers` as authored text; the
/// serve path uses [`apply_copy_spliced`] so text an earlier pass inserted is not re-scanned.
pub fn apply_copy_behaviors(
    body: &str,
    headers: &mut HashMap<String, Vec<String>>,
    behaviors: &[CopyBehavior],
    request: &RequestContext,
    stub: crate::imposter::headers::StubRef<'_>,
) -> String {
    let mut spliced_body = Spliced::authored(body.to_string());
    let mut spliced_headers = authored_headers(std::mem::take(headers));
    apply_copy_spliced(
        &mut spliced_body,
        &mut spliced_headers,
        behaviors,
        request,
        stub,
    );
    *headers = plain_headers(spliced_headers);
    spliced_body.into_text()
}

/// Apply copy behaviors, replacing only tokens that are not wholly inside text an earlier pass
/// inserted (issue #1203). What a copy inserts is recorded as inserted, so a later `lookup` never
/// expands a token the client sent.
pub(crate) fn apply_copy_spliced(
    body: &mut Spliced,
    headers: &mut SplicedHeaders,
    behaviors: &[CopyBehavior],
    request: &RequestContext,
    stub: crate::imposter::headers::StubRef<'_>,
) {
    for behavior in behaviors {
        // Mountebank (behaviors.js replaceArrayValuesIn) only replaces a token it has a value
        // for: an absent source or a selector that extracts nothing leaves the token as authored.
        let Some(source) = behavior.from.extract(request) else {
            continue;
        };

        // Only the indices the response actually spells are extracted and scanned for, so a
        // multi-node selector over a large request costs no more than the tokens written.
        let indices = written_indices(&behavior.into, body, headers);
        if indices.is_empty() {
            let Some(bare) = behavior.extraction.extract(&source) else {
                continue;
            };
            replace_copy_token(body, headers, &behavior.into, &bare, stub);
            continue;
        }

        let values = behavior.extraction.matches(&source);
        let Some(bare) = bare_value(&behavior.extraction, &values) else {
            continue;
        };

        // `${into}[i]` first, so the bare pass below cannot rewrite an indexed token's prefix. A
        // group that did not take part in the match is "" (Mountebank emits `,` from
        // `join(undefined)`). An index with no value is left to the bare pass, as in Mountebank.
        for i in indices {
            let Some(value) = values.get(i) else {
                continue;
            };
            let token = format!("{}[{i}]", behavior.into);
            replace_copy_token(body, headers, &token, value.as_deref().unwrap_or(""), stub);
        }
        replace_copy_token(body, headers, &behavior.into, bare, stub);
    }
}

/// The indices of every `${into}[n]` the body and header values spell, `n` written as Mountebank
/// writes an array index (no sign, no leading zero): Mountebank replaces `${into}[${i}]` for each
/// value literally, so `$C[01]` is never an indexed token.
fn written_indices(into: &str, body: &Spliced, headers: &SplicedHeaders) -> BTreeSet<usize> {
    let prefix = format!("{into}[");
    let mut indices = BTreeSet::new();
    for text in std::iter::once(body).chain(headers.values().flatten()) {
        for (at, _) in text.as_str().match_indices(&prefix) {
            let rest = &text.as_str()[at + prefix.len()..];
            let len = rest.bytes().take_while(u8::is_ascii_digit).count();
            let digits = &rest[..len];
            let canonical = len > 0 && (digits == "0" || !digits.starts_with('0'));
            if canonical
                && rest[len..].starts_with(']')
                && let Ok(i) = digits.parse()
            {
                indices.insert(i);
            }
        }
    }
    indices
}

/// The value of the bare `${into}` token — Rift's documented choice: a regex's first capture
/// group, else its whole match; the first JSONPath or XPath node. `None` when nothing matched.
fn bare_value<'a>(method: &ExtractionMethod, values: &'a [Option<String>]) -> Option<&'a str> {
    let whole = values.first().and_then(Option::as_deref);
    match method {
        ExtractionMethod::Regex { .. } => values.get(1).and_then(Option::as_deref).or(whole),
        ExtractionMethod::JsonPath { .. } | ExtractionMethod::XPath { .. } => whole,
    }
}

/// Replace one copy token in the body and in every header value.
fn replace_copy_token(
    body: &mut Spliced,
    headers: &mut SplicedHeaders,
    token: &str,
    replacement: &str,
    stub: crate::imposter::headers::StubRef<'_>,
) {
    body.replace_token(token, replacement);

    // Also replace in headers — per value, so multi-value headers (e.g. multiple Set-Cookie)
    // keep their multiplicity (RFC 7230 §3.2.2 forbids folding Set-Cookie).
    //
    // `replacement` is request-derived, so it can carry a byte a header value cannot hold; repair
    // it like the templating passes do (issue #1067). Only the substituted text is repaired — a
    // control character the author typed into the header itself is an authoring bug that must
    // still reach the builder and fail the response.
    //
    // Skipped outright when no header uses this token: the repair warns when it removes
    // something, so running it for a body-only stub would log a header warning, once per
    // request, about a header that does not exist.
    if headers
        .values()
        .flatten()
        .any(|v| v.contains_authored(token))
    {
        let repaired = crate::imposter::headers::sanitize_header_value(replacement, stub);
        for value in headers.values_mut().flatten() {
            value.replace_token(token, &repaired);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture identity for tests that only care about the repair itself, not which stub
    /// triggered it — the gate test below (issue #1075) asserts on deliberately chosen values.
    const FIXTURE_STUB: crate::imposter::headers::StubRef<'static> =
        crate::imposter::headers::StubRef {
            port: 0,
            index: 0,
            id: None,
        };

    #[test]
    fn test_copy_source_simple() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/users/123".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: Some("test body".to_string()),
            ..Default::default()
        };

        let source = CopySource::Simple("path".to_string());
        assert_eq!(source.extract(&request), Some("/users/123".to_string()));

        let source = CopySource::Simple("method".to_string());
        assert_eq!(source.extract(&request), Some("GET".to_string()));

        let source = CopySource::Simple("body".to_string());
        assert_eq!(source.extract(&request), Some("test body".to_string()));
    }

    #[test]
    fn test_copy_source_nested() {
        let mut query = HashMap::new();
        query.insert("name".to_string(), vec!["Alice".to_string()]);

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());

        let request = RequestContext {
            method: "GET".to_string(),
            path: "/test".to_string(),
            query,
            headers,
            body: None,
            ..Default::default()
        };

        let mut map = HashMap::new();
        map.insert("query".to_string(), "name".to_string());
        let source = CopySource::Nested(map);
        assert_eq!(source.extract(&request), Some("Alice".to_string()));

        let mut map = HashMap::new();
        map.insert("headers".to_string(), "Content-Type".to_string());
        let source = CopySource::Nested(map);
        assert_eq!(
            source.extract(&request),
            Some("application/json".to_string())
        );
    }

    #[test]
    fn test_apply_copy_behaviors() {
        let mut query = HashMap::new();
        query.insert("name".to_string(), vec!["Alice".to_string()]);

        let request = RequestContext {
            method: "GET".to_string(),
            path: "/users/123".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![
            CopyBehavior {
                from: CopySource::Simple("path".to_string()),
                into: "${PATH}".to_string(),
                extraction: ExtractionMethod::Regex {
                    selector: r"/users/(\d+)".to_string(),
                    options: None,
                },
            },
            CopyBehavior {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "name".to_string());
                    CopySource::Nested(map)
                },
                into: "${NAME}".to_string(),
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
            },
        ];

        let body = r#"{"userId": "${PATH}", "greeting": "Hello, ${NAME}!"}"#;
        let mut headers = HashMap::new();

        let result = apply_copy_behaviors(body, &mut headers, &behaviors, &request, FIXTURE_STUB);
        assert_eq!(result, r#"{"userId": "123", "greeting": "Hello, Alice!"}"#);
    }

    /// Issue #1067: what `copy` writes into a header comes from the request, so it can carry a
    /// byte a header value cannot hold. Repair it rather than letting it 500 the stub.
    #[test]
    fn copy_repairs_a_header_value_it_rewrote_from_request_data() {
        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["one\rtwo".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![CopyBehavior {
            from: {
                let mut map = HashMap::new();
                map.insert("query".to_string(), "q".to_string());
                CopySource::Nested(map)
            },
            into: "${q}".to_string(),
            extraction: ExtractionMethod::Regex {
                selector: ".*".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-Echo".to_string(), vec!["v=${q}".to_string()]);

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB);

        assert_eq!(
            headers["X-Echo"],
            vec!["v=onetwo".to_string()],
            "the CR the client supplied is removed; without the repair this value is unrepresentable"
        );
    }

    /// A body-only stub must stay silent (issue #1067). The repair logs whenever it removes
    /// something, so running it for a `copy` whose token appears in no header would warn about a
    /// header value that does not exist — once per request, on client-controlled input.
    #[test]
    fn copy_into_a_body_only_stub_does_not_warn_about_headers() {
        let request = RequestContext {
            method: "POST".to_string(),
            path: "/x".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: Some("one\r\ntwo".to_string()),
            ..Default::default()
        };

        let behaviors = vec![CopyBehavior {
            from: CopySource::Simple("body".to_string()),
            into: "${b}".to_string(),
            extraction: ExtractionMethod::Regex {
                selector: "(?s).*".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-Plain".to_string(), vec!["no token here".to_string()]);

        // `captured_logs`, not `traced_test`: see `copy_repair_warning_names_the_stub`. Under
        // `traced_test` this assertion passed even when the repair ran and warned (issue #1079).
        let mut body = String::new();
        let events = crate::test_support::captured_logs(|| {
            body = apply_copy_behaviors("x=${b}", &mut headers, &behaviors, &request, FIXTURE_STUB);
        });

        assert_eq!(
            body, "x=one\r\ntwo",
            "the body keeps the bytes the client sent"
        );
        assert_eq!(headers["X-Plain"], vec!["no token here".to_string()]);
        assert!(
            events.is_empty(),
            "no header used this token, so the header repair must not have run, but it logged {} event(s)",
            events.len()
        );
    }

    /// A repeated header keeps its multiplicity through the repair, and the one warning it does
    /// raise names the stub (issue #1075).
    ///
    /// One warning, not one per value: `copy` repairs the *replacement* once and splices the
    /// repaired text into every value, so a bad byte in the substitution is reported once however
    /// many header lines it lands in. That is deliberate — the warning is client-triggerable, and
    /// N copies of it per request would be N times the log volume for one defect.
    #[test]
    fn every_repaired_value_of_a_multi_value_header_names_the_same_stub() {
        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["one\rtwo".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![CopyBehavior {
            from: {
                let mut map = HashMap::new();
                map.insert("query".to_string(), "q".to_string());
                CopySource::Nested(map)
            },
            into: "${q}".to_string(),
            extraction: ExtractionMethod::Regex {
                selector: ".*".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert(
            "Set-Cookie".to_string(),
            vec!["a=${q}".to_string(), "b=${q}".to_string()],
        );

        let events = crate::test_support::captured_logs(|| {
            apply_copy_behaviors(
                "",
                &mut headers,
                &behaviors,
                &request,
                crate::imposter::headers::StubRef {
                    port: 4545,
                    index: 2,
                    id: Some("echo"),
                },
            );
        });

        assert_eq!(
            headers["Set-Cookie"],
            vec!["a=onetwo".to_string(), "b=onetwo".to_string()],
            "both cookie lines survive and are both repaired"
        );
        assert_eq!(
            events.len(),
            1,
            "the replacement is repaired once, not once per header line"
        );
        assert_eq!(events[0].field("port"), Some("4545"));
        assert_eq!(events[0].field("stub"), Some("2"));
        assert_eq!(events[0].field("stub_id"), Some("echo"));
    }

    /// Issue #1075: the repair `copy` triggers must name the stub too, or an operator sees a
    /// mangled header with no way back to which behavior produced it.
    #[test]
    fn copy_repair_warning_names_the_stub() {
        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["one\rtwo".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![CopyBehavior {
            from: {
                let mut map = HashMap::new();
                map.insert("query".to_string(), "q".to_string());
                CopySource::Nested(map)
            },
            into: "${q}".to_string(),
            extraction: ExtractionMethod::Regex {
                selector: ".*".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-Echo".to_string(), vec!["v=${q}".to_string()]);

        // `crate::test_support::captured_logs`, not `tracing_test::traced_test`: that macro
        // installs an `EnvFilter` of `"<crate_name>=trace"`, which drops an event raised on the
        // `rift::template` target, so a `logs_contain` assertion there passes against an
        // implementation that logs nothing at all.
        let events = crate::test_support::captured_logs(|| {
            apply_copy_behaviors(
                "",
                &mut headers,
                &behaviors,
                &request,
                crate::imposter::headers::StubRef {
                    port: 4545,
                    index: 2,
                    id: Some("echo"),
                },
            );
        });

        assert_eq!(headers["X-Echo"], vec!["v=onetwo".to_string()]);
        assert_eq!(events.len(), 1, "one repair, one warning");
        assert_eq!(events[0].field("port"), Some("4545"));
        assert_eq!(events[0].field("stub"), Some("2"));
        assert_eq!(events[0].field("stub_id"), Some("echo"));
    }

    /// Only the substituted text is repaired. A literal control character the author typed beside
    /// the token survives, so the response still fails loudly (issue #1067).
    #[test]
    fn copy_repairs_the_substituted_text_but_not_a_literal_control_char_beside_it() {
        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["one\rtwo".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![CopyBehavior {
            from: {
                let mut map = HashMap::new();
                map.insert("query".to_string(), "q".to_string());
                CopySource::Nested(map)
            },
            into: "${q}".to_string(),
            extraction: ExtractionMethod::Regex {
                selector: ".*".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-Echo".to_string(), vec!["bad\nvalue=${q}".to_string()]);

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB);

        assert_eq!(
            headers["X-Echo"],
            vec!["bad\nvalue=onetwo".to_string()],
            "the CR from the request is gone; the author's literal newline is not"
        );
    }

    /// The boundary (issue #1067). A header value this behavior never rewrote is a literal from
    /// the config; repairing it would hide an authoring bug that the response builder is supposed
    /// to surface as a 500. Only a value that actually contained the token is touched.
    #[test]
    fn copy_leaves_a_header_value_without_the_token_untouched() {
        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![CopyBehavior {
            from: {
                let mut map = HashMap::new();
                map.insert("query".to_string(), "q".to_string());
                CopySource::Nested(map)
            },
            into: "${q}".to_string(),
            extraction: ExtractionMethod::Regex {
                selector: ".*".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-Literal".to_string(), vec!["bad\nvalue".to_string()]);

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB);

        assert_eq!(
            headers["X-Literal"],
            vec!["bad\nvalue".to_string()],
            "a literal config value must reach the builder unrepaired and fail loudly"
        );
    }

    /// Same boundary on the source-missing path, which leaves the token as authored (issue
    /// #1402). Nothing is substituted, so nothing is repaired.
    #[test]
    fn copy_with_a_missing_source_does_not_repair_a_literal_config_value() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![CopyBehavior {
            from: {
                let mut map = HashMap::new();
                map.insert("query".to_string(), "q".to_string());
                CopySource::Nested(map)
            },
            into: "${q}".to_string(),
            extraction: ExtractionMethod::Regex {
                selector: ".*".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-Echo".to_string(), vec!["bad\nvalue=${q}".to_string()]);

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB);

        assert_eq!(
            headers["X-Echo"],
            vec!["bad\nvalue=${q}".to_string()],
            "the token is kept; the literal newline beside it is left to fail loudly"
        );
    }

    #[test]
    fn copy_preserves_multi_value_headers_and_substitutes_each() {
        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![CopyBehavior {
            from: {
                let mut map = HashMap::new();
                map.insert("query".to_string(), "q".to_string());
                CopySource::Nested(map)
            },
            into: "${q}".to_string(),
            extraction: ExtractionMethod::Regex {
                selector: ".*".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert(
            "Set-Cookie".to_string(),
            vec!["a=1".to_string(), "b=${q}".to_string()],
        );

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB);

        // Both cookie lines survive (no fold) and the token is substituted in place.
        assert_eq!(
            headers["Set-Cookie"],
            vec!["a=1".to_string(), "b=hi".to_string()]
        );
    }

    #[test]
    fn copy_missing_source_keeps_token_per_value_without_folding() {
        // No `q` query param → the source is absent, so the token is left as authored in each
        // value (Mountebank parity, issue #1402); the multiplicity survives.
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![CopyBehavior {
            from: {
                let mut map = HashMap::new();
                map.insert("query".to_string(), "q".to_string());
                CopySource::Nested(map)
            },
            into: "${q}".to_string(),
            extraction: ExtractionMethod::Regex {
                selector: ".*".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert(
            "Set-Cookie".to_string(),
            vec!["a=1".to_string(), "b=${q}".to_string()],
        );

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB);

        assert_eq!(
            headers["Set-Cookie"],
            vec!["a=1".to_string(), "b=${q}".to_string()]
        );
    }

    /// Runs one copy from `from` into `$C` over `body` and an `X-C: <$C>` header, returning both.
    fn copy_once(
        from: &str,
        extraction: serde_json::Value,
        request: &RequestContext,
        body: &str,
    ) -> (String, Vec<String>) {
        let behavior: CopyBehavior = serde_json::from_value(serde_json::json!({
            "from": serde_json::from_str::<serde_json::Value>(from).expect("from is JSON"),
            "into": "$C",
            "using": extraction,
        }))
        .expect("the copy parses");
        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-C".to_string(), vec!["<$C>".to_string()]);
        let served = apply_copy_behaviors(body, &mut headers, &[behavior], request, FIXTURE_STUB);
        (served, headers.remove("X-C").expect("header kept"))
    }

    fn request_with_body(body: &str) -> RequestContext {
        RequestContext {
            method: "POST".to_string(),
            path: "/abc".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: Some(body.to_string()),
            ..Default::default()
        }
    }

    /// Issue #1402: Mountebank's `replaceArrayValuesIn` replaces a token only when it has a
    /// value for it, so a regex that does not match leaves `$C` as authored.
    #[test]
    fn a_regex_that_does_not_match_leaves_the_token_in_body_and_header() {
        let (body, header) = copy_once(
            r#""path""#,
            serde_json::json!({"method": "regex", "selector": "\\d+"}),
            &request_with_body(""),
            "[abc][$C]",
        );
        assert_eq!(body, "[abc][$C]");
        assert_eq!(header, vec!["<$C>".to_string()]);
    }

    /// `ABC` matches the path `/abc` only under `ignoreCase`: without it nothing is extracted
    /// and the token stays; with it the match is copied.
    #[test]
    fn a_regex_that_matches_only_under_ignore_case_leaves_the_token_without_it() {
        let using = |ignore_case: bool| {
            serde_json::json!({
                "method": "regex",
                "selector": "ABC",
                "options": {"ignoreCase": ignore_case}
            })
        };
        let (body, header) = copy_once(r#""path""#, using(false), &request_with_body(""), "[$C]");
        assert_eq!(body, "[$C]");
        assert_eq!(header, vec!["<$C>".to_string()]);

        let (body, header) = copy_once(r#""path""#, using(true), &request_with_body(""), "[$C]");
        assert_eq!(body, "[abc]");
        assert_eq!(header, vec!["<abc>".to_string()]);
    }

    #[test]
    fn a_jsonpath_that_selects_nothing_leaves_the_token() {
        let (body, header) = copy_once(
            r#""body""#,
            serde_json::json!({"method": "jsonpath", "selector": "$.missing"}),
            &request_with_body(r#"{"present": 1}"#),
            "[$C]",
        );
        assert_eq!(body, "[$C]");
        assert_eq!(header, vec!["<$C>".to_string()]);
    }

    #[test]
    fn an_xpath_that_selects_nothing_leaves_the_token() {
        let (body, header) = copy_once(
            r#""body""#,
            serde_json::json!({"method": "xpath", "selector": "//missing"}),
            &request_with_body("<doc><present>1</present></doc>"),
            "[$C]",
        );
        assert_eq!(body, "[$C]");
        assert_eq!(header, vec!["<$C>".to_string()]);
    }

    #[test]
    fn an_absent_query_source_leaves_the_token() {
        let (body, header) = copy_once(
            r#"{"query": "id"}"#,
            serde_json::json!({"method": "regex", "selector": "(\\d+)"}),
            &request_with_body(""),
            "[$C]",
        );
        assert_eq!(body, "[$C]");
        assert_eq!(header, vec!["<$C>".to_string()]);
    }

    /// A request without a body copies from `""`, as in Mountebank, so `.*` empties the token
    /// rather than leaving it as an absent source would.
    #[test]
    fn a_request_without_a_body_copies_from_the_empty_body() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };
        let (body, header) = copy_once(
            r#""body""#,
            serde_json::json!({"method": "regex", "selector": ".*"}),
            &request,
            "[$C]",
        );
        assert_eq!(body, "[]");
        assert_eq!(header, vec!["<>".to_string()]);
    }

    /// The no-match `continue` must not stop later copies from running.
    #[test]
    fn a_copy_that_extracts_nothing_does_not_stop_the_next_copy() {
        let behaviors: Vec<CopyBehavior> = serde_json::from_value(serde_json::json!([
            {"from": "path", "into": "$A", "using": {"method": "regex", "selector": "\\d+"}},
            {"from": "path", "into": "$B", "using": {"method": "regex", "selector": "abc"}}
        ]))
        .expect("the copies parse");
        let served = apply_copy_behaviors(
            "$A/$B",
            &mut HashMap::new(),
            &behaviors,
            &request_with_body(""),
            FIXTURE_STUB,
        );
        assert_eq!(served, "$A/abc");
    }

    fn regex(selector: &str) -> serde_json::Value {
        serde_json::json!({"method": "regex", "selector": selector})
    }

    /// Issue #1407: Mountebank's `replaceArrayValuesIn` replaces `${into}[i]` with the i-th value
    /// of the regex's `exec` array — the whole match at 0, then each group.
    #[test]
    fn indexed_tokens_take_each_capture_group() {
        let (body, header) = copy_once(
            r#""body""#,
            regex(r"(\w+)-(\w+)"),
            &request_with_body("ab-cd"),
            "$C[1]-$C[2]|$C[0]|$C",
        );
        assert_eq!(body, "ab-cd|ab-cd|ab");
        assert_eq!(header, vec!["<ab>".to_string()]);
    }

    #[test]
    fn index_zero_is_the_whole_match() {
        let (body, _) = copy_once(
            r#""body""#,
            regex(r"\d+"),
            &request_with_body("x42"),
            "$C[0]",
        );
        assert_eq!(body, "42");
    }

    #[test]
    fn a_jsonpath_index_selects_that_node() {
        let (body, _) = copy_once(
            r#""body""#,
            serde_json::json!({"method": "jsonpath", "selector": "$..id"}),
            &request_with_body(r#"[{"id":1},{"id":2}]"#),
            "$C[0],$C[1]|$C",
        );
        assert_eq!(body, "1,2|1");
    }

    #[test]
    fn an_xpath_index_selects_that_node_in_document_order() {
        let (body, _) = copy_once(
            r#""body""#,
            serde_json::json!({"method": "xpath", "selector": "//n"}),
            &request_with_body("<d><n>a</n><n>b</n><n>c</n></d>"),
            "$C[2]$C[1]$C[0]|$C",
        );
        assert_eq!(body, "cba|a");
    }

    /// Mountebank has no index parser: an index past the values, or one it never spells
    /// (`[01]`), is not an indexed token, so the bare pass rewrites its prefix.
    #[test]
    fn an_out_of_range_or_non_canonical_index_has_its_prefix_rewritten() {
        let (body, _) = copy_once(
            r#""body""#,
            regex(r"(\w+)-(\w+)"),
            &request_with_body("ab-cd"),
            "$C[5]|$C[01]",
        );
        assert_eq!(body, "ab[5]|ab[01]");
    }

    /// A group that did not take part in the match is `""` (Mountebank emits `,` through
    /// `join(undefined)`; documented deviation).
    #[test]
    fn a_non_participating_group_is_empty() {
        let (body, _) = copy_once(
            r#""body""#,
            regex("(a)?(b)"),
            &request_with_body("b"),
            "[$C[1]][$C[2]][$C]",
        );
        assert_eq!(body, "[][b][b]");
    }

    #[test]
    fn indexed_tokens_are_replaced_in_each_value_of_a_multi_value_header() {
        let behavior: CopyBehavior = serde_json::from_value(serde_json::json!({
            "from": "body", "into": "$C", "using": regex(r"(\w+)-(\w+)")
        }))
        .expect("the copy parses");
        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert(
            "Set-Cookie".to_string(),
            vec![
                "a=$C[1]".to_string(),
                "b=$C[2]".to_string(),
                "c=1".to_string(),
            ],
        );
        apply_copy_behaviors(
            "",
            &mut headers,
            &[behavior],
            &request_with_body("ab-cd"),
            FIXTURE_STUB,
        );
        assert_eq!(
            headers["Set-Cookie"],
            vec!["a=ab".to_string(), "b=cd".to_string(), "c=1".to_string()]
        );
    }

    /// Issue #1203: what a copy inserts is never re-scanned, so a request value that spells an
    /// indexed token is served as sent.
    #[test]
    fn a_request_value_spelling_an_indexed_token_is_not_expanded() {
        let (body, _) = copy_once(
            r#""body""#,
            regex("(.*)"),
            &request_with_body("$C[1]$C"),
            "$C[0]",
        );
        assert_eq!(body, "$C[1]$C");
    }

    /// An authored `$C[${I}]` whose index an earlier copy inserted expands: its first and last
    /// bytes are authored, as with lookup's `${R}[${COL}]`.
    #[test]
    fn an_index_inserted_by_an_earlier_copy_composes() {
        let behaviors: Vec<CopyBehavior> = serde_json::from_value(serde_json::json!([
            {"from": {"query": "i"}, "into": "${I}", "using": regex(r"\d")},
            {"from": "body", "into": "$C", "using": regex(r"(\w+)-(\w+)")}
        ]))
        .expect("the copies parse");
        let mut request = request_with_body("ab-cd");
        request.query.insert("i".to_string(), vec!["2".to_string()]);
        let served = apply_copy_behaviors(
            "$C[${I}]",
            &mut HashMap::new(),
            &behaviors,
            &request,
            FIXTURE_STUB,
        );
        assert_eq!(served, "cd");
    }

    /// Only a canonical index closed by `]` is an indexed token; the scan reads headers too.
    #[test]
    fn written_indices_finds_only_canonical_indexed_tokens() {
        let body = Spliced::authored(
            "$C[0] $C[12] $C[01] $C[] $C[x] $C[3 $D[4] $C[18446744073709551616]".to_string(),
        );
        let headers = authored_headers(HashMap::from([(
            "X-C".to_string(),
            vec!["$C[7]".to_string()],
        )]));
        assert_eq!(
            written_indices("$C", &body, &headers),
            BTreeSet::from([0, 7, 12])
        );
    }
}
