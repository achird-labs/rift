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
        /// Bumped once each time a request body is parsed as JSON by [`super::LazyJson`].
        static JSON_PARSE: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn bump_json_parse() {
        JSON_PARSE.with(|c| c.set(c.get() + 1));
    }
    pub(crate) fn json_parses() -> usize {
        JSON_PARSE.with(Cell::get)
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
        JSON_PARSE.with(|c| c.set(0));
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    let Some(package) = parse_xml_like_mountebank(xml_str) else {
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
    let package = parse_xml_like_mountebank(xml_str)?;
    let document = package.as_document();
    eval_xpath_on(&document, path, ns)
}

// Thread-local cache of compiled XPath selectors, keyed by selector text.
//
// `sxd_xpath::XPath` is `!Send`/`!Sync` (its internal AST holds `Rc`s), so it cannot live in a
// process-global cache the way the JSONPath cache above does — every thread must compile and keep
// its own copy. That's still a large win: the request-handling threads in the pool are long-lived,
// so a per-thread cache amortizes the compile across every request that thread ever handles, not
// just within one request. `Rc` (not `Arc`) mirrors the type's own `!Send` bound.
thread_local! {
    static XPATH_CACHE: RefCell<HashMap<String, Rc<CompiledXPath>>> =
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

/// Return the compiled XPath for `selector`, compiling and caching it in this thread's cache on
/// first use. Returns `None` when `selector` fails to compile (callers treat this as "no
/// extraction"), preserving today's behavior on a bad selector.
///
/// The key is the selector alone: `sxd_xpath` compiles a namespace-independent `XPath`, and the
/// prefix→URI bindings are applied at *evaluation* time by [`xpath_context`] (issue #1384).
fn cached_xpath(selector: &str) -> Option<Rc<CompiledXPath>> {
    XPATH_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(xpath) = cache.get(selector) {
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
        cache.insert(selector.to_string(), Rc::clone(&xpath));
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

    let xpath = cached_xpath(selector)?;
    let context = xpath_context(&xpath, ns);
    match xpath.xpath.evaluate(&context, document.root()) {
        Ok(Value::String(s)) => Some(s),
        Ok(Value::Number(n)) => Some(n.to_string()),
        Ok(Value::Boolean(b)) => Some(b.to_string()),
        // The node set is a hash set: take the first node in document order, not whichever node
        // iteration happens to yield first (issue #1257).
        Ok(Value::Nodeset(nodes)) => nodes.document_order_first().map(mb_node_value),
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

    let xpath = cached_xpath(selector)?;
    let context = xpath_context(&xpath, ns);
    match xpath.xpath.evaluate(&context, document.root()) {
        Ok(Value::String(s)) => Some(vec![s]),
        Ok(Value::Number(n)) => Some(vec![n.to_string()]),
        Ok(Value::Boolean(b)) => Some(vec![b.to_string()]),
        Ok(Value::Nodeset(nodes)) => Some(
            nodes
                .document_order()
                .into_iter()
                .map(mb_node_value)
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
    let package = parse_xml_like_mountebank(xml_str)?;
    xpath_selection(&package.as_document(), selector, ns)
}

/// The DOM an XPath selector sees, parsed the way Mountebank's xmldom 0.9 reads the body (issues
/// #1455, #1479). Every XPath parse goes through here.
///
/// xmldom reports several things as non-fatal that `sxd_document` refuses outright, so they are
/// prepared away first (see [`prepare_xml`]), and text after the root element is ignored (see
/// [`without_trailing_text`]). What xmldom treats as fatal — an unclosed or mismatched tag, an
/// undeclared prefix, a second element after the root, a second DOCTYPE — still yields `None`,
/// as it does in Mountebank 2.9.2+.
pub(crate) fn parse_xml_like_mountebank(body: &str) -> Option<sxd_document::Package> {
    #[cfg(test)]
    counters::bump_dom_parse();
    parse_prepared(&prepare_xml(body)?)
}

/// [`parse_xml_like_mountebank`] on the name-folded view a case-insensitive XPath predicate selects
/// from (issue #1478): see [`fold_xml_names`].
pub(crate) fn parse_xml_folding_names(body: &str) -> Option<sxd_document::Package> {
    #[cfg(test)]
    counters::bump_dom_parse();
    parse_prepared(&fold_xml_names(&prepare_xml(body)?))
}

/// Parse a [`prepare_xml`]ed text, reading past what follows the root as xmldom does, into
/// xmldom's node model (see [`normalize_like_xmldom`]).
fn parse_prepared(xml: &str) -> Option<sxd_document::Package> {
    match sxd_document::parser::parse(xml) {
        Ok(package) => {
            normalize_like_xmldom(&package, xml);
            Some(package)
        }
        Err(error) => {
            let trimmed = without_trailing_text(xml, error.location())?;
            let package = sxd_document::parser::parse(&trimmed).ok()?;
            normalize_like_xmldom(&package, &trimmed);
            Some(package)
        }
    }
}

/// What Mountebank's `xpath.js` `nodeValue` reads from a selected node (issue #1495): a text node
/// or attribute its text, a comment its text, a PI its data, and any other node its first child's
/// data — so an element whose first child is an element, or that has none, reads as the string
/// `"undefined"`. Mountebank does this on purpose, so `exists: true` holds for an empty element.
fn mb_node_value(node: sxd_xpath::nodeset::Node<'_>) -> String {
    use sxd_document::dom::{ChildOfElement, ChildOfRoot};
    use sxd_xpath::nodeset::Node;
    const UNDEFINED: &str = "undefined";
    let pi = |value: Option<&str>| value.unwrap_or("").to_string();
    match node {
        Node::Text(text) => text.text().to_string(),
        Node::Attribute(attribute) => attribute.value().to_string(),
        Node::Comment(comment) => comment.text().to_string(),
        Node::ProcessingInstruction(instruction) => pi(instruction.value()),
        Node::Element(element) => match element.children().first() {
            Some(ChildOfElement::Text(text)) => text.text().to_string(),
            Some(ChildOfElement::Comment(comment)) => comment.text().to_string(),
            Some(ChildOfElement::ProcessingInstruction(instruction)) => pi(instruction.value()),
            Some(ChildOfElement::Element(_)) | None => UNDEFINED.to_string(),
        },
        // The XML declaration is a PI here, as in xmldom (see `normalize_like_xmldom`). Whitespace
        // before the root element is not a node in this DOM, so `/` reads "undefined" where
        // Mountebank reads that whitespace.
        Node::Root(root) => match root.children().first() {
            Some(ChildOfRoot::Comment(comment)) => comment.text().to_string(),
            Some(ChildOfRoot::ProcessingInstruction(instruction)) => pi(instruction.value()),
            Some(ChildOfRoot::Element(_)) | None => UNDEFINED.to_string(),
        },
        Node::Namespace(_) => UNDEFINED.to_string(),
    }
}

/// Give a DOM `sxd_document` parsed from `xml` the node model of Mountebank's xmldom (issue #1495),
/// so every XPath consumer (`text()`, `count()`, positions, [`mb_node_value`]) sees xmldom's nodes:
///
/// - `sxd_document` makes one text node per reference and per run of character data between
///   references; xmldom makes one for the whole run, so they are merged;
/// - an empty CDATA section is an empty text node here and no node in xmldom, so it is dropped;
///   any other CDATA section stays a node of its own, as in xmldom;
/// - the XML declaration is dropped here and kept as a PI by xmldom, so it is added back.
///
/// The children of every element are matched against the pieces of `xml` that made them. A
/// mismatch cannot happen for a text `sxd_document` accepted; if one did, the DOM is left as
/// parsed rather than half-rewritten.
fn normalize_like_xmldom(package: &sxd_document::Package, xml: &str) {
    use sxd_document::dom::{ChildOfElement, ChildOfRoot, Element};

    struct Frame<'d> {
        element: Element<'d>,
        old: Vec<ChildOfElement<'d>>,
        next: usize,
        new: Vec<ChildOfElement<'d>>,
        changed: bool,
    }

    impl<'d> Frame<'d> {
        fn open(element: Element<'d>) -> Self {
            Frame {
                element,
                old: element.children(),
                next: 0,
                new: Vec::new(),
                changed: false,
            }
        }

        fn take(&mut self) -> Option<ChildOfElement<'d>> {
            let child = self.old.get(self.next).copied();
            self.next += 1;
            child
        }
    }

    let document = package.as_document();
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut plan: Vec<(Element<'_>, Vec<ChildOfElement<'_>>)> = Vec::new();
    let mut declaration: Option<String> = None;
    let mut seen_root = false;
    let mut pos = 0;
    while pos < xml.len() {
        let (kind, end, _) = xml_piece(xml, pos);
        let piece = &xml[pos..end];
        pos = end;
        let Some(frame) = stack.last_mut() else {
            match kind {
                XmlPiece::StartTag | XmlPiece::EmptyTag if !seen_root => {
                    seen_root = true;
                    let Some(element) =
                        document
                            .root()
                            .children()
                            .into_iter()
                            .find_map(|c| match c {
                                ChildOfRoot::Element(e) => Some(e),
                                _ => None,
                            })
                    else {
                        return;
                    };
                    if kind == XmlPiece::StartTag {
                        stack.push(Frame::open(element));
                    }
                }
                XmlPiece::CommentOrPi if !seen_root => {
                    if let Some(data) = xml_declaration_data(piece) {
                        declaration = Some(data.to_string());
                    }
                }
                _ => {}
            }
            continue;
        };
        match kind {
            XmlPiece::StartTag | XmlPiece::EmptyTag => {
                let Some(ChildOfElement::Element(element)) = frame.take() else {
                    return;
                };
                frame.new.push(ChildOfElement::Element(element));
                if kind == XmlPiece::StartTag {
                    stack.push(Frame::open(element));
                }
            }
            XmlPiece::EndTag => {
                let Some(frame) = stack.pop() else { return };
                if frame.next != frame.old.len() {
                    return;
                }
                if frame.changed {
                    plan.push((frame.element, frame.new));
                }
            }
            XmlPiece::CommentOrPi => {
                let Some(
                    child @ (ChildOfElement::Comment(_) | ChildOfElement::ProcessingInstruction(_)),
                ) = frame.take()
                else {
                    return;
                };
                frame.new.push(child);
            }
            XmlPiece::CData => {
                let Some(child @ ChildOfElement::Text(_)) = frame.take() else {
                    return;
                };
                if piece == "<![CDATA[]]>" {
                    frame.changed = true;
                } else {
                    frame.new.push(child);
                }
            }
            XmlPiece::Text => {
                let mut texts = Vec::new();
                for _ in 0..text_tokens(piece) {
                    let Some(ChildOfElement::Text(text)) = frame.take() else {
                        return;
                    };
                    texts.push(text);
                }
                match texts.as_slice() {
                    [] => {}
                    [one] => frame.new.push(ChildOfElement::Text(*one)),
                    many => {
                        let merged: String = many.iter().map(|t| t.text()).collect();
                        frame
                            .new
                            .push(ChildOfElement::Text(document.create_text(&merged)));
                        frame.changed = true;
                    }
                }
            }
            XmlPiece::Doctype => return,
        }
    }
    if !stack.is_empty() {
        return;
    }
    for (element, children) in plan {
        element.replace_children(children);
    }
    if let Some(data) = declaration {
        let root = document.root();
        let mut children: Vec<ChildOfRoot<'_>> = vec![ChildOfRoot::ProcessingInstruction(
            document.create_processing_instruction("xml", Some(&data)),
        )];
        children.extend(root.children());
        root.replace_children(children);
    }
}

/// The data of an XML declaration (`<?xml version="1.0"?>` reads `version="1.0"`), or `None` for
/// any other comment or PI.
fn xml_declaration_data(piece: &str) -> Option<&str> {
    let rest = piece.strip_prefix("<?xml")?.strip_suffix("?>")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim_start())
}

/// How many text nodes `sxd_document` makes for a run of character data: one per reference and one
/// per non-empty run between references. [`prepare_xml`] has made every `&` start a reference.
fn text_tokens(text: &str) -> usize {
    let mut tokens = 0;
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        tokens += usize::from(at > 0);
        let Some(close) = rest[at..].find(';') else {
            return tokens + 1;
        };
        tokens += 1;
        rest = &rest[at + close + 1..];
    }
    tokens + usize::from(!rest.is_empty())
}

/// `xml` ASCII-lowercased as Mountebank lowercases the body of a case-insensitive XPath predicate
/// (issue #1478) — names, prefixes, attribute values, text, CDATA contents, comments and PI data —
/// except for what keeps the document readable: the `<![CDATA[` and `]]>` markers, a PI's target
/// (`<?xml`) and the whole DOCTYPE. Mountebank lowercases those too, so xmldom refuses a body with a
/// CDATA section or a DOCTYPE; Rift still reads it (a documented difference).
fn fold_xml_names(xml: &str) -> Cow<'_, str> {
    let mut out = Rewrite::new(xml);
    let mut pos = 0;
    while pos < xml.len() {
        let (kind, end, _) = xml_piece(xml, pos);
        let folded_from = match kind {
            XmlPiece::Doctype => None,
            XmlPiece::CData => Some(pos + "<![CDATA[".len()),
            XmlPiece::CommentOrPi if xml[pos..].starts_with("<?") => Some(
                xml[pos + 2..end]
                    .find(|c: char| c.is_whitespace() || c == '?')
                    .map_or(end, |at| pos + 2 + at),
            ),
            _ => Some(pos),
        };
        if let Some(from) = folded_from.filter(|&from| {
            from < end && xml.as_bytes()[from..end].iter().any(u8::is_ascii_uppercase)
        }) {
            // The markers kept as written all lie before `from`; a closing `]]>` or `?>` has no
            // letter to fold.
            out.replace(from, end, &xml[from..end].to_ascii_lowercase());
        }
        pos = end;
    }
    out.finish()
}

/// A case-insensitive XPath selector as Mountebank folds it: wholly lowercased, string literals
/// included, to compare against the folded document of [`fold_xml_names`].
pub(crate) fn fold_xpath_selector(selector: &str) -> Cow<'_, str> {
    if selector.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(selector.to_ascii_lowercase())
    } else {
        Cow::Borrowed(selector)
    }
}

/// One lexical piece of an XML text, as far as [`parse_xml_like_mountebank`] needs to tell them
/// apart: character data, a tag, or a construct whose content is not parsed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XmlPiece {
    Text,
    StartTag,
    EndTag,
    EmptyTag,
    /// A comment or processing instruction.
    CommentOrPi,
    CData,
    Doctype,
}

/// The piece of `xml` starting at byte `start`: its kind, where it ends, and whether it was
/// terminated (an unterminated construct runs to the end of `xml`).
fn xml_piece(xml: &str, start: usize) -> (XmlPiece, usize, bool) {
    let rest = &xml[start..];
    let bytes = rest.as_bytes();
    let through = |close: &str, open_len: usize, kind| match rest[open_len..].find(close) {
        Some(at) => (kind, start + open_len + at + close.len(), true),
        None => (kind, xml.len(), false),
    };
    if rest.starts_with("<!--") {
        return through("-->", 4, XmlPiece::CommentOrPi);
    }
    if rest.starts_with("<![CDATA[") {
        return through("]]>", 9, XmlPiece::CData);
    }
    if rest.starts_with("<?") {
        return through("?>", 2, XmlPiece::CommentOrPi);
    }
    let kind = match bytes {
        [b'<', b'!', ..] => XmlPiece::Doctype,
        [b'<', b'/', ..] => XmlPiece::EndTag,
        [b'<', c, ..] if c.is_ascii_alphabetic() || matches!(c, b'_' | b':') || !c.is_ascii() => {
            XmlPiece::StartTag
        }
        // A `<` that opens nothing is text, as xmldom reads it.
        _ => {
            // Search bytes, not `rest[1..]`: the piece may open with a multi-byte character.
            let end = bytes
                .iter()
                .skip(1)
                .position(|&b| b == b'<')
                .map_or(xml.len(), |at| start + 1 + at);
            return (XmlPiece::Text, end, true);
        }
    };
    // A tag (or DOCTYPE) ends at the first `>` outside a quoted value — and, for a DOCTYPE,
    // outside its internal subset's brackets, where a comment or PI is skipped whole: its text
    // is neither a quote nor a bracket.
    let mut quote = None;
    let mut brackets = 0usize;
    let mut i = 1;
    while i < bytes.len() {
        let b = bytes[i];
        match (quote, b) {
            (Some(q), _) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b'<')
                if brackets > 0
                    && kind == XmlPiece::Doctype
                    && (rest[i..].starts_with("<!--") || rest[i..].starts_with("<?")) =>
            {
                let (_, end, terminated) = xml_piece(xml, start + i);
                if !terminated {
                    return (kind, xml.len(), false);
                }
                i = end - start;
                continue;
            }
            (None, b'"' | b'\'') => quote = Some(b),
            (None, b'[') => brackets += 1,
            (None, b']') => brackets = brackets.saturating_sub(1),
            (None, b'>') if brackets == 0 || kind != XmlPiece::Doctype => {
                let kind = if kind == XmlPiece::StartTag && bytes[i - 1] == b'/' {
                    XmlPiece::EmptyTag
                } else {
                    kind
                };
                return (kind, start + i + 1, true);
            }
            _ => {}
        }
        i += 1;
    }
    (kind, xml.len(), false)
}

/// `body` as `sxd_document` must see it to read what xmldom reads, or `None` when xmldom finds
/// it fatal in a way `sxd_document` does not check: a second DOCTYPE, or an unterminated DOCTYPE
/// or comment or PI inside its internal subset.
///
/// - every `&` that starts no reference XML defines becomes `&amp;`, so it reads back as the
///   literal text xmldom keeps (`x&nbsp;y`, `a & b`, and an entity a DOCTYPE declares, which
///   xmldom does not expand either). Only character data and tags (attribute values) are
///   touched: a comment, PI, CDATA section or DOCTYPE holds no references;
/// - a character reference to a surrogate pair becomes the one character the pair encodes, and
///   one to a lone surrogate or past U+10FFFF becomes U+FFFD: xmldom yields a JavaScript string
///   for it that a Rust string cannot hold;
/// - in character data, a `<` that opens no tag becomes `&lt;` and a `]]>` becomes `]]&gt;`,
///   which xmldom both keep as text;
/// - text before the root element becomes one space, as xmldom ignores it — never a tag, CDATA
///   or declaration there, which stay fatal;
/// - a leading BOM becomes the whitespace that may precede the document, so a BOM before an XML
///   declaration still fails, as it does in xmldom;
/// - a DOCTYPE that opens the body gets a space before it: `sxd_document` accepts one only after
///   some other prolog item.
fn prepare_xml(body: &str) -> Option<Cow<'_, str>> {
    let lead = body
        .find(|c: char| !matches!(c, '\u{FEFF}' | ' ' | '\t' | '\r' | '\n'))
        .unwrap_or(body.len());
    let mut out = Rewrite::new(body);
    if body[..lead].contains('\u{FEFF}') {
        out.replace(0, lead, &body[..lead].replace('\u{FEFF}', " "));
    } else if lead == 0 && body.starts_with("<!DOCTYPE") {
        out.replace(0, 0, " ");
    }
    let mut in_prolog = true;
    let mut doctypes = 0;
    let mut pos = lead;
    while pos < body.len() {
        let (kind, end, terminated) = xml_piece(body, pos);
        match kind {
            XmlPiece::Doctype if in_prolog && body[pos..].starts_with("<!DOCTYPE") => {
                doctypes += 1;
                if doctypes > 1 || !terminated {
                    return None;
                }
                for (start, end) in subset_comments_and_pis(&body[pos..end])? {
                    out.replace(pos + start, pos + end, " ");
                }
            }
            XmlPiece::Text if in_prolog => {
                if !body[pos..end]
                    .bytes()
                    .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                {
                    out.replace(pos, end, " ");
                }
            }
            XmlPiece::Text => rewrite_character_data(body, pos, end, true, &mut out),
            XmlPiece::StartTag | XmlPiece::EndTag | XmlPiece::EmptyTag => {
                in_prolog = false;
                rewrite_character_data(body, pos, end, false, &mut out);
            }
            XmlPiece::CommentOrPi | XmlPiece::CData | XmlPiece::Doctype => {}
        }
        pos = end;
    }
    Some(out.finish())
}

/// The byte ranges of the comments and PIs inside a DOCTYPE's internal subset, or `None` when one
/// is unterminated. Neither parser reads the subset, but `sxd_document` ends it at the first `]`,
/// even one inside a comment.
fn subset_comments_and_pis(doctype: &str) -> Option<Vec<(usize, usize)>> {
    let bytes = doctype.as_bytes();
    let mut ranges = Vec::new();
    let mut quote = None;
    let mut brackets = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (Some(q), b) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b'<')
                if brackets > 0
                    && (doctype[i..].starts_with("<!--") || doctype[i..].starts_with("<?")) =>
            {
                let (_, end, terminated) = xml_piece(doctype, i);
                if !terminated {
                    return None;
                }
                ranges.push((i, end));
                i = end;
                continue;
            }
            (None, q @ (b'"' | b'\'')) => quote = Some(q),
            (None, b'[') => brackets += 1,
            (None, b']') => brackets = brackets.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    Some(ranges)
}

/// A copy of a borrowed text made only once the first edit lands: edits go in increasing order.
struct Rewrite<'a> {
    source: &'a str,
    copied: usize,
    out: Option<String>,
}

impl<'a> Rewrite<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            copied: 0,
            out: None,
        }
    }

    /// Replace `source[start..end]` with `with`; `start` is at or after the previous edit's end.
    fn replace(&mut self, start: usize, end: usize, with: &str) {
        let out = self
            .out
            .get_or_insert_with(|| String::with_capacity(self.source.len() + 16));
        out.push_str(&self.source[self.copied..start]);
        out.push_str(with);
        self.copied = end;
    }

    fn finish(self) -> Cow<'a, str> {
        match self.out {
            Some(mut out) => {
                out.push_str(&self.source[self.copied..]);
                Cow::Owned(out)
            }
            None => Cow::Borrowed(self.source),
        }
    }
}

/// Rewrite the references in `body[start..end]` — a text piece when `text`, else a tag — and, in
/// a text piece, the `<` and `]]>` xmldom keeps as text (see [`prepare_xml`]).
fn rewrite_character_data(body: &str, start: usize, end: usize, text: bool, out: &mut Rewrite) {
    let bytes = &body.as_bytes()[..end];
    let mut i = start;
    while i < end {
        match bytes[i] {
            b'<' if text => {
                out.replace(i, i + 1, "&lt;");
                i += 1;
            }
            b']' if text && bytes[i..].starts_with(b"]]>") => {
                out.replace(i + 2, i + 3, "&gt;");
                i += 3;
            }
            b'&' => i = rewrite_reference(bytes, i, out),
            _ => i += 1,
        }
    }
}

/// Rewrite the `&` at `bytes[at]` as [`prepare_xml`] describes; returns where scanning resumes.
fn rewrite_reference(bytes: &[u8], at: usize, out: &mut Rewrite) -> usize {
    const REPLACEMENT: &str = "&#xFFFD;";
    let Some((reference, len)) = xml_reference(&bytes[at + 1..]) else {
        out.replace(at + 1, at + 1, "amp;");
        return at + 1;
    };
    let next = at + 1 + len;
    match reference {
        XmlReference::Entity => next,
        XmlReference::Char(Some(c)) if char::from_u32(c).is_some() => next,
        XmlReference::Char(Some(high @ 0xD800..=0xDBFF)) => {
            let low = if bytes.get(next) == Some(&b'&') {
                xml_reference(&bytes[next + 1..])
            } else {
                None
            };
            match low {
                Some((XmlReference::Char(Some(low @ 0xDC00..=0xDFFF)), low_len)) => {
                    let c = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
                    let pair_end = next + 1 + low_len;
                    out.replace(at, pair_end, &format!("&#x{c:X};"));
                    pair_end
                }
                _ => {
                    out.replace(at, next, REPLACEMENT);
                    next
                }
            }
        }
        XmlReference::Char(_) => {
            out.replace(at, next, REPLACEMENT);
            next
        }
    }
}

/// A reference the parser resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XmlReference {
    /// One of XML's five predefined entities.
    Entity,
    /// A decimal or hex character reference to this code point; `None` when it overflows `u32`.
    Char(Option<u32>),
}

/// The reference the text after an `&` completes, and its length through the `;`: one of XML's
/// five predefined entities, or a decimal or hex character reference. `None` for anything else.
fn xml_reference(after_amp: &[u8]) -> Option<(XmlReference, usize)> {
    let len = after_amp
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || *b == b'#'))
        .unwrap_or(after_amp.len());
    if after_amp.get(len) != Some(&b';') {
        return None;
    }
    // The digits are ASCII, so the text is UTF-8; an overflowing value parses to `None`.
    let code = |digits: &[u8], radix| {
        std::str::from_utf8(digits)
            .ok()
            .and_then(|d| u32::from_str_radix(d, radix).ok())
    };
    let reference = match &after_amp[..len] {
        b"lt" | b"gt" | b"amp" | b"quot" | b"apos" => XmlReference::Entity,
        [b'#', b'x', hex @ ..] if !hex.is_empty() && hex.iter().all(u8::is_ascii_hexdigit) => {
            XmlReference::Char(code(hex, 16))
        }
        [b'#', dec @ ..] if !dec.is_empty() && dec.iter().all(u8::is_ascii_digit) => {
            XmlReference::Char(code(dec, 10))
        }
        _ => return None,
    };
    Some((reference, len + 1))
}

/// `xml` cut back to its root element plus the comments and PIs after it, when the parse failed
/// (at byte `error_at`) only on text or CDATA after the root — which xmldom reads past — or on
/// one stray end tag naming the root, which xmldom also ignores. `None` when there is no closed
/// root, the error lies inside it, or anything else follows it: another tag, a DOCTYPE or an
/// unterminated construct, all of which xmldom treats as fatal.
fn without_trailing_text(xml: &str, error_at: usize) -> Option<String> {
    let mut depth = 0usize;
    let mut pos = 0;
    let mut root = "";
    let root_end = loop {
        if pos >= xml.len() {
            return None;
        }
        let (kind, end, _) = xml_piece(xml, pos);
        match kind {
            XmlPiece::StartTag | XmlPiece::EmptyTag if depth == 0 => {
                root = tag_name(&xml[pos..end]);
                if kind == XmlPiece::EmptyTag {
                    break end;
                }
                depth = 1;
            }
            XmlPiece::StartTag => depth += 1,
            XmlPiece::EndTag => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    break end;
                }
            }
            _ => {}
        }
        pos = end;
    };
    if error_at < root_end {
        return None;
    }
    let mut kept = xml[..root_end].to_string();
    let mut stray_end_tag = false;
    pos = root_end;
    while pos < xml.len() {
        let (kind, end, terminated) = xml_piece(xml, pos);
        match kind {
            _ if !terminated => return None,
            XmlPiece::CommentOrPi => kept.push_str(&xml[pos..end]),
            XmlPiece::Text | XmlPiece::CData => {}
            // xmldom's recovery from any further markup after a stray end tag is a TypeError it
            // happens to survive, not a rule (a documented difference).
            XmlPiece::EndTag if !stray_end_tag && ends_element(&xml[pos..end], root) => {
                stray_end_tag = true;
            }
            XmlPiece::StartTag | XmlPiece::EndTag | XmlPiece::EmptyTag | XmlPiece::Doctype => {
                return None;
            }
        }
        pos = end;
    }
    Some(kept)
}

/// The name a start or empty tag opens: what follows its `<` up to whitespace, `/` or `>`.
fn tag_name(tag: &str) -> &str {
    let name = &tag[1..];
    name.find([' ', '\t', '\r', '\n', '/', '>'])
        .map_or(name, |at| &name[..at])
}

/// Whether `end_tag` is exactly `</name>`, with optional whitespace before the `>`.
fn ends_element(end_tag: &str, name: &str) -> bool {
    end_tag
        .strip_prefix("</")
        .and_then(|rest| rest.strip_prefix(name))
        .is_some_and(|rest| rest.trim_start_matches([' ', '\t', '\r', '\n']) == ">")
}

/// Parse-once-per-request primitive for the XML DOM (issue #711).
///
/// `sxd_document::Package` is `!Send`, and every borrow off it (`Document<'d>`) is lifetime-bound to
/// it — it cannot be stored in `Arc<StubState>` or held across an `.await` point, so this type is
/// deliberately scoped to live only for the synchronous duration of one matching pass: constructed
/// before the stub loop, dropped when matching returns. Within that scope it memoizes the parse (and
/// the parse failure) so N XPath predicates across N stubs in one request share a single
/// [`parse_xml_like_mountebank`] call instead of one each.
pub(crate) struct LazyXmlDom<'a> {
    body: &'a str,
    parsed: OnceCell<Option<sxd_document::Package>>,
    /// The name-folded view case-insensitive predicates select from (issue #1478), built on first
    /// use and then shared the same way.
    folded: OnceCell<Option<sxd_document::Package>>,
}

impl<'a> LazyXmlDom<'a> {
    pub(crate) fn new(body: &'a str) -> Self {
        Self {
            body,
            parsed: OnceCell::new(),
            folded: OnceCell::new(),
        }
    }

    /// The parsed DOM, parsing (and bumping `DOM_PARSE`) on the first call only. `None` if the body
    /// isn't well-formed XML — cached too, so a malformed body doesn't retry the parse per predicate.
    pub(crate) fn document(&self) -> Option<sxd_document::dom::Document<'_>> {
        self.parsed
            .get_or_init(|| parse_xml_like_mountebank(self.body))
            .as_ref()
            .map(sxd_document::Package::as_document)
    }

    /// [`Self::document`] for the name-folded view (see [`parse_xml_folding_names`]).
    pub(crate) fn folded_document(&self) -> Option<sxd_document::dom::Document<'_>> {
        self.folded
            .get_or_init(|| parse_xml_folding_names(self.body))
            .as_ref()
            .map(sxd_document::Package::as_document)
    }
}

/// A request body parsed as JSON on first use, then shared (issue #1442) — the JSON sibling of
/// [`LazyXmlDom`]. Matching used to parse every request body up front, so a large body cost its
/// whole parse tree even when no stub needed it. Now it is parsed only for what compares structure
/// (an object/array body expectation, `exists` on the body, a jsonpath selector, the body index
/// dimensions), and then exactly once per request (#290).
///
/// Not JSON (or no body) is `None`, cached like a success: whether a body is JSON is the question,
/// not an error.
pub(crate) struct LazyJson<'a> {
    body: Option<&'a str>,
    parsed: OnceCell<Option<serde_json::Value>>,
}

impl<'a> LazyJson<'a> {
    pub(crate) fn new(body: Option<&'a str>) -> Self {
        Self {
            body,
            parsed: OnceCell::new(),
        }
    }

    /// No body to parse: what a predicate with a selector compares against instead of the raw body.
    pub(crate) fn absent() -> Self {
        Self::new(None)
    }

    /// The parsed body, parsing on the first call only.
    pub(crate) fn get(&self) -> Option<&serde_json::Value> {
        self.parsed
            .get_or_init(|| {
                let body = self.body?;
                #[cfg(test)]
                counters::bump_json_parse();
                serde_json::from_str(body).ok()
            })
            .as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_json_parses_only_on_first_use() {
        counters::reset();
        let lazy = LazyJson::new(Some(r#"{"a":1}"#));
        assert_eq!(counters::json_parses(), 0, "nothing parsed until asked");
        assert_eq!(lazy.get(), Some(&serde_json::json!({"a": 1})));
        assert!(lazy.get().is_some());
        assert_eq!(counters::json_parses(), 1, "parsed once");
        assert_eq!(LazyJson::new(Some("not json")).get(), None);
        assert_eq!(LazyJson::absent().get(), None);
    }

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
                    "isbn": "0-553-2131-3",
                    "price": 8.99
                },
                {
                    "category": "fiction",
                    "author": "J. R. R. Tolkien",
                    "title": "The Lord of the Rings",
                    "isbn": "0-395-1939-8",
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

    // A quoted name or a filter must end where it ends: the shorthand after it is still rewritten.
    // The pass-through cases below cannot tell, since nothing follows the quote or filter, so a
    // quote that never closes or a filter depth that never returns to zero left them all green
    // (surviving mutants in the weekly cargo-mutants run, #1345).
    #[test]
    fn normalize_rewrites_after_a_quoted_name_or_filter_closes() {
        let cases = [
            // The quote closes, so the `.[` after it is outside it.
            ("$['a'].b.[0]", "$['a'].b..[0]"),
            // An escaped quote does not close the name; the one after it does. The escaped
            // character itself is copied through.
            ("$['it\\'s'].b.[0]", "$['it\\'s'].b..[0]"),
            // A nested `[...]` inside a filter does not end the filter, and the filter's own `]`
            // does.
            ("$[?@.a[0] == 1].b.[0]", "$[?@.a[0] == 1].b..[0]"),
            // A `]` inside a quoted string in a filter is not the filter's end.
            ("$[?@.a[0] == ']'].b.[0]", "$[?@.a[0] == ']'].b..[0]"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_jsonpath(input), expected, "input {input:?}");
        }
        // And the converse: a `.[` after a nested `[...]` but still inside the filter is untouched.
        let inside = "$[?@.a[0] == 1 && @.b.[0] == 2]";
        assert!(
            matches!(normalize_jsonpath(inside), Cow::Borrowed(_)),
            "{inside:?} is one filter and must pass through, got {:?}",
            normalize_jsonpath(inside)
        );
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

    /// Issue #1384: a selector that evaluates to a string, number or boolean substitutes that
    /// scalar. (Mountebank leaves the token in place, or errors on `string()`: a documented
    /// superset.)
    #[test]
    fn scalar_xpath_results_are_substituted() {
        let package = sxd_document::parser::parse("<r><a>x</a><a>y</a></r>").unwrap();
        let doc = package.as_document();
        assert_eq!(
            eval_xpath_on(&doc, "string(//a)", None).as_deref(),
            Some("x")
        );
        assert_eq!(
            eval_xpath_on(&doc, "boolean(//a)", None).as_deref(),
            Some("true")
        );
        assert_eq!(
            eval_xpath_on(&doc, "boolean(//zz)", None).as_deref(),
            Some("false")
        );
        assert_eq!(
            eval_xpath_on(&doc, "count(//a)", None).as_deref(),
            Some("2")
        );
    }

    /// The cache holds many selectors at once: it is only emptied when full, so a second selector
    /// does not evict the first.
    #[test]
    fn distinct_cached_selectors_do_not_evict_each_other() {
        let package = sxd_document::parser::parse("<r><a>x</a></r>").unwrap();
        let doc = package.as_document();
        counters::reset();
        for selector in ["//a[1384]", "//b[1384]", "//a[1384]", "//b[1384]"] {
            let _ = eval_xpath_on(&doc, selector, None);
        }
        assert_eq!(counters::xpath_compiles(), 2, "each selector compiles once");
    }

    /// Issue #1384: the compiled-selector cache is keyed on the selector alone, so one selector
    /// evaluated under two namespace maps must still resolve each against its own bindings.
    #[test]
    fn one_cached_selector_honours_each_namespace_map() {
        let xml = r#"<r xmlns:a="urn:a" xmlns:b="urn:b"><a:n>from-a</a:n><b:n>from-b</b:n></r>"#;
        let bind = |uri: &str| HashMap::from([("p".to_string(), uri.to_string())]);
        assert_eq!(
            extract_xpath_with_ns(xml, "//p:n", Some(&bind("urn:a"))).as_deref(),
            Some("from-a")
        );
        assert_eq!(
            extract_xpath_with_ns(xml, "//p:n", Some(&bind("urn:b"))).as_deref(),
            Some("from-b")
        );
    }

    /// Issue #1455: XML is read for XPath the way Mountebank's xmldom 0.9 reads it — a leading
    /// BOM, text after the root and an entity XML does not define are not errors there — while
    /// what xmldom treats as fatal still yields no document.
    mod xml_like_mountebank {
        use super::*;
        use crate::imposter::Predicate;
        use crate::imposter::predicates::predicate_matches;

        fn holds(predicate: serde_json::Value, body: &str) -> bool {
            let predicate: Predicate = serde_json::from_value(predicate).expect("predicate json");
            predicate_matches(
                &predicate,
                "POST",
                "/",
                None,
                &HashMap::<String, String>::new(),
                Some(body),
                None,
                None,
                None,
                0,
            )
            .expect("evaluates")
        }

        fn exists(selector: &str, present: bool) -> serde_json::Value {
            serde_json::json!({ "exists": { "body": present }, "xpath": { "selector": selector } })
        }

        /// Every XPath entry point reads `body` as `expected`: a copy (first value and all
        /// values), the request's shared DOM, and an `exists` / `equals` predicate.
        fn assert_reads(body: &str, selector: &str, expected: &[&str]) {
            let method = ExtractionMethod::XPath {
                selector: selector.to_string(),
                namespaces: None,
            };
            let all: Vec<Option<String>> = expected.iter().map(|v| Some(v.to_string())).collect();
            assert_eq!(method.matches(body), all, "copy, all values: {body:?}");
            assert_eq!(
                extract_xpath(body, selector).as_deref(),
                expected.first().copied(),
                "copy, first value: {body:?}"
            );
            let dom = LazyXmlDom::new(body);
            let values = dom
                .document()
                .and_then(|doc| eval_xpath_all_on(&doc, selector, None));
            assert_eq!(
                values,
                Some(expected.iter().map(|v| v.to_string()).collect()),
                "shared DOM: {body:?}"
            );
            assert!(holds(exists(selector, true), body), "exists true: {body:?}");
            assert!(
                !holds(exists(selector, false), body),
                "exists false: {body:?}"
            );
            let equals = serde_json::json!({ "equals": { "body": expected[0] }, "caseSensitive": true,
                                             "xpath": { "selector": selector } });
            assert!(holds(equals, body), "equals: {body:?}");
        }

        /// `body` is no document: nothing selected anywhere, and `exists: true` does not hold.
        fn assert_no_document(body: &str) {
            assert!(
                LazyXmlDom::new(body).document().is_none(),
                "{body:?} parsed"
            );
            assert_eq!(extract_xpath(body, "//b"), None, "{body:?}");
            assert!(xpath_selection_in(body, "//b", None).is_none(), "{body:?}");
            assert!(!holds(exists("//b", true), body), "{body:?}");
        }

        #[test]
        fn a_leading_bom_is_ignored() {
            assert_reads("\u{FEFF}<a><b>1</b></a>", "//b", &["1"]);
        }

        #[test]
        fn text_after_the_root_is_ignored() {
            assert_reads("<a><b>1</b></a>trailing", "//b", &["1"]);
            assert_reads("<a><b>1</b></a>x &nbsp; y", "//b", &["1"]);
            assert_reads("<a><b>1</b></a><![CDATA[x]]>", "//b", &["1"]);
        }

        #[test]
        fn whitespace_comments_and_pis_after_the_root_still_parse() {
            assert_reads("<a><b>1</b></a>\n<!--c--><?pi x?>  ", "//b", &["1"]);
            assert_reads("<a><b>1</b></a><!--c-->junk", "//comment()", &["c"]);
        }

        #[test]
        fn an_undefined_entity_reads_literally() {
            assert_reads("<a><b>x&nbsp;y</b></a>", "//b", &["x&nbsp;y"]);
        }

        #[test]
        fn predefined_entities_decode_and_undefined_ones_stay_literal() {
            assert_reads(
                "<a><b>&amp;&lt;</b><b>&copy;</b></a>",
                "//b",
                &["&<", "&copy;"],
            );
            assert_reads("<a><b>&#169;&#xA9;&amp;nbsp;</b></a>", "//b", &["©©&nbsp;"]);
        }

        #[test]
        fn an_ampersand_that_starts_no_reference_reads_literally() {
            assert_reads("<a><b>a & b&nbsp&lt</b></a>", "//b", &["a & b&nbsp&lt"]);
            assert_reads("<a><b>é&nbsp;ü</b></a>é trailing ü", "//b", &["é&nbsp;ü"]);
        }

        #[test]
        fn cdata_comments_and_pis_are_not_rewritten() {
            assert_reads("<a><b><![CDATA[x&nbsp;y]]></b></a>", "//b", &["x&nbsp;y"]);
            assert_reads(
                "<a><!-- &nbsp; --><b>1</b></a>",
                "//comment()",
                &[" &nbsp; "],
            );
            assert_reads(
                "<a><?pi &nbsp;?><b>1</b></a>",
                "//processing-instruction()",
                &["&nbsp;"],
            );
        }

        #[test]
        fn an_undefined_entity_in_an_attribute_reads_literally() {
            assert_reads(r#"<a><b x="a&nbsp;b">1</b></a>"#, "//b/@x", &["a&nbsp;b"]);
            assert_reads(r#"<a><b x='a > &amp; b'>1</b></a>"#, "//b/@x", &["a > & b"]);
        }

        // xmldom 0.9 does not expand an entity the internal DTD subset declares: it reads it
        // literally, like any other undefined entity.
        #[test]
        fn a_doctype_declared_entity_reads_literally() {
            assert_reads(
                r#"<!DOCTYPE a [<!ENTITY e "x&amp;">]><a><b>&e;</b></a>"#,
                "//b",
                &["&e;"],
            );
            assert_reads(
                r#"<?xml version="1.0"?><!DOCTYPE a [<!ENTITY e "x">]><a><b>&e;</b></a>"#,
                "//b",
                &["&e;"],
            );
            assert_reads("<!DOCTYPE a><a><b>1</b></a>", "//b", &["1"]);
        }

        // Issue #1479: the recoveries xmldom reports through `onError` rather than a throw, so
        // Mountebank's `xpath.select` yields a value for each. Expected values are what
        // Mountebank 2.9.5's own `xpath.select` returns, except where a test says otherwise.
        #[test]
        fn a_less_than_that_opens_no_tag_reads_as_text() {
            assert_reads("<a>1 < 2</a>", "//a", &["1 < 2"]);
            assert_reads("<r><a>1 < 2</a><b>ok</b></r>", "//b", &["ok"]);
            assert_reads("<a>x<</a>", "//a", &["x<"]);
            assert_reads("<a>1 &lt; 2 < 3</a>", "//a", &["1 < 2 < 3"]);
            assert_reads("<a>x <= y & z</a>", "//a", &["x <= y & z"]);
        }

        #[test]
        fn a_cdata_close_in_text_reads_as_text() {
            assert_reads("<a>x]]>y</a>", "//a", &["x]]>y"]);
            assert_reads("<a>]]]></a>", "//a", &["]]]>"]);
        }

        #[test]
        fn one_stray_end_tag_naming_the_root_is_ignored() {
            assert_reads("<a>v</a></a>", "//a", &["v"]);
            assert_reads("<a>v</a></a>x", "//a", &["v"]);
            assert_reads("<a>v</a>x</a>", "//a", &["v"]);
            assert_reads("<a>v</a></a ><?pi?><!--c-->", "//a", &["v"]);
            assert_reads("<r><a>v</a></r></r>", "//a", &["v"]);
            assert_reads("<a><b>1</b></a></a>", "//b", &["1"]);
            assert!(LazyXmlDom::new("<a></a></a>").document().is_some());
        }

        #[test]
        fn a_surrogate_pair_of_references_is_one_character() {
            assert_reads("<a>&#55357;&#56832;</a>", "//a", &["😀"]);
            assert_reads("<a>&#xD83D;&#xDE00;</a>", "//a", &["😀"]);
            assert_reads("<a>&#xd800;&#xdc00;</a>", "//a", &["\u{10000}"]);
        }

        // Not Mountebank's value: it yields a lone surrogate (or garbage past U+10FFFF), which a
        // Rust string cannot hold. U+FFFD is Rift's documented deviation.
        #[test]
        fn a_lone_surrogate_or_out_of_range_reference_reads_as_a_replacement_character() {
            assert_reads("<a>&#xD800;</a>", "//a", &["\u{FFFD}"]);
            assert_reads("<a>&#xDE00;&#xD83D;</a>", "//a", &["\u{FFFD}\u{FFFD}"]);
            assert_reads("<a>&#xD83D;x&#xDE00;</a>", "//a", &["\u{FFFD}x\u{FFFD}"]);
            assert_reads("<a>&#99999999999;</a>", "//a", &["\u{FFFD}"]);
            assert_reads("<a>&#x110000;</a>", "//a", &["\u{FFFD}"]);
            assert_reads(r#"<a b="&#xD800;">v</a>"#, "//a/@b", &["\u{FFFD}"]);
            assert_reads("<a>&lt;&#xD800;</a>", "//a", &["<\u{FFFD}"]);
        }

        #[test]
        fn text_before_the_root_is_ignored() {
            assert_reads("<?xml version=\"1.0\"?>\u{FEFF}<a>v</a>", "//a", &["v"]);
            assert_reads("<?xml version=\"1.0\"?>x<a>v</a>", "//a", &["v"]);
            assert_reads("x<a>v</a>", "//a", &["v"]);
            assert_reads("<!--c-->x<a>v</a>", "//a", &["v"]);
            assert_reads("<!DOCTYPE a>x<a>v</a>", "//a", &["v"]);
            assert_reads("x<!DOCTYPE a><a>v</a>", "//a", &["v"]);
            assert_reads("x <!--c--> y<a>v</a>", "//a", &["v"]);
            assert_reads("1 < 2<a>v</a>", "//a", &["v"]);
            assert_reads("x&nbsp;<a>v</a>", "//a", &["v"]);
            assert_reads("x]]><a>v</a>", "//a", &["v"]);
        }

        #[test]
        fn a_doctype_subset_skips_its_comments_and_pis() {
            assert_reads(
                "<!DOCTYPE a [<!-- it's --> ]><a>&nbsp;</a>",
                "//a",
                &["&nbsp;"],
            );
            assert_reads("<!DOCTYPE a [<!-- ] > --> ]><a>&amp;</a>", "//a", &["&"]);
            assert_reads(
                "<!DOCTYPE a [<?pi it's?> ]><a>&nbsp;</a>",
                "//a",
                &["&nbsp;"],
            );
            assert_reads("<!DOCTYPE a [<?pi ] > ?>]><a>v</a>", "//a", &["v"]);
        }

        #[test]
        fn what_xmldom_recovers_by_accident_stays_fatal() {
            for body in [
                // A `<` that starts a name but parses as no tag: documented, not copied.
                "<a>x <b</a>",
                "<a>if (i<n)</a>",
                // Markup after a stray end tag: documented, not copied.
                "<a></a></a><b>",
                "<a>v</a></a><b/>",
                "<a>v</a></a></a>",
                // A stray end tag that does not name the root is fatal in xmldom too.
                "<a></a></x>",
                "<a>v</a></b>",
                "<a>v</a></ a>",
                "<a>v</a></a x=\"1\">",
                // The prolog drops text, never a tag, CDATA, a declaration or a second DOCTYPE.
                "x<b/><a>v</a>",
                "x</b><a>v</a>",
                "x<?xml version=\"1.0\"?><a>v</a>",
                "<![CDATA[x]]><a>v</a>",
                "x<![CDATA[x]]><a>v</a>",
                "x<!DOCTYPE a><!DOCTYPE a><a>v</a>",
                "<!--c--><!DOCTYPE a><!DOCTYPE a><a>v</a>",
                "\u{FEFF}<?xml version=\"1.0\"?><a>v</a>",
                r#"<a b="1 < 2"/>"#,
                "<a>x<!y</a>",
                // An unterminated comment or PI inside a DOCTYPE's internal subset.
                "<!DOCTYPE a [<!--x->]><a>v</a>",
                "<!DOCTYPE a [<?pi ]><a>v</a>",
                // A `]` quoted in the internal subset: sxd ends the subset there. Documented.
                r#"<!DOCTYPE a [<!-- --> <!ENTITY x "]">]><a>v</a>"#,
            ] {
                assert_no_document(body);
            }
        }

        fn xml_token() -> impl proptest::strategy::Strategy<Value = &'static str> {
            proptest::sample::select(vec![
                "<",
                ">",
                "&",
                ";",
                "#",
                "x",
                "]]>",
                "-->",
                "?>",
                "<!--",
                "<![CDATA[",
                "<!DOCTYPE ",
                "\"",
                "'",
                "\u{FEFF}",
                "é",
                "😀",
                "a",
                "/",
                "[",
                "]",
                " ",
                "<a>",
                "</a>",
                "&amp;",
                "&nbsp;",
                "&#x41;",
                "<?",
                "</a >",
                "<a/>",
                "&#xD83D;",
                "&#xDE00;",
                "&#55357;",
                "&#99999999999;",
                "&#x110000;",
                "&#",
                "<?xml version=\"1.0\"?>",
                "1 < 2",
                "text",
            ])
        }

        proptest::proptest! {
            #![proptest_config(proptest::prelude::ProptestConfig {
                cases: 1000,
                failure_persistence: None,
                ..proptest::prelude::ProptestConfig::default()
            })]

            #[test]
            fn preparing_any_text_never_panics(
                tokens in proptest::collection::vec(xml_token(), 0..40)
            ) {
                let _ = parse_xml_like_mountebank(&tokens.concat());
            }

            #[test]
            fn folding_any_text_never_panics_and_is_idempotent(
                tokens in proptest::collection::vec(xml_token(), 0..40)
            ) {
                let body = tokens.concat();
                let _ = parse_xml_folding_names(&body);
                if let Some(prepared) = prepare_xml(&body) {
                    let once = fold_xml_names(&prepared).into_owned();
                    proptest::prop_assert_eq!(fold_xml_names(&once).into_owned(), once);
                }
                let once = fold_xml_names(&body).into_owned();
                proptest::prop_assert_eq!(fold_xml_names(&once).into_owned(), once);
            }
        }

        #[test]
        fn folding_lowercases_names_values_text_cdata_comments_and_pi_data() {
            assert_eq!(
                fold_xml_names(
                    r#"<?xml version="1.0" encoding="UTF-8"?><?Target Data?><X:Root xmlns:X="urn:Up" Id="V"><!-- Note --><Child A='B'>Text &amp; More<![CDATA[<B>Raw</B>]]></Child></X:Root >"#
                ),
                r#"<?xml version="1.0" encoding="utf-8"?><?Target data?><x:root xmlns:x="urn:up" id="v"><!-- note --><child a='b'>text &amp; more<![CDATA[<b>raw</b>]]></child></x:root >"#
            );
        }

        #[test]
        fn folding_keeps_the_markers_and_the_doctype_readable() {
            let doctype = "<!DOCTYPE A [<!ENTITY E 'X'>]>";
            let folded = fold_xml_names(&format!("{doctype}<A><![CDATA[Q]]></A>")).into_owned();
            assert_eq!(folded, format!("{doctype}<a><![CDATA[q]]></a>"));
            assert!(matches!(fold_xml_names("<a b='c'>d</a>"), Cow::Borrowed(_)));
            let package =
                parse_xml_folding_names("<!DOCTYPE A><A><![CDATA[XY]]></A>").expect("parses");
            assert_eq!(
                eval_xpath_all_on(&package.as_document(), "//a", None),
                Some(vec!["xy".to_string()])
            );
        }

        #[test]
        fn folding_is_idempotent() {
            for body in [
                "<A><B>1</B></A>",
                r#"<X:A xmlns:X="urn:X"><X:B Id='Q'>T</X:B></X:A>"#,
                "<!DOCTYPE A><A><![CDATA[Q]]></A>",
            ] {
                let once = fold_xml_names(body).into_owned();
                assert_eq!(fold_xml_names(&once), once);
            }
        }

        #[test]
        fn folded_names_collide_as_in_mountebank() {
            let package = parse_xml_folding_names("<r><a>1</a><A>2</A></r>").expect("parses");
            assert_eq!(
                eval_xpath_all_on(&package.as_document(), "//a", None),
                Some(vec!["1".to_string(), "2".to_string()])
            );
        }

        #[test]
        fn the_shared_dom_folds_once() {
            let dom = LazyXmlDom::new("<A><B>1</B></A>");
            counters::reset();
            for _ in 0..3 {
                let doc = dom.folded_document().expect("parses");
                assert_eq!(
                    eval_xpath_all_on(&doc, "//b", None),
                    Some(vec!["1".to_string()])
                );
            }
            assert!(dom.document().is_some());
            assert_eq!(
                counters::dom_parses(),
                2,
                "one raw parse and one folded parse"
            );
        }

        #[test]
        fn a_case_insensitive_selector_folds_literals_too() {
            assert_eq!(
                fold_xpath_selector("//X:B[@Id='Q' or text()=\"Mixed\"]/COUNT"),
                "//x:b[@id='q' or text()=\"mixed\"]/count"
            );
            assert!(matches!(fold_xpath_selector("//a"), Cow::Borrowed(_)));
        }

        #[test]
        fn a_folded_selector_naming_an_operator_still_compiles() {
            for selector in ["//DIV", "//a/MOD", "//AND/OR", "//a[@DIV]"] {
                assert_eq!(validate_xpath_selector(selector), Ok(()), "{selector}");
                assert_eq!(
                    validate_xpath_selector(&fold_xpath_selector(selector)),
                    Ok(()),
                    "{selector}"
                );
            }
        }

        #[test]
        fn a_body_needing_no_preparation_is_not_copied() {
            assert!(matches!(
                prepare_xml("<a><b x='1'>&amp;&#65;&#x1F600;</b><!-- & -->\n<![CDATA[<]]></a>"),
                Some(Cow::Borrowed(_))
            ));
        }

        #[test]
        fn fatal_errors_still_yield_no_document() {
            for body in [
                "<x:a><b>1</b></x:a>",
                "<a><b>1</b><br></a>",
                "<a><b>1</b>",
                "<a><b>1</b></a><c>2</c>",
                "<a><b>1</b></a>junk<c/>",
                "<a><b>1</b></a></x>",
                "\u{FEFF}",
                "trailing",
                "",
                "\u{FEFF}<?xml version=\"1.0\"?><a><b>1</b></a>",
                "<!DOCTYPE a><!DOCTYPE a><b/>",
                "<!DOCTYPE a> <!DOCTYPE a><a><b>1</b></a>",
            ] {
                assert_no_document(body);
            }
        }

        // Issue #1495: a selected node reads as Mountebank's `xpath.js` `nodeValue` reads it, over
        // xmldom's node model. Expected values are Mountebank 2.9.5's, probed live.
        #[test]
        fn an_element_reads_as_its_first_child_node() {
            assert_reads("<a>xy</a>", "//a", &["xy"]);
            assert_reads("<a>x<b/>y</a>", "//a", &["x"]);
            assert_reads("<a>\n  <b>v</b>\n</a>", "//a", &["\n  "]);
            assert_reads("<a><![CDATA[p]]>q</a>", "//a", &["p"]);
            assert_reads("<a><!--c-->x</a>", "//a", &["c"]);
            assert_reads("<a><?pi d?>x</a>", "//a", &["d"]);
            assert_reads("<a>x&amp;y<b/>z</a>", "//a", &["x&y"]);
        }

        #[test]
        fn an_element_with_no_text_first_reads_undefined() {
            assert_reads("<a><b>in</b>y</a>", "//a", &["undefined"]);
            assert_reads("<a/>", "//a", &["undefined"]);
            assert_reads("<a></a>", "//a", &["undefined"]);
            assert_reads("<a><![CDATA[]]></a>", "//a", &["undefined"]);
            assert_reads("<r><a>1</a><a/></r>", "//a", &["1", "undefined"]);
        }

        #[test]
        fn a_pi_without_data_reads_empty() {
            let values = |body: &str, selector: &str| {
                let package = parse_xml_like_mountebank(body).expect("parses");
                eval_xpath_all_on(&package.as_document(), selector, None).expect("evaluates")
            };
            assert_eq!(values("<a><?pi?>x</a>", "//a"), [""]);
        }

        #[test]
        fn text_attribute_and_scalar_results_are_unchanged() {
            assert_reads(r#"<a k="v">t</a>"#, "//a/@k", &["v"]);
            assert_reads("<a>x<b/>y</a>", "//a/text()", &["x", "y"]);
            assert_reads("<a>t</a>", "string(//a)", &["t"]);
            assert_reads("<a>t</a><!--x-->", "count(//a)", &["1"]);
        }

        #[test]
        fn a_run_of_text_and_references_is_one_text_node() {
            assert_reads("<a>x&amp;y</a>", "//a/text()", &["x&y"]);
            assert_reads("<a>x&#65;y</a>", "//a/text()", &["xAy"]);
            assert_reads("<a>x&nbsp;y</a>", "//a/text()", &["x&nbsp;y"]);
            assert_reads("<a>&amp;&amp;</a>", "//a/text()", &["&&"]);
            assert_reads("<a>x&amp;y</a>", "count(//a/text())", &["1"]);
        }

        #[test]
        fn a_cdata_section_is_its_own_node_and_an_empty_one_is_none() {
            assert_reads("<a>x<![CDATA[p]]>y</a>", "count(//a/text())", &["3"]);
            assert_reads("<a>x<![CDATA[p]]>y</a>", "//a/text()", &["x", "p", "y"]);
            assert_reads("<a><![CDATA[]]></a>", "count(//a/text())", &["0"]);
            assert_reads("<a>x<![CDATA[]]>y</a>", "//a/text()", &["x", "y"]);
        }

        #[test]
        fn the_root_reads_as_its_first_node() {
            assert_reads(r#"<?xml version="1.0"?><r/>"#, "/", &[r#"version="1.0""#]);
            assert_reads("<!--c--><r/>", "/", &["c"]);
            assert_reads("<!DOCTYPE r><r/>", "/", &["undefined"]);
            assert_reads("<r/>", "/", &["undefined"]);
        }

        #[test]
        fn a_namespace_node_reads_undefined() {
            let package = parse_xml_like_mountebank(r#"<a xmlns:p="u">t</a>"#).expect("parses");
            let values = eval_xpath_all_on(&package.as_document(), "//a/namespace::*", None)
                .expect("evaluates");
            assert!(!values.is_empty());
            assert!(values.iter().all(|v| v == "undefined"), "{values:?}");
        }

        #[test]
        fn the_folded_view_gets_the_same_node_model() {
            let package = parse_xml_folding_names("<A>X&amp;Y<B/>Z</A>").expect("parses");
            let values = eval_xpath_all_on(&package.as_document(), "//a", None).expect("evaluates");
            assert_eq!(values, ["x&y"]);
        }

        #[test]
        fn text_tokens_counts_sxds_text_nodes() {
            assert_eq!(text_tokens("xy"), 1);
            assert_eq!(text_tokens("x&amp;y"), 3);
            assert_eq!(text_tokens("&amp;&amp;"), 2);
            assert_eq!(text_tokens("&amp;y"), 2);
            assert_eq!(text_tokens("x&#65;"), 2);
        }
    }
}
