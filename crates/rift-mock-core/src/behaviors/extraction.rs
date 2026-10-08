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
    /// XPath expression for XML, with Mountebank's optional `ns` prefix→URI map (issue #1326)
    #[serde(rename = "xpath")]
    XPath {
        selector: String,
        /// Read leniently: before #1326 the key was ignored, so a stored config may carry any
        /// shape here and must still decode (rift-cluster replays stored bytes). Admission refuses
        /// a malformed one.
        #[serde(
            rename = "ns",
            default,
            deserialize_with = "lenient_namespaces",
            skip_serializing_if = "Option::is_none"
        )]
        namespaces: Option<HashMap<String, String>>,
    },
}

/// An `ns` that is an object of strings, else `None`.
fn lenient_namespaces<'de, D>(deserializer: D) -> Result<Option<HashMap<String, String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
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
            ExtractionMethod::XPath {
                selector,
                namespaces,
            } => extract_xpath_with_ns(value, selector, namespaces.as_ref()),
        }
    }

    /// Every value the selector matches, in Mountebank's order: what a lookup `key.index` indexes
    /// (`lookupRow`: `keyValues[index]`, issue #1240).
    ///
    /// For a regex this is `RegExp.exec`'s array: the whole match at 0, then each capture group, a
    /// group that did not participate being `None`. For JSONPath and XPath it is each selected
    /// value, in document order. No match, or a body that does not parse, is an empty list.
    pub fn matches(&self, value: &str) -> Vec<Option<String>> {
        match self {
            ExtractionMethod::Regex { selector, options } => {
                let opts = options.as_ref();
                let Ok(re) = RegexBuilder::new(selector)
                    .case_insensitive(opts.is_some_and(|o| o.ignore_case))
                    .multi_line(opts.is_some_and(|o| o.multiline))
                    .build()
                else {
                    return Vec::new();
                };
                re.captures(value)
                    .map(|caps| {
                        caps.iter()
                            .map(|m| m.map(|m| m.as_str().to_string()))
                            .collect()
                    })
                    .unwrap_or_default()
            }
            ExtractionMethod::JsonPath { selector } => {
                let Ok(json) = serde_json::from_str::<serde_json::Value>(value) else {
                    return Vec::new();
                };
                let Some(json_path) = cached_jsonpath(selector) else {
                    return Vec::new();
                };
                json_path
                    .query(&json)
                    .all()
                    .into_iter()
                    .map(|node| Some(json_node_text(node)))
                    .collect()
            }
            ExtractionMethod::XPath {
                selector,
                namespaces,
            } => xpath_all(value, selector, namespaces.as_ref()),
        }
    }
}

/// A selected JSON node as the text a copy, lookup or predicate uses: a string unquoted, anything
/// else as its JSON (Mountebank's `forceStrings` for scalars).
pub(crate) fn json_node_text(node: &serde_json::Value) -> String {
    match node {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Every value an XPath selector yields against `xml_str`, in document order.
fn xpath_all(
    xml_str: &str,
    selector: &str,
    ns: Option<&HashMap<String, String>>,
) -> Vec<Option<String>> {
    #[cfg(test)]
    counters::bump_dom_parse();
    let Ok(package) = sxd_document::parser::parse(xml_str) else {
        return Vec::new();
    };
    eval_xpath_all_on(&package.as_document(), selector, ns)
        .map(|values| values.into_iter().map(Some).collect())
        .unwrap_or_default()
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
///
/// Mountebank's jsonpath-plus shorthands are then rewritten to the RFC 9535 form that selects the
/// same nodes (issue #1255); see [`rewrite_jsonpath_plus_shorthands`].
fn normalize_jsonpath(path: &str) -> Cow<'_, str> {
    let trimmed = path.trim();
    let prefix = if trimmed.starts_with('$') {
        ""
    } else if trimmed.starts_with('[') {
        "$"
    } else {
        "$."
    };
    let rooted = if prefix.is_empty() && trimmed.len() == path.len() {
        // Borrow only when nothing needs changing: already rooted and no stray padding.
        Cow::Borrowed(path)
    } else {
        Cow::Owned(format!("{prefix}{trimmed}"))
    };
    match rewrite_jsonpath_plus_shorthands(&rooted) {
        Some(rewritten) => Cow::Owned(rewritten),
        None => rooted,
    }
}

/// Rewrite the two jsonpath-plus spellings RFC 9535 refuses or reads differently, outside quoted
/// names, or `None` when the selector contains neither:
///
/// - `.[` is a descendant segment in jsonpath-plus (`toPathArray` turns the `.` and the `[` into
///   `;;`, then `;;` into `;..;`), so it becomes `..[`.
/// - A slice end or step that parses to `0` falls back to its default there (`parseInt(end) ||
///   len`, `parseInt(step) || 1`), so `[:0]` is the whole array, not an empty one. It becomes empty
///   (open end, default step). A zero end with a negative step is left alone.
///
/// Nothing inside an RFC 9535 filter (`[?...]`) is rewritten.
fn rewrite_jsonpath_plus_shorthands(selector: &str) -> Option<String> {
    // Runs on every match-time lookup (before the compiled-selector cache), so a selector that
    // cannot contain either shorthand must not allocate.
    if !selector.contains(".[") && !selector.contains(':') {
        return None;
    }
    let chars: Vec<char> = selector.chars().collect();
    let mut out = String::with_capacity(selector.len() + 2);
    let mut changed = false;
    let mut quote: Option<char> = None;
    // Inside an RFC 9535 filter (`[?...]`) nothing is rewritten: jsonpath-plus filters are
    // JavaScript (`?(...)`), so Mountebank gives these spellings no meaning to match there.
    let mut filter_depth = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            out.push(c);
            if c == '\\' {
                if let Some(&next) = chars.get(i + 1) {
                    out.push(next);
                    i += 1;
                }
            } else if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if filter_depth > 0 {
            match c {
                '\'' | '"' => quote = Some(c),
                '[' => filter_depth += 1,
                ']' => filter_depth -= 1,
                _ => {}
            }
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                out.push(c);
            }
            '[' if chars.get(i + 1) == Some(&'?') => {
                filter_depth = 1;
                out.push(c);
            }
            '.' if chars.get(i + 1) == Some(&'[') && !out.ends_with('.') => {
                out.push_str("..");
                changed = true;
            }
            '[' => {
                let close = chars[i + 1..]
                    .iter()
                    .position(|&ch| matches!(ch, ']' | '[' | '\'' | '"'))
                    .map(|offset| i + 1 + offset)
                    .filter(|&end| chars[end] == ']');
                let slice = close.and_then(|end| {
                    rewrite_zero_slice_bounds(&chars[i + 1..end].iter().collect::<String>())
                        .map(|rewritten| (end, rewritten))
                });
                match slice {
                    Some((end, rewritten)) => {
                        out.push('[');
                        out.push_str(&rewritten);
                        out.push(']');
                        changed = true;
                        i = end;
                    }
                    None => out.push(c),
                }
            }
            _ => out.push(c),
        }
        i += 1;
    }
    changed.then_some(out)
}

/// For a bracket body that is a slice (`start:end` or `start:end:step`, each part an optional
/// integer), the body with a zero end or step emptied, or `None` if it is not a slice or has
/// neither.
fn rewrite_zero_slice_bounds(body: &str) -> Option<String> {
    let parts: Vec<&str> = body.split(':').map(str::trim).collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let is_int = |part: &str| {
        let digits = part.strip_prefix('-').unwrap_or(part);
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    };
    if !parts.iter().all(|part| part.is_empty() || is_int(part)) {
        return None;
    }
    let is_zero =
        |part: &str| is_int(part) && part.trim_start_matches('-').bytes().all(|b| b == b'0');
    // A negative step walks down, where an end of `0` is a real bound (`[5:0:-1]` stops before
    // index 0) and jsonpath-plus, which only counts up, selects nothing to be compatible with.
    let negative_step = parts
        .get(2)
        .is_some_and(|step| step.starts_with('-') && !is_zero(step));
    let zero_end = is_zero(parts[1]) && !negative_step;
    let zero_step = parts.get(2).is_some_and(|step| is_zero(step));
    if !zero_end && !zero_step {
        return None;
    }
    let end = if zero_end { "" } else { parts[1] };
    Some(match parts.get(2) {
        Some(step) => format!("{}:{end}:{}", parts[0], if zero_step { "" } else { step }),
        None => format!("{}:{end}", parts[0]),
    })
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
/// parses the body once and delegates to [`extract_jsonpath_value`]. Predicates use
/// [`jsonpath_selection`] instead, which keeps every selected value (issue #1257).
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
    node_list.first().map(json_node_text)
}

/// What a predicate's selector selected, as Mountebank's predicates see it (issue #1257).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Selection {
    /// One value, compared as before. Nothing selected is the empty string, as Mountebank reads it.
    One(String),
    /// Several values (or the elements of a single selected array): the predicate holds when any
    /// of them satisfies it, and `deepEquals` compares the sorted list.
    Many(Vec<String>),
}

impl Selection {
    fn from_texts(mut texts: Vec<String>) -> Self {
        match texts.len() {
            0 => Self::One(String::new()),
            1 => Self::One(texts.swap_remove(0)),
            _ => Self::Many(texts),
        }
    }
}

/// What a predicate's JSONPath selector selects: the text of every selected node, except that a
/// single selected node which is a non-empty array stands for its elements (Mountebank cannot tell
/// `$.tags` from `$.tags[*]`). `None` when the selector does not compile.
pub(crate) fn jsonpath_selection(json: &serde_json::Value, path: &str) -> Option<Selection> {
    let json_path = cached_jsonpath(path)?;
    let nodes = json_path.query(json).all();
    Some(match nodes.as_slice() {
        [serde_json::Value::Array(items)] if !items.is_empty() => {
            Selection::Many(items.iter().map(json_node_text).collect())
        }
        _ => Selection::from_texts(nodes.into_iter().map(json_node_text).collect()),
    })
}

/// What a predicate's XPath selector selects, in document order. `None` when the selector does not
/// compile or does not evaluate.
pub(crate) fn xpath_selection(
    document: &sxd_document::dom::Document,
    selector: &str,
    ns: Option<&HashMap<String, String>>,
) -> Option<Selection> {
    eval_xpath_all_on(document, selector, ns).map(Selection::from_texts)
}

/// Extract value using XPath, optionally with namespace prefix bindings.
/// Used by copy behaviors and predicate xpath parameter.
pub fn extract_xpath(xml_str: &str, path: &str) -> Option<String> {
    extract_xpath_with_ns(xml_str, path, None)
}

/// Extract value using XPath with optional namespace prefix→URI map.
///
/// String-only entry point for callers that only have the raw body (copy behaviors). It parses the
/// body itself — bumping `DOM_PARSE` — then delegates to [`eval_xpath_on`]. Predicates use
/// [`xpath_selection`] against the request's shared DOM instead (issues #711, #1257).
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
    static XPATH_CACHE: RefCell<HashMap<(String, String), Rc<CompiledXPath>>> =
        RefCell::new(HashMap::new());
}

/// A compiled selector and every namespace prefix its text may use.
struct CompiledXPath {
    xpath: sxd_xpath::XPath,
    prefixes: Vec<String>,
}

/// The URI an unbound prefix is bound to. `sxd_xpath` panics when a name test's prefix has no
/// binding (`No namespace for prefix`), which dropped the connection mid-request; bound to a URI no
/// document declares, the step selects nothing, as for any other name absent from the document.
const UNBOUND_PREFIX_URI: &str = "urn:rift:unbound-xpath-prefix";

/// Every `prefix` in a `prefix:name` / `prefix:*` of `selector`. An over-approximation — a
/// `p:x` inside a string literal is listed too — which is harmless: binding a prefix the
/// expression never resolves changes nothing. An axis (`child::`) is not a prefix. A `-` may be
/// part of a name (`my-ns:x`) or the minus operator (`count(//a)-p:x`), so both readings are
/// listed.
fn xpath_prefixes(selector: &str) -> Vec<String> {
    static QNAME_PREFIX: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?:^|::|[^\w.:])([^\W\d][\w.-]*):(?:[^\W\d]|\*)")
            .expect("static regex compiles")
    });
    let mut prefixes: Vec<String> = Vec::new();
    for caps in QNAME_PREFIX.captures_iter(selector) {
        let name = &caps[1];
        prefixes.push(name.to_string());
        prefixes.extend(
            name.match_indices('-')
                .map(|(at, _)| &name[at + 1..])
                .filter(|rest| rest.starts_with(|c: char| c.is_alphabetic() || c == '_'))
                .map(str::to_string),
        );
    }
    prefixes.sort_unstable();
    prefixes.dedup();
    prefixes
}

/// An evaluation context binding `ns`, and every other prefix `xpath` uses to
/// [`UNBOUND_PREFIX_URI`].
fn xpath_context(
    xpath: &CompiledXPath,
    ns: Option<&HashMap<String, String>>,
) -> sxd_xpath::Context<'static> {
    let mut context = sxd_xpath::Context::new();
    for prefix in &xpath.prefixes {
        if !ns.is_some_and(|ns| ns.contains_key(prefix)) {
            context.set_namespace(prefix, UNBOUND_PREFIX_URI);
        }
    }
    if let Some(namespaces) = ns {
        for (prefix, uri) in namespaces {
            context.set_namespace(prefix, uri);
        }
    }
    context
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
fn cached_xpath(selector: &str, ns: Option<&HashMap<String, String>>) -> Option<Rc<CompiledXPath>> {
    let key = (selector.to_string(), ns_key(ns));
    XPATH_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(xpath) = cache.get(&key) {
            return Some(Rc::clone(xpath));
        }
        #[cfg(test)]
        counters::bump_xpath_compile();
        let xpath = Rc::new(CompiledXPath {
            xpath: sxd_xpath::Factory::new().build(selector).ok()??,
            prefixes: xpath_prefixes(selector),
        });
        if cache.len() >= MAX_CACHED_XPATHS {
            cache.clear();
        }
        cache.insert(key, Rc::clone(&xpath));
        Some(xpath)
    })
}

/// Check that `selector` compiles as the JSONPath the matcher will run — the same rooting
/// ([`normalize_jsonpath`]) and the same parser as [`cached_jsonpath`]. The config doors call this so
/// a selector that can never extract is refused at load (issue #1220) instead of reaching the
/// matcher, where the failed extraction would read as the empty string.
pub(crate) fn validate_jsonpath_selector(selector: &str) -> Result<(), String> {
    serde_json_path::JsonPath::parse(&normalize_jsonpath(selector))
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Check that `selector` compiles as the XPath the matcher will run (the same `Factory` as
/// [`cached_xpath`]). An empty selector compiles to no expression at all, which the matcher also
/// reads as "no extraction", so it is refused alongside a syntax error (issue #1220).
pub(crate) fn validate_xpath_selector(selector: &str) -> Result<(), String> {
    match sxd_xpath::Factory::new().build(selector) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("the selector is empty".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Evaluate an XPath selector against an already-parsed DOM `Document`, using the thread-local
/// compiled-selector cache: the first selected value, in document order.
pub(crate) fn eval_xpath_on(
    document: &sxd_document::dom::Document,
    selector: &str,
    ns: Option<&HashMap<String, String>>,
) -> Option<String> {
    use sxd_xpath::Value;

    let xpath = cached_xpath(selector, ns)?;
    let context = xpath_context(&xpath, ns);
    match xpath.xpath.evaluate(&context, document.root()) {
        Ok(Value::String(s)) => Some(s),
        Ok(Value::Number(n)) => Some(n.to_string()),
        Ok(Value::Boolean(b)) => Some(b.to_string()),
        // The node set is a hash set: take the first node in document order, not whichever node
        // iteration happens to yield first (issue #1257).
        Ok(Value::Nodeset(nodes)) => nodes.document_order_first().map(|n| n.string_value()),
        _ => None,
    }
}

/// Every value an XPath selector yields against `document`, in document order — what a predicate
/// matches against (issue #1257). `None` when the selector does not compile or does not evaluate.
fn eval_xpath_all_on(
    document: &sxd_document::dom::Document,
    selector: &str,
    ns: Option<&HashMap<String, String>>,
) -> Option<Vec<String>> {
    use sxd_xpath::Value;

    let xpath = cached_xpath(selector, ns)?;
    let context = xpath_context(&xpath, ns);
    match xpath.xpath.evaluate(&context, document.root()) {
        Ok(Value::String(s)) => Some(vec![s]),
        Ok(Value::Number(n)) => Some(vec![n.to_string()]),
        Ok(Value::Boolean(b)) => Some(vec![b.to_string()]),
        Ok(Value::Nodeset(nodes)) => Some(
            nodes
                .document_order()
                .iter()
                .map(|n| n.string_value())
                .collect(),
        ),
        Err(_) => None,
    }
}

/// [`xpath_selection`] against a raw body, for a caller without a pre-parsed DOM.
pub(crate) fn xpath_selection_in(
    xml_str: &str,
    selector: &str,
    ns: Option<&HashMap<String, String>>,
) -> Option<Selection> {
    #[cfg(test)]
    counters::bump_dom_parse();
    let package = sxd_document::parser::parse(xml_str).ok()?;
    xpath_selection(&package.as_document(), selector, ns)
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

    // Issue #1257: a copy/lookup takes the first selected node in document order. The DOM's node
    // set is a hash set, so "first" used to be whichever node hashed first.
    #[test]
    fn xpath_extraction_takes_the_first_node_in_document_order() {
        let xml = "<r><n>first</n><n>second</n><n>third</n><n>fourth</n><n>fifth</n></r>";
        for _ in 0..50 {
            assert_eq!(extract_xpath(xml, "//n"), Some("first".to_string()));
        }
    }

    // Issue #1255: Mountebank's jsonpath-plus reads `.[` as a descendant segment and a slice end
    // or step of `0` as open; the selector is rewritten to the RFC 9535 form that means the same.
    #[test]
    fn normalize_rewrites_jsonpath_plus_shorthands() {
        let cases = [
            ("$.a.b.[0].c", "$.a.b..[0].c"),
            ("$.a.b.[:0].c", "$.a.b..[:].c"),
            ("$.x.y.[*].z", "$.x.y..[*].z"),
            ("$.a[1:0]", "$.a[1:]"),
            ("$.a[::0]", "$.a[::]"),
            ("$.a[0:0:0]", "$.a[0::]"),
            ("$.a[ : -0 ]", "$.a[:]"),
            ("b.[0]", "$.b..[0]"),
            ("  $.a.[0]  ", "$.a..[0]"),
            ("$.[0]", "$..[0]"),
            ("$.a[3:0:2]", "$.a[3::2]"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_jsonpath(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn normalize_leaves_standard_and_quoted_selectors_alone() {
        for selector in [
            "$.a[:1]",
            "$.a[1:3:2]",
            "$.a[-2:]",
            "$.a..[0]",
            "$['a.[x']",
            "$[\"k.[:0]\"]",
            "$['it\\'s.[0]']",
            "$.a[?@.b == 'x.[0]']",
            "$.a[5:0:-1]",
            "$[?@.a[0:0]]",
            "$[?@.a.[0]].b",
            "$.a[:0",
            "$.store.book",
        ] {
            assert!(
                matches!(normalize_jsonpath(selector), Cow::Borrowed(_)),
                "{selector:?} must pass through untouched, got {:?}",
                normalize_jsonpath(selector)
            );
        }
    }

    fn selected(selector: &str, json: serde_json::Value) -> Vec<serde_json::Value> {
        cached_jsonpath(selector)
            .unwrap_or_else(|| panic!("{selector} compiles"))
            .query(&json)
            .all()
            .into_iter()
            .cloned()
            .collect()
    }

    // Expected values measured with jsonpath-plus 10.4.0, the library Mountebank evaluates with.
    #[test]
    fn dot_bracket_selects_what_mountebank_selects() {
        use serde_json::json;
        let nested = json!({"a": {"b": [{"c": 1}, {"c": 2}, {"d": [{"c": 3}]}]}});
        assert_eq!(
            selected("$.a.b.[0].c", nested.clone()),
            vec![json!(1), json!(3)]
        );
        assert_eq!(selected("$.a.b[0].c", nested), vec![json!(1)]);

        let object_of_arrays =
            json!({"a": {"b": {"p": [{"c": "p0"}, {"c": "p1"}], "q": [{"c": "q0"}]}}});
        assert_eq!(
            selected("$.a.b.[0].c", object_of_arrays.clone()),
            vec![json!("p0"), json!("q0")]
        );
        assert!(selected("$.a.b[0].c", object_of_arrays).is_empty());
    }

    #[test]
    fn zero_slice_end_selects_the_whole_array_as_mountebank_does() {
        use serde_json::json;
        let body = json!({"x": {"y": [{"z": "first"}, {"z": "second"}]}});
        assert_eq!(
            selected("$.x.y.[:0].z", body.clone()),
            vec![json!("first"), json!("second")]
        );
        assert_eq!(
            selected("$.x.y[1:0].z", body.clone()),
            vec![json!("second")]
        );
        assert_eq!(
            selected("$.x.y[::0].z", body.clone()),
            vec![json!("first"), json!("second")]
        );
        // A reverse slice keeps its RFC 9535 meaning: down from index 1, stopping before 0.
        assert_eq!(selected("$.x.y[1:0:-1].z", body), vec![json!("second")]);
        assert_eq!(
            extract_jsonpath(
                r#"{"x":{"y":[{"z":"first"},{"z":"second"}]}}"#,
                "$.x.y.[:0].z"
            ),
            Some("first".to_string())
        );
        assert!(validate_jsonpath_selector("$.x.y.[:0].z").is_ok());
        assert!(validate_jsonpath_selector("$.a.b.[0].c").is_ok());
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

    /// Issue #1326: the `ns` map Mountebank's copy/lookup xpath carries reaches the evaluator
    /// through the enum, for both the single-value and the all-values path.
    #[test]
    fn xpath_extraction_method_binds_its_ns_map() {
        let method: ExtractionMethod = serde_json::from_value(serde_json::json!({
            "method": "xpath",
            "selector": "//mb:name",
            "ns": {"mb": "http://example.com/mb"}
        }))
        .unwrap();
        let xml = r#"<mb:root xmlns:mb="http://example.com/mb"><mb:name>first</mb:name><mb:name>second</mb:name></mb:root>"#;
        assert_eq!(method.extract(xml), Some("first".to_string()));
        assert_eq!(
            method.matches(xml),
            vec![Some("first".to_string()), Some("second".to_string())]
        );
    }

    #[test]
    fn xpath_extraction_method_without_ns_selects_nothing_prefixed() {
        let method: ExtractionMethod = serde_json::from_value(serde_json::json!({
            "method": "xpath",
            "selector": "//mb:name"
        }))
        .unwrap();
        let xml = r#"<mb:root xmlns:mb="http://example.com/mb"><mb:name>first</mb:name></mb:root>"#;
        assert_eq!(method.extract(xml), None);
        assert!(method.matches(xml).is_empty());
    }

    /// A stored `ns` the engine never read must still decode (rift-cluster replays stored bytes):
    /// anything but an object of strings decodes as no map; admission refuses it instead.
    #[test]
    fn xpath_extraction_method_decodes_a_malformed_ns_as_absent() {
        for ns in [
            serde_json::json!("x"),
            serde_json::json!(null),
            serde_json::json!(["a"]),
            serde_json::json!({"mb": 1}),
        ] {
            let method: ExtractionMethod = serde_json::from_value(serde_json::json!({
                "method": "xpath",
                "selector": "//name",
                "ns": ns
            }))
            .unwrap_or_else(|e| panic!("ns {ns} must decode: {e}"));
            assert!(
                matches!(
                    method,
                    ExtractionMethod::XPath {
                        namespaces: None,
                        ..
                    }
                ),
                "ns {ns} decoded as {method:?}"
            );
        }
    }

    #[test]
    fn xpath_extraction_method_serializes_ns_only_when_present() {
        let bare: ExtractionMethod = serde_json::from_value(serde_json::json!({
            "method": "xpath", "selector": "//a"
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            serde_json::json!({"method": "xpath", "selector": "//a"})
        );
        let with_ns: ExtractionMethod = serde_json::from_value(serde_json::json!({
            "method": "xpath", "selector": "//a:b", "ns": {"a": "urn:a"}
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&with_ns).unwrap(),
            serde_json::json!({"method": "xpath", "selector": "//a:b", "ns": {"a": "urn:a"}})
        );
    }

    #[test]
    fn xpath_prefixes_lists_name_test_prefixes_but_not_axes() {
        assert_eq!(
            xpath_prefixes("//a:x/child::b:y[@c:z='1']/d:*"),
            vec!["a", "b", "c", "d"]
        );
        assert_eq!(xpath_prefixes("/root/child::item[1]"), Vec::<String>::new());
        assert_eq!(xpath_prefixes("count(//p:n) > 1"), vec!["p"]);
        assert_eq!(xpath_prefixes("count(//a)-p:n"), vec!["p"]);
        assert_eq!(xpath_prefixes("1 -p:x"), vec!["p"]);
        assert_eq!(xpath_prefixes("-p:x"), vec!["p"]);
        assert_eq!(xpath_prefixes("//my-ns:x"), vec!["my-ns", "ns"]);
    }

    /// An unbound prefix used to panic inside `sxd_xpath` (`No namespace for prefix`) and drop
    /// the request; it now selects nothing, on the copy and on the predicate path.
    #[test]
    fn an_unbound_prefix_selects_nothing_instead_of_panicking() {
        let xml = r#"<mb:root xmlns:mb="http://example.com/mb"><mb:name>first</mb:name></mb:root>"#;
        assert_eq!(extract_xpath(xml, "//mb:name"), None);
        assert!(xpath_all(xml, "//mb:name", None).is_empty());
        let mut ns = HashMap::new();
        ns.insert("other".to_string(), "urn:other".to_string());
        assert_eq!(extract_xpath_with_ns(xml, "//mb:name", Some(&ns)), None);
        assert_eq!(
            extract_xpath(xml, "count(//mb:name)-mb:n"),
            Some("NaN".to_string())
        );
        let package = sxd_document::parser::parse(xml).unwrap();
        let selection = xpath_selection(&package.as_document(), "//mb:name", None);
        assert!(
            matches!(&selection, Some(Selection::One(s)) if s.is_empty()),
            "nothing selected reads as the empty string: {selection:?}"
        );
    }
}
