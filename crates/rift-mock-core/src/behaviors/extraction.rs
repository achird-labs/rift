//! Extraction methods: regex, JSONPath, XPath.

use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, LazyLock};

/// Test-only counters proving the parse/compile-once guarantees of issue #711.
///
/// These are the verifier for two acceptance criteria that are, literally, "N parses become one"
/// and "zero per-request selector compilation": there is no behavioural proxy for "the DOM was
/// parsed exactly once", so the count is asserted directly. `#[cfg(test)]` so release builds carry
/// neither the counters nor the increments.
///
/// **Thread-local, not global atomics.** `cargo test` runs the suite in parallel, and many other
/// tests evaluate jsonpath/xpath predicates concurrently — a process-global counter would be bumped
/// by all of them, so a `reset(); drive one request; read count` assertion would race. The counted
/// work (DOM parse, selector compile) all runs synchronously on the thread driving the request, so a
/// thread-local counter captures exactly that request's activity and nothing else.
#[cfg(test)]
pub(crate) mod counters {
    use std::cell::Cell;

    thread_local! {
        /// Bumped once each time an XML body is parsed into a DOM `Package`.
        static DOM_PARSE: Cell<usize> = const { Cell::new(0) };
        /// Bumped on an XPath compile (a thread-local cache miss).
        static XPATH_COMPILE: Cell<usize> = const { Cell::new(0) };
        /// Bumped on a JSONPath selector compile (a global cache miss).
        static JSONPATH_COMPILE: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn bump_dom_parse() {
        DOM_PARSE.with(|c| c.set(c.get() + 1));
    }
    pub(crate) fn bump_xpath_compile() {
        XPATH_COMPILE.with(|c| c.set(c.get() + 1));
    }
    pub(crate) fn bump_jsonpath_compile() {
        JSONPATH_COMPILE.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn reset() {
        DOM_PARSE.with(|c| c.set(0));
        XPATH_COMPILE.with(|c| c.set(0));
        JSONPATH_COMPILE.with(|c| c.set(0));
    }
    pub(crate) fn dom_parses() -> usize {
        DOM_PARSE.with(Cell::get)
    }
    pub(crate) fn xpath_compiles() -> usize {
        XPATH_COMPILE.with(Cell::get)
    }
    pub(crate) fn jsonpath_compiles() -> usize {
        JSONPATH_COMPILE.with(Cell::get)
    }
}

/// Regex matching options (Mountebank-compatible)
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegexOptions {
    /// Case-insensitive matching
    #[serde(default)]
    pub ignore_case: bool,
    /// Multiline mode (`^`/`$` match line boundaries)
    #[serde(default)]
    pub multiline: bool,
}

/// Method for extracting values from source
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum ExtractionMethod {
    /// Regular expression with capture groups
    Regex {
        selector: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<RegexOptions>,
    },
    /// JSONPath expression
    #[serde(rename = "jsonpath")]
    JsonPath { selector: String },
    /// XPath expression for XML
    #[serde(rename = "xpath")]
    XPath { selector: String },
}

impl ExtractionMethod {
    /// Apply extraction to a value
    pub fn extract(&self, value: &str) -> Option<String> {
        match self {
            ExtractionMethod::Regex { selector, options } => {
                let opts = options.as_ref();
                let re = RegexBuilder::new(selector)
                    .case_insensitive(opts.is_some_and(|o| o.ignore_case))
                    .multi_line(opts.is_some_and(|o| o.multiline))
                    .build()
                    .ok()?;
                if let Some(caps) = re.captures(value) {
                    // Return first capture group if exists, otherwise full match
                    caps.get(1)
                        .or_else(|| caps.get(0))
                        .map(|m| m.as_str().to_string())
                } else {
                    None
                }
            }
            ExtractionMethod::JsonPath { selector } => extract_jsonpath(value, selector),
            ExtractionMethod::XPath { selector } => extract_xpath(value, selector),
        }
    }
}

/// Normalize a JSONPath selector to an RFC 9535 rooted path.
///
/// serde_json_path requires the leading root identifier `$`, but Mountebank (and
/// the recorded predicates we ingest) accept bare selectors such as `searchValue`
/// or `user.name`. Treat a bare selector as root-relative: prepend `$` when it
/// already begins with a segment (`[0]`, `['k']`) and `$.` otherwise.
///
/// Surrounding whitespace is trimmed uniformly so a rooted and a bare selector
/// are handled the same way regardless of stray padding.
fn normalize_jsonpath(path: &str) -> Cow<'_, str> {
    let trimmed = path.trim();
    let prefix = if trimmed.starts_with('$') {
        ""
    } else if trimmed.starts_with('[') {
        "$"
    } else {
        "$."
    };
    // Borrow only when nothing needs changing: already rooted and no stray padding.
    if prefix.is_empty() && trimmed.len() == path.len() {
        Cow::Borrowed(path)
    } else {
        Cow::Owned(format!("{prefix}{trimmed}"))
    }
}

/// Process-wide cache of compiled JSONPath selectors (issue #711).
///
/// Every predicate/copy-behavior evaluation that uses a `jsonpath` selector recompiled it from its
/// source string per call — for a stub set with N jsonpath-selectored stubs, one request paid N
/// compiles even when every stub shares the same selector string across requests. `JsonPath` (unlike
/// `sxd_xpath::XPath`, see the thread-local cache below) is `Send + Sync`, so a process-global cache
/// mirroring [`regex_cache`](super::super::imposter::predicates::regex_cache) is sufficient — no
/// thread-local needed.
///
/// Bounded for the same reason as the regex cache: it's a process-global static, so imposter churn
/// with ever-distinct selectors must not grow it forever.
const MAX_CACHED_JSONPATHS: usize = 1024;

static JSONPATH_CACHE: LazyLock<
    parking_lot::RwLock<HashMap<String, Arc<serde_json_path::JsonPath>>>,
> = LazyLock::new(|| parking_lot::RwLock::new(HashMap::new()));

/// Return the compiled selector for `selector`, compiling and caching it on first use. The cache key
/// is the *normalized* (rooted) selector string, so a bare and a `$`-rooted form of the same
/// selector share one cache entry. Returns `None` when `selector` fails to parse (callers treat this
/// as "no extraction"), preserving today's behavior on a bad selector.
fn cached_jsonpath(selector: &str) -> Option<Arc<serde_json_path::JsonPath>> {
    let rooted = normalize_jsonpath(selector);

    // Fast path: shared read lock, no allocation or compile on a hit.
    {
        let cache = JSONPATH_CACHE.read();
        if let Some(jp) = cache.get(rooted.as_ref()) {
            return Some(Arc::clone(jp));
        }
    }

    // Slow path (cache miss): compile once (outside the lock), then insert under the write lock.
    #[cfg(test)]
    counters::bump_jsonpath_compile();
    let compiled = Arc::new(serde_json_path::JsonPath::parse(&rooted).ok()?);
    let mut cache = JSONPATH_CACHE.write();
    // Another thread may have inserted this selector while we compiled — reuse its entry.
    if let Some(jp) = cache.get(rooted.as_ref()) {
        return Some(Arc::clone(jp));
    }
    if cache.len() >= MAX_CACHED_JSONPATHS {
        cache.clear();
    }
    cache.insert(rooted.into_owned(), Arc::clone(&compiled));
    Some(compiled)
}

/// Extract value using JSONPath (RFC 9535 compliant via serde_json_path)
/// Used by copy behaviors and predicate jsonpath parameter.
/// Supports the full JSONPath spec: wildcards, descendant segments, filters,
/// negative indices, selector sequences, bracket notation, etc.
/// Bare selectors (no leading `$`) are treated as root-relative for
/// Mountebank compatibility.
///
/// String-only entry point for callers that only have the raw body (copy/lookup behaviors); it
/// parses the body once and delegates to [`extract_jsonpath_value`], which is also what the matching
/// hot path calls directly with an already-parsed `Value` to avoid a second parse (issue #711).
pub fn extract_jsonpath(json_str: &str, path: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(json_str).ok()?;
    extract_jsonpath_value(&json, path)
}

/// Extract value using JSONPath against an already-parsed JSON value. Reuses the caller's parse
/// (the matching hot path parses the request body once per request, issue #290) and the process-wide
/// compiled-selector cache (issue #711) instead of recompiling `path` on every call.
pub fn extract_jsonpath_value(json: &serde_json::Value, path: &str) -> Option<String> {
    let json_path = cached_jsonpath(path)?;
    let node_list = json_path.query(json);

    // Return the first matched node as a string
    let first = node_list.first()?;
    match first {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => Some("null".to_string()),
        _ => Some(first.to_string()),
    }
}

/// Extract value using XPath, optionally with namespace prefix bindings.
/// Used by copy behaviors and predicate xpath parameter.
pub fn extract_xpath(xml_str: &str, path: &str) -> Option<String> {
    extract_xpath_with_ns(xml_str, path, None)
}

/// Extract value using XPath with optional namespace prefix→URI map.
///
/// String-only entry point for callers that only have the raw body (copy behaviors, and any
/// predicate caller that hasn't pre-parsed a DOM). It parses the body itself — bumping `DOM_PARSE` —
/// then delegates to [`eval_xpath_on`], which is also what the matching hot path calls directly
/// against a DOM shared across a whole request (issue #711).
pub fn extract_xpath_with_ns(
    xml_str: &str,
    path: &str,
    ns: Option<&HashMap<String, String>>,
) -> Option<String> {
    use sxd_document::parser;

    #[cfg(test)]
    counters::bump_dom_parse();
    let package = parser::parse(xml_str).ok()?;
    let document = package.as_document();
    eval_xpath_on(&document, path, ns)
}

// Thread-local cache of compiled XPath selectors, keyed by `(selector, namespace-map key)`.
//
// `sxd_xpath::XPath` is `!Send`/`!Sync` (its internal AST holds `Rc`s), so it cannot live in a
// process-global cache the way the JSONPath cache above does — every thread must compile and keep
// its own copy. That's still a large win: the request-handling threads in the pool are long-lived,
// so a per-thread cache amortizes the compile across every request that thread ever handles, not
// just within one request. `Rc` (not `Arc`) mirrors the type's own `!Send` bound.
thread_local! {
    static XPATH_CACHE: RefCell<HashMap<(String, String), Rc<sxd_xpath::XPath>>> =
        RefCell::new(HashMap::new());
}

/// Per-thread ceiling on distinct cached XPath selectors, mirroring [`MAX_CACHED_JSONPATHS`]/the
/// regex cache's bound — imposter churn with ever-distinct selectors must not grow this forever.
const MAX_CACHED_XPATHS: usize = 1024;

/// Deterministic key for a namespace prefix→URI map: sorted `(prefix, uri)` pairs joined, so two
/// equal maps (in any iteration order) always produce the same string, and an absent/empty map
/// always produces the same fixed empty key. Part of the cache key alongside the selector string —
/// the same selector text resolves differently under different namespace bindings.
fn ns_key(ns: Option<&HashMap<String, String>>) -> String {
    let Some(ns) = ns else {
        return String::new();
    };
    let mut pairs: Vec<(&str, &str)> = ns.iter().map(|(p, u)| (p.as_str(), u.as_str())).collect();
    pairs.sort_unstable();
    pairs
        .into_iter()
        .map(|(p, u)| format!("{p}={u}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Return the compiled XPath for `(selector, ns)`, compiling and caching it in this thread's cache
/// on first use. Returns `None` when `selector` fails to compile (callers treat this as "no
/// extraction"), preserving today's behavior on a bad selector.
///
/// `ns` is part of the key defensively, not because compilation depends on it: `sxd_xpath` compiles
/// a namespace-independent `XPath` and the real prefix→URI bindings are applied at *evaluation* time
/// in [`eval_xpath_on`]. So the key's `ns_key` component can at worst duplicate an entry (or, on the
/// theoretical `ns_key` collision where a URI contains the `,`/`=` delimiters, share one) — either
/// way the compiled selector returned is correct, because it carries no namespace state.
fn cached_xpath(
    selector: &str,
    ns: Option<&HashMap<String, String>>,
) -> Option<Rc<sxd_xpath::XPath>> {
    let key = (selector.to_string(), ns_key(ns));
    XPATH_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(xpath) = cache.get(&key) {
            return Some(Rc::clone(xpath));
        }
        #[cfg(test)]
        counters::bump_xpath_compile();
        let xpath = Rc::new(sxd_xpath::Factory::new().build(selector).ok()??);
        if cache.len() >= MAX_CACHED_XPATHS {
            cache.clear();
        }
        cache.insert(key, Rc::clone(&xpath));
        Some(xpath)
    })
}

/// Evaluate an XPath selector against an already-parsed DOM `Document`, using the thread-local
/// compiled-selector cache. The matching hot path calls this directly with a `Document` shared
/// across every predicate/stub in one request (issue #711) instead of parsing per predicate.
pub(crate) fn eval_xpath_on(
    document: &sxd_document::dom::Document,
    selector: &str,
    ns: Option<&HashMap<String, String>>,
) -> Option<String> {
    use sxd_xpath::{Context, Value};

    let xpath = cached_xpath(selector, ns)?;

    let mut context = Context::new();
    if let Some(namespaces) = ns {
        for (prefix, uri) in namespaces {
            context.set_namespace(prefix, uri);
        }
    }

    let root = document.root();
    match xpath.evaluate(&context, root) {
        Ok(Value::String(s)) => Some(s),
        Ok(Value::Number(n)) => Some(n.to_string()),
        Ok(Value::Boolean(b)) => Some(b.to_string()),
        Ok(Value::Nodeset(nodes)) => nodes.iter().next().map(|n| n.string_value()),
        _ => None,
    }
}

/// Parse-once-per-request primitive for the XML DOM (issue #711).
///
/// `sxd_document::Package` is `!Send`, and every borrow off it (`Document<'d>`) is lifetime-bound to
/// it — it cannot be stored in `Arc<StubState>` or held across an `.await` point, so this type is
/// deliberately scoped to live only for the synchronous duration of one matching pass: constructed
/// before the stub loop, dropped when matching returns. Within that scope it memoizes the parse (and
/// the parse failure) so N XPath predicates across N stubs in one request share a single
/// `sxd_document::parser::parse` call instead of one each.
pub(crate) struct LazyXmlDom<'a> {
    body: &'a str,
    parsed: OnceCell<Option<sxd_document::Package>>,
}

impl<'a> LazyXmlDom<'a> {
    pub(crate) fn new(body: &'a str) -> Self {
        Self {
            body,
            parsed: OnceCell::new(),
        }
    }

    /// The parsed DOM, parsing (and bumping `DOM_PARSE`) on the first call only. `None` if the body
    /// isn't well-formed XML — cached too, so a malformed body doesn't retry the parse per predicate.
    pub(crate) fn document(&self) -> Option<sxd_document::dom::Document<'_>> {
        self.parsed
            .get_or_init(|| {
                #[cfg(test)]
                counters::bump_dom_parse();
                sxd_document::parser::parse(self.body).ok()
            })
            .as_ref()
            .map(sxd_document::Package::as_document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extraction_regex() {
        let method = ExtractionMethod::Regex {
            selector: r"/users/(\d+)".to_string(),
            options: None,
        };
        assert_eq!(method.extract("/users/123"), Some("123".to_string()));
        assert_eq!(method.extract("/posts/456"), None);
    }

    #[test]
    fn test_extraction_regex_full_match() {
        let method = ExtractionMethod::Regex {
            selector: r".*".to_string(),
            options: None,
        };
        assert_eq!(
            method.extract("hello world"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn test_extraction_jsonpath() {
        let method = ExtractionMethod::JsonPath {
            selector: "$.user.name".to_string(),
        };
        let json = r#"{"user": {"name": "Alice", "age": 30}}"#;
        assert_eq!(method.extract(json), Some("Alice".to_string()));
    }

    #[test]
    fn test_extraction_jsonpath_array() {
        let method = ExtractionMethod::JsonPath {
            selector: "$.items[0]".to_string(),
        };
        let json = r#"{"items": ["first", "second"]}"#;
        assert_eq!(method.extract(json), Some("first".to_string()));
    }

    // =========================================================================
    // Issue #78: JSONPath RFC 9535 compliance tests
    // =========================================================================

    // Test data matching the RFC 9535 examples section
    const STORE_JSON: &str = r#"{
        "store": {
            "book": [
                {
                    "category": "reference",
                    "author": "Nigel Rees",
                    "title": "Sayings of the Century",
                    "price": 8.95
                },
                {
                    "category": "fiction",
                    "author": "Evelyn Waugh",
                    "title": "Sword of Honour",
                    "price": 12.99
                },
                {
                    "category": "fiction",
                    "author": "Herman Melville",
                    "title": "Moby Dick",
                    "isbn": "0-553-21311-3",
                    "price": 8.99
                },
                {
                    "category": "fiction",
                    "author": "J. R. R. Tolkien",
                    "title": "The Lord of the Rings",
                    "isbn": "0-395-19395-8",
                    "price": 22.99
                }
            ],
            "bicycle": {
                "color": "red",
                "price": 399.99
            }
        }
    }"#;

    #[test]
    fn test_jsonpath_wildcard_selector() {
        // $.store.book[*].author → all authors
        let result = extract_jsonpath(STORE_JSON, "$.store.book[*].author");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Nigel Rees");
    }

    #[test]
    fn test_jsonpath_descendant_author() {
        // $..author → all authors (descendant segment)
        let result = extract_jsonpath(STORE_JSON, "$..author");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Nigel Rees");
    }

    #[test]
    fn test_jsonpath_descendant_price() {
        // $.store..price → prices of everything in the store
        let result = extract_jsonpath(STORE_JSON, "$.store..price");
        assert!(result.is_some());
        // serde_json uses BTreeMap (alphabetical key ordering), so "bicycle" comes before "book"
        assert_eq!(result.unwrap(), "399.99");
    }

    #[test]
    fn test_jsonpath_array_index() {
        // $..book[2] → the third book
        let result = extract_jsonpath(STORE_JSON, "$..book[2].title");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Moby Dick");
    }

    #[test]
    fn test_jsonpath_array_index_author() {
        // $..book[2].author → the third book's author
        let result = extract_jsonpath(STORE_JSON, "$..book[2].author");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Herman Melville");
    }

    #[test]
    fn test_jsonpath_missing_field() {
        // $..book[2].publisher → empty (third book has no publisher)
        let result = extract_jsonpath(STORE_JSON, "$..book[2].publisher");
        assert!(result.is_none());
    }

    #[test]
    fn test_jsonpath_negative_index() {
        // $..book[-1] → the last book
        let result = extract_jsonpath(STORE_JSON, "$..book[-1].title");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "The Lord of the Rings");
    }

    #[test]
    fn test_jsonpath_slice_first_two() {
        // $..book[:2] → the first two books (slice notation)
        let result = extract_jsonpath(STORE_JSON, "$..book[:2]");
        assert!(result.is_some());
    }

    #[test]
    fn test_jsonpath_filter_isbn() {
        // $..book[?@.isbn] → all books with an ISBN
        let result = extract_jsonpath(STORE_JSON, "$..book[?@.isbn].title");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Moby Dick");
    }

    #[test]
    fn test_jsonpath_filter_price() {
        // $..book[?@.price<10] → all books cheaper than 10
        let result = extract_jsonpath(STORE_JSON, "$..book[?@.price<10].title");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Sayings of the Century");
    }

    #[test]
    fn test_jsonpath_bracket_notation() {
        // $['store']['bicycle']['color'] → bracket notation for string index
        let result = extract_jsonpath(STORE_JSON, "$['store']['bicycle']['color']");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "red");
    }

    #[test]
    fn test_jsonpath_store_wildcard() {
        // $.store.* → all things in the store
        let result = extract_jsonpath(STORE_JSON, "$.store.*");
        assert!(result.is_some());
    }

    #[test]
    fn test_jsonpath_basic_still_works() {
        // Ensure basic paths like $.field and $.nested.field still work
        let json = r#"{"user": {"name": "Alice", "age": 30}}"#;
        assert_eq!(
            extract_jsonpath(json, "$.user.name"),
            Some("Alice".to_string())
        );
        assert_eq!(extract_jsonpath(json, "$.user.age"), Some("30".to_string()));

        let json = r#"{"items": ["first", "second"]}"#;
        assert_eq!(
            extract_jsonpath(json, "$.items[0]"),
            Some("first".to_string())
        );
        assert_eq!(
            extract_jsonpath(json, "$.items[1]"),
            Some("second".to_string())
        );
    }

    // Issue #306: bare selectors (no leading `$`) are treated as root-relative,
    // matching Mountebank behaviour.
    #[test]
    fn test_jsonpath_bare_selector_root_relative() {
        let json = r#"{"searchValue": "v"}"#;
        assert_eq!(extract_jsonpath(json, "searchValue"), Some("v".to_string()));
        // Equivalent to the rooted form.
        assert_eq!(
            extract_jsonpath(json, "searchValue"),
            extract_jsonpath(json, "$.searchValue")
        );
    }

    #[test]
    fn test_jsonpath_bare_nested_selector() {
        let json = r#"{"user": {"name": "Alice", "age": 30}}"#;
        assert_eq!(
            extract_jsonpath(json, "user.name"),
            Some("Alice".to_string())
        );
        assert_eq!(extract_jsonpath(json, "user.age"), Some("30".to_string()));
        // Equivalent to the rooted form.
        assert_eq!(
            extract_jsonpath(json, "user.name"),
            extract_jsonpath(json, "$.user.name")
        );
    }

    #[test]
    fn test_jsonpath_selector_whitespace_trimmed() {
        // Stray padding is trimmed for both bare and rooted selectors.
        let json = r#"{"searchValue": "v"}"#;
        assert_eq!(
            extract_jsonpath(json, "  searchValue  "),
            Some("v".to_string())
        );
        assert_eq!(
            extract_jsonpath(json, "  $.searchValue  "),
            Some("v".to_string())
        );
    }

    #[test]
    fn test_jsonpath_bare_bracket_selector() {
        // Leading-bracket bare selectors must get `$` (not `$.`) prepended.
        let json = r#"{"items": ["first", "second"]}"#;
        assert_eq!(
            extract_jsonpath(json, "items[0]"),
            Some("first".to_string())
        );
        let json = r#"{"searchValue": "v"}"#;
        assert_eq!(
            extract_jsonpath(json, "['searchValue']"),
            Some("v".to_string())
        );
    }

    #[test]
    fn test_jsonpath_rooted_selector_unchanged() {
        // The rooted form must keep working exactly as before.
        let json = r#"{"searchValue": "v"}"#;
        assert_eq!(
            extract_jsonpath(json, "$.searchValue"),
            Some("v".to_string())
        );
    }

    #[test]
    fn test_extraction_regex_ignore_case() {
        let method = ExtractionMethod::Regex {
            selector: "hello".to_string(),
            options: Some(RegexOptions {
                ignore_case: true,
                multiline: false,
            }),
        };
        assert_eq!(method.extract("HELLO world"), Some("HELLO".to_string()));
        assert_eq!(method.extract("nope"), None);
    }

    #[test]
    fn test_extraction_regex_multiline() {
        let method = ExtractionMethod::Regex {
            selector: r"^line2".to_string(),
            options: Some(RegexOptions {
                ignore_case: false,
                multiline: true,
            }),
        };
        assert_eq!(
            method.extract("line1\nline2\nline3"),
            Some("line2".to_string())
        );
    }

    #[test]
    fn test_extraction_regex_options_serde() {
        let json = r#"{"method": "regex", "selector": ".*", "options": {"ignoreCase": true, "multiline": false}}"#;
        let method: ExtractionMethod = serde_json::from_str(json).unwrap();
        match method {
            ExtractionMethod::Regex {
                options: Some(opts),
                ..
            } => {
                assert!(opts.ignore_case);
                assert!(!opts.multiline);
            }
            _ => panic!("Expected Regex with options"),
        }
    }

    #[test]
    fn test_extract_xpath_without_namespaces() {
        let xml = r#"<root><child>value</child></root>"#;
        assert_eq!(extract_xpath(xml, "//child"), Some("value".to_string()));
    }

    #[test]
    fn test_extract_xpath_with_ns_map() {
        let xml = r#"<ns:root xmlns:ns="http://example.com/ns"><ns:item>hello</ns:item></ns:root>"#;
        let mut ns = std::collections::HashMap::new();
        ns.insert("ns".to_string(), "http://example.com/ns".to_string());
        let result = extract_xpath_with_ns(xml, "//ns:item", Some(&ns));
        assert_eq!(result, Some("hello".to_string()));
    }

    #[test]
    fn test_extract_xpath_with_multiple_ns_bindings() {
        let xml = r#"<a:root xmlns:a="http://a.com" xmlns:b="http://b.com"><a:x><b:y>found</b:y></a:x></a:root>"#;
        let mut ns = std::collections::HashMap::new();
        ns.insert("a".to_string(), "http://a.com".to_string());
        ns.insert("b".to_string(), "http://b.com".to_string());
        let result = extract_xpath_with_ns(xml, "//a:x/b:y", Some(&ns));
        assert_eq!(result, Some("found".to_string()));
    }
}
