//! Copy behavior - copy request fields into response.

use super::extraction::{ExtractionMethod, XPathSelectorError};
use super::request::RequestContext;
use super::spliced::{
    Spliced, SplicedHeaders, authored_headers, plain_headers, replace_response_token,
};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
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
    /// Request field: "path", "method", "body", "ip" or "requestFrom". The name is matched
    /// case-insensitively, as Mountebank does.
    Simple(String),
    /// Nested field: {"query": "name"}, {"headers": "Content-Type"} or {"form": "name"}. The
    /// container key is exact; the name inside it is matched case-insensitively.
    Nested(HashMap<String, String>),
}

impl CopySource {
    /// Extract value from request data, matching Mountebank's `getFrom` (issue #1590): a string
    /// `from` names a request field, case-insensitively; a nested `from` names its container
    /// (`query`, `headers`, `form`) exactly and the entry inside it case-insensitively.
    pub fn extract(&self, request: &RequestContext) -> Option<String> {
        match self {
            CopySource::Simple(field) => {
                let field = field.to_ascii_lowercase();
                match field.as_str() {
                    "requestfrom" => request.request_from.clone(),
                    "path" => Some(request.path.clone()),
                    "method" => Some(request.method.clone()),
                    // A body that did not decode is Mountebank's `undefined` body: no source
                    // (issue #1584).
                    "body" if request.body_undefined => None,
                    // A request without a body has the body `""` in Mountebank (issue #1365), so
                    // a selector that matches nothing still runs against it.
                    "body" => Some(request.body.clone().unwrap_or_default()),
                    "ip" => request.ip.clone(),
                    // `query`, `headers` and `form` as whole objects are `[object Object]` in
                    // Mountebank; not modelled.
                    _ => None,
                }
            }
            CopySource::Nested(map) => {
                if let Some(name) = map.get("query") {
                    // A repeated key gives its first value, as Mountebank's `getFrom` does for a
                    // multi-valued request field (issue #1553).
                    ignoring_case(&request.query, name).and_then(|v| v.first().cloned())
                } else if let Some(name) = map.get("headers") {
                    ignoring_case(&request.headers, name).cloned()
                } else if let Some(name) = map.get("form") {
                    request
                        .form
                        .as_ref()
                        .and_then(|form| ignoring_case(form, name))
                        .cloned()
                } else {
                    None
                }
            }
        }
    }
}

/// The value under `name`, matched ASCII-case-insensitively as Mountebank's `getKeyIgnoringCase`
/// does. An exact key wins so the result does not depend on map order when both spellings exist.
/// Takes any map's entries, so the insertion-ordered query and form (issue #1596) and the headers
/// share it.
fn ignoring_case<'a, V: 'a>(
    map: impl IntoIterator<Item = (&'a String, &'a V)> + Clone,
    name: &str,
) -> Option<&'a V> {
    map.clone()
        .into_iter()
        .find(|(k, _)| k.as_str() == name)
        .or_else(|| map.into_iter().find(|(k, _)| k.eq_ignore_ascii_case(name)))
        .map(|(_, v)| v)
}

/// Apply copy behaviors to response body. Treats `body` and `headers` as authored text; the
/// serve path uses [`apply_copy_spliced`] so text an earlier pass inserted is not re-scanned.
///
/// # Errors
///
/// An XPath `using` that fails when it runs (issue #1585): Mountebank fails the request.
pub fn apply_copy_behaviors(
    body: &str,
    headers: &mut HashMap<String, Vec<String>>,
    behaviors: &[CopyBehavior],
    request: &RequestContext,
    stub: crate::imposter::headers::StubRef<'_>,
) -> Result<String, XPathSelectorError> {
    let mut spliced_body = Spliced::authored(body.to_string());
    let mut spliced_headers = authored_headers(std::mem::take(headers));
    let applied = apply_copy_spliced(
        &mut spliced_body,
        &mut spliced_headers,
        &mut None,
        behaviors,
        request,
        stub,
    );
    *headers = plain_headers(spliced_headers);
    applied.map(|()| spliced_body.into_text())
}

/// Apply copy behaviors, replacing only tokens that are not wholly inside text an earlier pass
/// inserted (issue #1203). What a copy inserts is recorded as inserted, so a later `lookup` never
/// expands a token the client sent.
///
/// `status` is a `statusCode` string (issue #1558): Mountebank's `globalObjectReplace` replaces
/// tokens in every string field of the response, so it is scanned like a header value.
///
/// An XPath `using` that fails when it runs is the error (issue #1585), with the copies before it
/// applied: Mountebank's `copy` throws, and the request fails with `500`. It runs whether or not
/// the response spells the token, as Mountebank selects before it replaces.
pub(crate) fn apply_copy_spliced(
    body: &mut Spliced,
    headers: &mut SplicedHeaders,
    status: &mut Option<Spliced>,
    behaviors: &[CopyBehavior],
    request: &RequestContext,
    stub: crate::imposter::headers::StubRef<'_>,
) -> Result<(), XPathSelectorError> {
    for behavior in behaviors {
        // Mountebank (behaviors.js replaceArrayValuesIn) only replaces a token it has a value
        // for: a selector that extracts nothing leaves the token as authored. An absent source is
        // `undefined`, which only a regex reads (issue #1583).
        let Some(source) =
            source_or_undefined(&behavior.extraction, behavior.from.extract(request))
        else {
            continue;
        };

        // Only the indices the response actually spells are extracted and scanned for, so a
        // multi-node selector over a large request costs no more than the tokens written.
        let indices = written_indices(&behavior.into, body, headers, status.as_ref());
        if indices.is_empty() {
            let Some(bare) = behavior.extraction.extract(&source)? else {
                continue;
            };
            replace_response_token(body, headers, status, &behavior.into, &bare, stub);
            continue;
        }

        let values = behavior.extraction.matches(&source)?;
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
            replace_response_token(
                body,
                headers,
                status,
                &token,
                value.as_deref().unwrap_or(""),
                stub,
            );
        }
        replace_response_token(body, headers, status, &behavior.into, bare, stub);
    }
    Ok(())
}

/// The indices of every `${into}[n]` the body, the status and the header names and values spell,
/// `n` written as Mountebank writes an array index (no sign, no leading zero): Mountebank replaces
/// `${into}[${i}]` for each value literally, so `$C[01]` is never an indexed token.
fn written_indices(
    into: &str,
    body: &Spliced,
    headers: &SplicedHeaders,
    status: Option<&Spliced>,
) -> BTreeSet<usize> {
    let prefix = format!("{into}[");
    let mut indices = BTreeSet::new();
    let names = headers.keys().map(|name| name.as_str());
    for text in std::iter::once(body.as_str())
        .chain(headers.values().flatten().map(Spliced::as_str))
        .chain(names)
        .chain(status.map(Spliced::as_str))
    {
        for (at, _) in text.match_indices(&prefix) {
            let rest = &text[at + prefix.len()..];
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

/// The text a `copy` or `lookup` selector runs over (issue #1583). A request field that is absent
/// is `undefined` in Mountebank's `getFrom`: `RegExp.exec(undefined)` reads it as the text
/// `undefined`, so a regex still matches it, while JSONPath and XPath select nothing from it.
///
/// What is inserted from `undefined` is request-derived like any other source, so it is recorded
/// as inserted text (issue #1203).
pub(crate) fn source_or_undefined(
    extraction: &ExtractionMethod,
    extracted: Option<String>,
) -> Option<Cow<'static, str>> {
    match (extracted, extraction) {
        (Some(source), _) => Some(Cow::Owned(source)),
        (None, ExtractionMethod::Regex { .. }) => Some(Cow::Borrowed("undefined")),
        (None, ExtractionMethod::JsonPath { .. } | ExtractionMethod::XPath { .. }) => None,
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

    /// Issue #1590: Mountebank's `getFrom` reads a string `from` and a nested inner name
    /// case-insensitively, exact on the container key, and reaches `ip`/`requestFrom`/`form`.
    fn ctx_1590() -> RequestContext {
        RequestContext {
            method: "POST".into(),
            path: "/Pth".into(),
            query: crate::util::OrderedMap::from_iter([(
                "q".into(),
                vec!["qv".into(), "second".into()],
            )]),
            headers: HashMap::from([("X-Test".into(), "hv".into())]),
            body: Some("b".into()),
            ip: Some("::ffff:127.0.0.1".into()),
            request_from: Some("::ffff:127.0.0.1:55333".into()),
            form: Some(crate::util::OrderedMap::from_iter([(
                "f".into(),
                "fv".into(),
            )])),
            ..Default::default()
        }
    }

    fn nested(container: &str, name: &str) -> CopySource {
        CopySource::Nested(HashMap::from([(container.into(), name.into())]))
    }

    #[test]
    fn a_simple_from_matches_the_field_name_case_insensitively() {
        let r = ctx_1590();
        for (from, want) in [
            ("BODY", "b"),
            ("Path", "/Pth"),
            ("METHOD", "POST"),
            ("ip", "::ffff:127.0.0.1"),
            ("IP", "::ffff:127.0.0.1"),
            ("requestFrom", "::ffff:127.0.0.1:55333"),
            ("REQUESTFROM", "::ffff:127.0.0.1:55333"),
        ] {
            assert_eq!(
                CopySource::Simple(from.into()).extract(&r).as_deref(),
                Some(want),
                "from {from}"
            );
        }
        assert_eq!(CopySource::Simple("nope".into()).extract(&r), None);
    }

    #[test]
    fn a_nested_from_matches_the_inner_name_case_insensitively_and_the_container_exactly() {
        let r = ctx_1590();
        assert_eq!(nested("query", "Q").extract(&r).as_deref(), Some("qv"));
        assert_eq!(
            nested("headers", "X-TEST").extract(&r).as_deref(),
            Some("hv")
        );
        assert_eq!(nested("form", "f").extract(&r).as_deref(), Some("fv"));
        assert_eq!(nested("form", "F").extract(&r).as_deref(), Some("fv"));
        assert_eq!(nested("QUERY", "q").extract(&r), None);
        assert_eq!(nested("Headers", "x-test").extract(&r), None);
        assert_eq!(nested("form", "missing").extract(&r), None);
    }

    #[test]
    fn a_form_or_ip_source_without_a_client_is_none() {
        let r = RequestContext::default();
        assert_eq!(nested("form", "f").extract(&r), None);
        assert_eq!(CopySource::Simple("ip".into()).extract(&r), None);
    }

    #[test]
    fn a_lookup_key_from_reads_the_field_name_case_insensitively() {
        let key = super::super::lookup::LookupKey {
            from: CopySource::Simple("PATH".into()),
            extraction: ExtractionMethod::Regex {
                selector: "^/(.+)$".into(),
                options: None,
            },
            index: None,
        };
        assert_eq!(
            key.extract(&ctx_1590())
                .expect("a regex never fails")
                .as_deref(),
            Some("Pth")
        );
    }

    #[test]
    fn test_copy_source_simple() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/users/123".to_string(),
            query: Default::default(),
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

    /// Issue #1584: a body that did not decode is no source at all, unlike a request without a
    /// body, whose body is `""`.
    #[test]
    fn an_undefined_body_is_no_copy_source() {
        let source = CopySource::Simple("body".to_string());
        let undefined = RequestContext {
            body_undefined: true,
            ..Default::default()
        };
        assert_eq!(source.extract(&undefined), None);
        assert_eq!(
            source.extract(&RequestContext::default()),
            Some(String::new())
        );
    }

    #[test]
    fn test_copy_source_nested() {
        let mut query = crate::util::OrderedMap::default();
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
        let mut query = crate::util::OrderedMap::default();
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

        let result = apply_copy_behaviors(body, &mut headers, &behaviors, &request, FIXTURE_STUB)
            .expect("copies");
        assert_eq!(result, r#"{"userId": "123", "greeting": "Hello, Alice!"}"#);
    }

    /// Issue #1067: what `copy` writes into a header comes from the request, so it can carry a
    /// byte a header value cannot hold. Repair it rather than letting it 500 the stub.
    #[test]
    fn copy_repairs_a_header_value_it_rewrote_from_request_data() {
        let mut query = crate::util::OrderedMap::default();
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

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB).expect("copies");

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
            query: Default::default(),
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
            body = apply_copy_behaviors("x=${b}", &mut headers, &behaviors, &request, FIXTURE_STUB)
                .expect("copies");
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
        let mut query = crate::util::OrderedMap::default();
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
            )
            .expect("copies");
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
        let mut query = crate::util::OrderedMap::default();
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
            )
            .expect("copies");
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
        let mut query = crate::util::OrderedMap::default();
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

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB).expect("copies");

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
        let mut query = crate::util::OrderedMap::default();
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

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB).expect("copies");

        assert_eq!(
            headers["X-Literal"],
            vec!["bad\nvalue".to_string()],
            "a literal config value must reach the builder unrepaired and fail loudly"
        );
    }

    /// Same boundary on the source-missing path when the regex does not match `undefined` either,
    /// which leaves the token as authored (issues #1402, #1583). Nothing is substituted, so
    /// nothing is repaired.
    #[test]
    fn copy_with_a_missing_source_does_not_repair_a_literal_config_value() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query: Default::default(),
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
                selector: r"\d+".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-Echo".to_string(), vec!["bad\nvalue=${q}".to_string()]);

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB).expect("copies");

        assert_eq!(
            headers["X-Echo"],
            vec!["bad\nvalue=${q}".to_string()],
            "the token is kept; the literal newline beside it is left to fail loudly"
        );
    }

    #[test]
    fn copy_preserves_multi_value_headers_and_substitutes_each() {
        let mut query = crate::util::OrderedMap::default();
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

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB).expect("copies");

        // Both cookie lines survive (no fold) and the token is substituted in place.
        assert_eq!(
            headers["Set-Cookie"],
            vec!["a=1".to_string(), "b=hi".to_string()]
        );
    }

    #[test]
    fn copy_missing_source_keeps_token_per_value_without_folding() {
        // No `q` query param → the source is `undefined`, which `\d+` does not match, so the token
        // is left as authored in each value (Mountebank parity, issues #1402, #1583); the
        // multiplicity survives.
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query: Default::default(),
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
                selector: r"\d+".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert(
            "Set-Cookie".to_string(),
            vec!["a=1".to_string(), "b=${q}".to_string()],
        );

        apply_copy_behaviors("", &mut headers, &behaviors, &request, FIXTURE_STUB).expect("copies");

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
        let served = apply_copy_behaviors(body, &mut headers, &[behavior], request, FIXTURE_STUB)
            .expect("copies");
        (served, headers.remove("X-C").expect("header kept"))
    }

    fn request_with_body(body: &str) -> RequestContext {
        RequestContext {
            method: "POST".to_string(),
            path: "/abc".to_string(),
            query: Default::default(),
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
            query: Default::default(),
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
        )
        .expect("copies");
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
        )
        .expect("copies");
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
        )
        .expect("copies");
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
            written_indices("$C", &body, &headers, None),
            BTreeSet::from([0, 7, 12])
        );
    }

    /// Issue #1583: Mountebank's `getFrom` gives `undefined` for a missing request field, and
    /// `RegExp.exec(undefined)` reads it as the text `undefined`, so a regex still matches it.
    #[test]
    fn a_missing_source_runs_a_regex_over_the_text_undefined() {
        let request = request_with_body("");
        for (from, selector, expected) in [
            (r#"{"query": "nosuch"}"#, ".*", "undefined"),
            (r#"{"headers": "X-None"}"#, ".*", "undefined"),
            (r#"{"query": "nosuch"}"#, r"\w+", "undefined"),
            (r#"{"query": "nosuch"}"#, "und", "und"),
            (r#""nosuch""#, ".*", "undefined"),
            (r#"{"query": "nosuch"}"#, r"\d+", "$C"),
        ] {
            let (body, header) = copy_once(from, regex(selector), &request, "[$C]");
            assert_eq!(body, format!("[{expected}]"), "{from} {selector}");
            assert_eq!(header, vec![format!("<{expected}>")], "{from} {selector}");
        }
    }

    /// Issue #1583: JSONPath and XPath select nothing from `undefined`, so the token stays.
    #[test]
    fn a_missing_source_leaves_the_token_for_jsonpath_and_xpath() {
        let request = request_with_body("");
        for using in [
            serde_json::json!({"method": "jsonpath", "selector": "$.a"}),
            serde_json::json!({"method": "xpath", "selector": "//a"}),
        ] {
            let (body, header) = copy_once(r#"{"query": "nosuch"}"#, using, &request, "[$C]");
            assert_eq!(body, "[$C]");
            assert_eq!(header, vec!["<$C>".to_string()]);
        }
    }

    /// Copies the path's first segment into `${v}` over `headers`, returning them.
    fn copy_path_into_headers(
        path: &str,
        headers: &[(&str, &str)],
    ) -> HashMap<String, Vec<String>> {
        let behavior: CopyBehavior = serde_json::from_value(serde_json::json!({
            "from": "path", "into": "${v}", "using": regex(r"[^/]+")
        }))
        .expect("the copy parses");
        let mut request = request_with_body("");
        request.path = path.to_string();
        let mut map: HashMap<String, Vec<String>> = headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), vec![(*v).to_string()]))
            .collect();
        apply_copy_behaviors("", &mut map, &[behavior], &request, FIXTURE_STUB).expect("copies");
        map
    }

    /// Issue #1583: Mountebank's `globalObjectReplace` replaces tokens in every key of the
    /// response, header names included. Rift left the name alone and the builder answered 500.
    #[test]
    fn a_token_in_a_header_name_is_replaced() {
        let headers = copy_path_into_headers("/p", &[("X-${v}", "1")]);
        assert_eq!(
            headers,
            HashMap::from([("X-p".to_string(), vec!["1".to_string()])])
        );
    }

    #[test]
    fn a_header_name_and_its_value_are_both_replaced() {
        let headers = copy_path_into_headers("/p", &[("X-${v}", "v=${v}")]);
        assert_eq!(
            headers,
            HashMap::from([("X-p".to_string(), vec!["v=p".to_string()])])
        );
    }

    /// The renamed header replaces one already of that name, case-insensitively, as Mountebank's
    /// `obj[newKey] = obj[key]` does before Node writes the headers.
    #[test]
    fn a_renamed_header_replaces_one_already_of_that_name() {
        let headers = copy_path_into_headers("/p", &[("X-${v}", "renamed"), ("x-P", "authored")]);
        assert_eq!(
            headers,
            HashMap::from([("X-p".to_string(), vec!["renamed".to_string()])])
        );
    }

    /// A header name whose token has no value keeps it, and still reaches the builder as written.
    #[test]
    fn a_header_name_whose_token_has_no_value_keeps_it() {
        let behavior: CopyBehavior = serde_json::from_value(serde_json::json!({
            "from": "path", "into": "${v}", "using": regex(r"\d+")
        }))
        .expect("the copy parses");
        let mut headers = HashMap::from([("X-${v}".to_string(), vec!["1".to_string()])]);
        apply_copy_behaviors(
            "",
            &mut headers,
            &[behavior],
            &request_with_body(""),
            FIXTURE_STUB,
        )
        .expect("copies");
        assert_eq!(
            headers,
            HashMap::from([("X-${v}".to_string(), vec!["1".to_string()])])
        );
    }

    /// An indexed token in a header name takes its value too.
    #[test]
    fn an_indexed_token_in_a_header_name_is_replaced() {
        let behavior: CopyBehavior = serde_json::from_value(serde_json::json!({
            "from": "path", "into": "$C", "using": regex(r"(\w+)-(\w+)")
        }))
        .expect("the copy parses");
        let mut request = request_with_body("");
        request.path = "/ab-cd".to_string();
        let mut headers = HashMap::from([("X-$C[2]".to_string(), vec!["1".to_string()])]);
        apply_copy_behaviors("", &mut headers, &[behavior], &request, FIXTURE_STUB)
            .expect("copies");
        assert_eq!(
            headers,
            HashMap::from([("X-cd".to_string(), vec!["1".to_string()])])
        );
    }
}
