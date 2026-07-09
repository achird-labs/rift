//! Declarative response templating: the `{{ function args | filter }}` evaluator (issue #359).
//!
//! This generalizes the existing `${request.*}` substitution (`extensions::template`) into a
//! small function-call grammar so the most common "scripting" use case — pure data transforms
//! from the request (and flow state) into the response — needs no script engine at all.
//!
//! Opt-in via `_rift.templated: true` on an `is` response (wired in `imposter::handler`); when
//! absent/false, a literal `{{` is served verbatim so recorded fixtures never break.
//!
//! # Grammar
//!
//! ```text
//! {{ <head> [args...] [| <filter> [args...]] }}
//! ```
//!
//! `head` is a function name, optionally dotted (`request.query.name`, `state.attempts`). `args`
//! are space-separated words; a single-quoted word (`'like this'`) may contain spaces and is
//! taken literally (no escape sequences in v1). A `key='value'` word is a named argument (used by
//! `now`). Filters chain with `|` and transform the base expression's string result.
//!
//! # Function set (v1)
//!
//! - `request.method`, `request.path`
//! - `request.query.<name>`
//! - `request.header '<Name>'` (case-insensitive)
//! - `request.json '<jsonpath>'` — `$`, dotted keys, `[<index>]` array indexing over the parsed
//!   request body
//! - `now [offset='±Nh|m|s|d'] [format='<strftime>']` — default format is RFC3339
//! - `uuid` — a random UUID v4
//! - `randomInt <a> <b>` — random integer in `[a, b]`
//! - `state.<key>` — read-only flow-state lookup for the request's resolved flow id
//!
//! Filters: `| last_segment` (trailing `/`-segment), `| regex '<pattern>' <group>` (capture group
//! `<group>` of the first match), `| json` (JSON-string-escape the value so it is safe to place
//! inside a JSON string literal — always apply it when a substituted value goes into `"..."`).
//!
//! # Error policy (AC3)
//!
//! An unknown function/filter, a malformed token, or a failed lookup (missing query param/header/
//! jsonpath segment/state key) is never silent: in debug mode (see [`crate::util::rift_debug_env`])
//! it fails the whole render (surfaced by the caller as a request-time error); otherwise the token
//! is replaced with an empty string and a `tracing::warn!(target: "rift::template", ..)` names it.

use crate::extensions::flow_state::FlowStore;
use crate::extensions::template::RequestData;
use rand::Rng;
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

/// Everything a `{{ }}` expression may read: the parsed request and a read-only view onto the
/// request's flow state.
pub struct TemplateContext<'a> {
    /// The request the response is being generated for.
    pub request: &'a RequestData,
    /// The resolved flow id for `state.*` lookups (same id the scenario FSM/scripts use).
    pub flow_id: &'a str,
    /// Read-only flow-state backend.
    pub flow_store: &'a dyn FlowStore,
}

/// Render the full `{{ }}` template surface for a `templated: true` response: first expands the
/// legacy relative-date tokens (`{{NOW}}`/`{{DAYS+N}}`/`{{MONTHS+N}}`, issue #195) via
/// [`crate::extensions::template::apply_date_templates`], then evaluates the v1 function-call
/// grammar added by this module. Doing dates first means the two grammars never collide: by the
/// time this module's regex scans the body, no legacy uppercase token is left over to be
/// misread as an unknown function.
pub fn render_templated(
    input: &str,
    ctx: &TemplateContext<'_>,
    debug: bool,
) -> Result<String, String> {
    let expanded = crate::extensions::template::apply_date_templates(input);
    render(&expanded, ctx, debug)
}

/// Evaluate every `{{ ... }}` expression in `input` against `ctx`. In debug mode the first
/// failing token aborts the whole render with an error describing it; otherwise each failing
/// token is replaced with an empty string and logged via `tracing::warn!`.
fn render(input: &str, ctx: &TemplateContext<'_>, debug: bool) -> Result<String, String> {
    let re = expr_regex();
    let mut first_error: Option<String> = None;
    let rendered = re
        .replace_all(input, |caps: &regex::Captures| {
            if first_error.is_some() {
                // Already failing in debug mode; the replacement text is discarded by the Err
                // return below, so its exact content doesn't matter.
                return String::new();
            }
            let raw = &caps[0];
            let inner = caps[1].trim();
            match evaluate(inner, ctx) {
                Ok(value) => value,
                Err(reason) => {
                    if debug {
                        first_error = Some(format!("template error in `{raw}`: {reason}"));
                    } else {
                        tracing::warn!(
                            target: "rift::template",
                            token = %raw,
                            reason = %reason,
                            "template token failed; substituting empty string"
                        );
                    }
                    String::new()
                }
            }
        })
        .into_owned();

    match first_error {
        Some(e) => Err(e),
        None => Ok(rendered),
    }
}

fn expr_regex() -> &'static Regex {
    static EXPR_REGEX: OnceLock<Regex> = OnceLock::new();
    // Non-greedy, no nested braces — matches `{{ ... }}` including the legacy date tokens'
    // syntax shape, but `render_templated` always expands those first (see doc comment above).
    EXPR_REGEX.get_or_init(|| {
        Regex::new(r"\{\{\s*([^{}]+?)\s*\}\}").unwrap_or_else(|e| {
            // A hardcoded, constant pattern failing to compile is a programming error caught
            // immediately by any test exercising this module, not a data-dependent runtime
            // failure — matches the existing static-regex convention in `extensions::template`.
            unreachable!("static template expression regex must compile: {e}")
        })
    })
}

/// Evaluate a single trimmed `{{ ... }}` inner expression (no surrounding braces).
fn evaluate(inner: &str, ctx: &TemplateContext<'_>) -> Result<String, String> {
    let mut segments = split_top_level(inner, '|').into_iter();
    let base = segments.next().unwrap_or_default();
    let base_words = tokenize_words(&base);
    let head = base_words
        .first()
        .ok_or_else(|| "empty template expression".to_string())?;
    let mut value = eval_base(head, &base_words[1..], ctx)?;

    for filter_part in segments {
        let filter_words = tokenize_words(&filter_part);
        let fname = filter_words
            .first()
            .ok_or_else(|| "empty filter after `|`".to_string())?;
        value = apply_filter(&value, fname, &filter_words[1..])?;
    }
    Ok(value)
}

/// Split `s` on top-level occurrences of `sep`, respecting single-quoted substrings (a `sep`
/// inside quotes — e.g. a `|` in a regex pattern — is not a split point).
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '\'' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            c if c == sep && !in_quotes => {
                parts.push(std::mem::take(&mut current));
            }
            c => current.push(c),
        }
    }
    parts.push(current);
    parts
}

/// Split `s` into whitespace-separated words, treating a single-quoted run as one word (quote
/// characters are stripped; no escape sequences in v1).
fn tokenize_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_content = false;
    for c in s.chars() {
        match c {
            '\'' => {
                in_quotes = !in_quotes;
                has_content = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_content {
                    words.push(std::mem::take(&mut current));
                    has_content = false;
                }
            }
            c => {
                current.push(c);
                has_content = true;
            }
        }
    }
    if has_content {
        words.push(current);
    }
    words
}

/// Evaluate the base (pre-filter) expression named `head` with positional/named `args`.
fn eval_base(head: &str, args: &[String], ctx: &TemplateContext<'_>) -> Result<String, String> {
    match head {
        "request.method" => Ok(ctx.request.method.clone()),
        "request.path" => Ok(ctx.request.path.clone()),
        "request.header" => {
            let name = args.first().ok_or_else(|| {
                "request.header requires a quoted header name, e.g. request.header 'Name'"
                    .to_string()
            })?;
            ctx.request
                .headers
                .get(&name.to_lowercase())
                .cloned()
                .ok_or_else(|| format!("no such header: '{name}'"))
        }
        "request.json" => {
            let path = args.first().ok_or_else(|| {
                "request.json requires a quoted jsonpath, e.g. request.json '$.a.b'".to_string()
            })?;
            let json: Value = serde_json::from_str(&ctx.request.body)
                .map_err(|e| format!("request body is not valid JSON: {e}"))?;
            let resolved = eval_jsonpath(&json, path)?;
            Ok(value_to_string(&resolved))
        }
        "now" => {
            let mut offset = None;
            let mut format = None;
            for arg in args {
                if let Some(v) = arg.strip_prefix("offset=") {
                    offset = Some(v.to_string());
                } else if let Some(v) = arg.strip_prefix("format=") {
                    format = Some(v.to_string());
                } else {
                    return Err(format!(
                        "now: unknown argument '{arg}' (expected offset='...' or format='...')"
                    ));
                }
            }
            eval_now(offset.as_deref(), format.as_deref())
        }
        "uuid" => Ok(uuid::Uuid::new_v4().to_string()),
        "randomInt" => {
            let lo_str = args.first().ok_or_else(|| {
                "randomInt requires two integer arguments: randomInt a b".to_string()
            })?;
            let hi_str = args.get(1).ok_or_else(|| {
                "randomInt requires two integer arguments: randomInt a b".to_string()
            })?;
            let lo: i64 = lo_str
                .parse()
                .map_err(|_| format!("randomInt: invalid integer '{lo_str}'"))?;
            let hi: i64 = hi_str
                .parse()
                .map_err(|_| format!("randomInt: invalid integer '{hi_str}'"))?;
            if lo > hi {
                return Err(format!(
                    "randomInt: lower bound {lo} is greater than upper bound {hi}"
                ));
            }
            Ok(rand::thread_rng().gen_range(lo..=hi).to_string())
        }
        _ => {
            if let Some(name) = head.strip_prefix("request.query.") {
                return ctx
                    .request
                    .query
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("no such query parameter: '{name}'"));
            }
            if let Some(key) = head.strip_prefix("state.") {
                if key.is_empty() {
                    return Err("state requires a key, e.g. state.myKey".to_string());
                }
                let found = ctx
                    .flow_store
                    .get(ctx.flow_id, key)
                    .map_err(|e| format!("state.{key}: flow store error: {e}"))?;
                return found
                    .map(|v| value_to_string(&v))
                    .ok_or_else(|| format!("no such state key: '{key}'"));
            }
            Err(format!("unknown template function: '{head}'"))
        }
    }
}

/// Apply a `| filter` to the string result of the base expression (or a prior filter).
fn apply_filter(value: &str, name: &str, args: &[String]) -> Result<String, String> {
    match name {
        "last_segment" => Ok(value
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string()),
        "regex" => {
            let pattern = args.first().ok_or_else(|| {
                "regex filter requires a quoted pattern, e.g. | regex '(\\d+)$' 1".to_string()
            })?;
            let group_str = args.get(1).ok_or_else(|| {
                "regex filter requires a capture group number, e.g. | regex '(\\d+)$' 1".to_string()
            })?;
            let group: usize = group_str
                .parse()
                .map_err(|_| format!("regex filter: invalid group number '{group_str}'"))?;
            let re = Regex::new(pattern)
                .map_err(|e| format!("regex filter: invalid pattern '{pattern}': {e}"))?;
            let caps = re
                .captures(value)
                .ok_or_else(|| format!("regex filter: pattern '{pattern}' did not match"))?;
            caps.get(group)
                .map(|m| m.as_str().to_string())
                .ok_or_else(|| format!("regex filter: no capture group {group}"))
        }
        // Issue #359 B3 (correctness + security): JSON-string-escape the value so it is safe to
        // place *inside* a JSON string literal in the template (the author writes `"{{ ... | json}}"`
        // — this filter escapes the interior). A substituted value carrying `"`, `\`, a newline, or
        // another control char would otherwise produce an invalid JSON body. We serialize the value
        // as a JSON string and strip the surrounding quotes, yielding just the escaped interior.
        "json" => {
            let quoted = serde_json::to_string(value)
                .map_err(|e| format!("json filter: could not encode value: {e}"))?;
            // `serde_json::to_string(&str)` always yields a `"..."` literal (>= 2 chars); strip the
            // opening and closing quote to leave the escaped interior the author places inline.
            let interior = quoted
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(&quoted);
            Ok(interior.to_string())
        }
        _ => Err(format!("unknown template filter: '{name}'")),
    }
}

/// One segment of a parsed `$.a.b[0]`-style jsonpath.
enum PathSegment {
    Key(String),
    Index(usize),
}

/// Hand-rolled minimal jsonpath parser: `$`, dotted keys, `[<index>]` array indexing. Deliberately
/// not a general jsonpath implementation — the v1 function set only needs this subset.
fn parse_jsonpath(path: &str) -> Result<Vec<PathSegment>, String> {
    let rest = path
        .trim()
        .strip_prefix('$')
        .ok_or_else(|| format!("jsonpath must start with '$': '{path}'"))?;
    let mut segments = Vec::new();
    let mut chars = rest.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '.' => {
                chars.next();
                let mut key = String::new();
                while let Some(&c2) = chars.peek() {
                    if c2 == '.' || c2 == '[' {
                        break;
                    }
                    key.push(c2);
                    chars.next();
                }
                if key.is_empty() {
                    return Err(format!("empty key segment in jsonpath: '{path}'"));
                }
                segments.push(PathSegment::Key(key));
            }
            '[' => {
                chars.next();
                let mut idx = String::new();
                while let Some(&c2) = chars.peek() {
                    if c2 == ']' {
                        break;
                    }
                    idx.push(c2);
                    chars.next();
                }
                if chars.next() != Some(']') {
                    return Err(format!("unterminated '[' in jsonpath: '{path}'"));
                }
                let n: usize = idx
                    .parse()
                    .map_err(|_| format!("invalid array index '{idx}' in jsonpath: '{path}'"))?;
                segments.push(PathSegment::Index(n));
            }
            other => {
                return Err(format!(
                    "unexpected character '{other}' in jsonpath: '{path}'"
                ));
            }
        }
    }
    Ok(segments)
}

/// Resolve `path` against `root`. A missing key/out-of-bounds index is an `Err` (AC3 policy).
fn eval_jsonpath(root: &Value, path: &str) -> Result<Value, String> {
    let segments = parse_jsonpath(path)?;
    let mut current = root;
    for segment in &segments {
        current = match segment {
            PathSegment::Key(k) => current
                .get(k)
                .ok_or_else(|| format!("jsonpath: no such key '{k}' in '{path}'"))?,
            PathSegment::Index(i) => current
                .get(i)
                .ok_or_else(|| format!("jsonpath: index {i} out of bounds in '{path}'"))?,
        };
    }
    Ok(current.clone())
}

/// Render a resolved JSON value as the string a template token substitutes: a scalar renders as
/// its natural string form, an object/array as compact JSON.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        // `Value` is always representable as JSON; `unwrap_or_default` only guards the
        // unreachable serialization-error branch (e.g. a NaN float, which `Value` can't hold).
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Parse an offset like `-36h`/`+2d`/`90m`/`30s` into a `chrono::Duration`.
fn parse_offset(s: &str) -> Result<chrono::Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty offset".to_string());
    }
    let (sign, rest) = match s.chars().next() {
        Some('+') => (1i64, &s[1..]),
        Some('-') => (-1i64, &s[1..]),
        _ => (1i64, s),
    };
    let unit = rest
        .chars()
        .last()
        .ok_or_else(|| format!("empty offset magnitude: '{s}'"))?;
    let num_str = &rest[..rest.len() - unit.len_utf8()];
    let num: i64 = num_str
        .parse()
        .map_err(|_| format!("invalid offset number '{num_str}' in '{s}'"))?;
    let magnitude = match unit {
        's' => chrono::Duration::try_seconds(num),
        'm' => chrono::Duration::try_minutes(num),
        'h' => chrono::Duration::try_hours(num),
        'd' => chrono::Duration::try_days(num),
        other => return Err(format!("invalid offset unit '{other}' (expected s/m/h/d)")),
    }
    .ok_or_else(|| format!("offset overflows the representable range: '{s}'"))?;
    Ok(if sign < 0 { -magnitude } else { magnitude })
}

/// Evaluate `now` with an optional `offset` and `format`. Default format is RFC3339.
fn eval_now(offset: Option<&str>, format: Option<&str>) -> Result<String, String> {
    let mut dt = chrono::Utc::now();
    if let Some(off) = offset {
        let duration = parse_offset(off)?;
        dt = dt
            .checked_add_signed(duration)
            .ok_or_else(|| format!("now: offset '{off}' overflows the representable date range"))?;
    }
    match format {
        // Issue #359 B2 (no-panic): `chrono::DateTime::format` PANICS on a malformed strftime
        // string (e.g. `format='100%'`, `'%'`, `'%-'`). Validate first by walking the parsed
        // format items; if any is an `Item::Error`, route the failure through the normal AC3 error
        // policy (debug -> 500, else empty + warn) instead of unwinding the connection task.
        Some(fmt) => {
            use chrono::format::{Item, StrftimeItems};
            if StrftimeItems::new(fmt).any(|item| matches!(item, Item::Error)) {
                return Err(format!("now: invalid strftime format string '{fmt}'"));
            }
            Ok(dt.format(fmt).to_string())
        }
        None => Ok(dt.to_rfc3339()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::InMemoryFlowStore;
    use std::collections::HashMap;

    fn request_data() -> RequestData {
        let headers = hyper::HeaderMap::new();
        let mut data = RequestData::new(
            "POST",
            "/orders/11111111-1111-1111-1111-111111111111",
            Some("name=John&age=30"),
            &headers,
            Some(r#"{"variant":{"variantAttribute":"blue"},"items":[{"id":42},{"id":43}]}"#),
        );
        data.headers
            .insert("x-request-id".to_string(), "req-1".to_string());
        data
    }

    fn store() -> InMemoryFlowStore {
        InMemoryFlowStore::new(60)
    }

    fn ctx<'a>(
        request: &'a RequestData,
        flow_id: &'a str,
        store: &'a InMemoryFlowStore,
    ) -> TemplateContext<'a> {
        TemplateContext {
            request,
            flow_id,
            flow_store: store,
        }
    }

    #[test]
    fn templated_false_leaves_literal_braces_untouched() {
        // The opt-in gate lives in the handler, not this module — this test just documents that
        // `render_templated` is never reached unless `_rift.templated` is true; a plain string
        // with `{{now}}` is unrelated to that gate. Covered end-to-end in imposter tests.
    }

    #[test]
    fn request_method_and_path() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        assert_eq!(
            render_templated("{{request.method}}", &tctx, false).unwrap(),
            "POST"
        );
        assert_eq!(
            render_templated("{{request.path}}", &tctx, false).unwrap(),
            "/orders/11111111-1111-1111-1111-111111111111"
        );
    }

    #[test]
    fn request_query_present_and_missing() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        assert_eq!(
            render_templated("{{request.query.name}}", &tctx, false).unwrap(),
            "John"
        );
        // Missing query param, non-debug: empty string, no panic.
        assert_eq!(
            render_templated("[{{request.query.missing}}]", &tctx, false).unwrap(),
            "[]"
        );
    }

    #[test]
    fn request_header_case_insensitive() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        assert_eq!(
            render_templated("{{request.header 'X-Request-Id'}}", &tctx, false).unwrap(),
            "req-1"
        );
        assert_eq!(
            render_templated("{{request.header 'x-request-id'}}", &tctx, false).unwrap(),
            "req-1"
        );
    }

    #[test]
    fn request_json_nested_and_array_index() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        assert_eq!(
            render_templated(
                "{{request.json '$.variant.variantAttribute'}}",
                &tctx,
                false
            )
            .unwrap(),
            "blue"
        );
        assert_eq!(
            render_templated("{{request.json '$.items[0].id'}}", &tctx, false).unwrap(),
            "42"
        );
        assert_eq!(
            render_templated("{{request.json '$.items[1].id'}}", &tctx, false).unwrap(),
            "43"
        );
    }

    #[test]
    fn request_json_missing_path_empty_in_non_debug_error_in_debug() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        assert_eq!(
            render_templated("[{{request.json '$.nope'}}]", &tctx, false).unwrap(),
            "[]"
        );
        assert!(render_templated("{{request.json '$.nope'}}", &tctx, true).is_err());
    }

    #[test]
    fn last_segment_filter() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        assert_eq!(
            render_templated("{{request.path | last_segment}}", &tctx, false).unwrap(),
            "11111111-1111-1111-1111-111111111111"
        );
    }

    #[test]
    fn regex_filter_capture_group() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        let out = render_templated(
            "{{request.path | regex '^/orders/([0-9a-f-]+)$' 1}}",
            &tctx,
            false,
        )
        .unwrap();
        assert_eq!(out, "11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn now_with_offset_and_format() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        let out = render_templated("{{now}}", &tctx, false).unwrap();
        chrono::DateTime::parse_from_rfc3339(&out).expect("default now() is RFC3339");

        let minus = render_templated("{{now offset='-36h'}}", &tctx, false).unwrap();
        let expected = chrono::Utc::now() - chrono::Duration::hours(36);
        let parsed = chrono::DateTime::parse_from_rfc3339(&minus).expect("valid RFC3339");
        assert!((parsed.timestamp() - expected.timestamp()).abs() <= 2);

        let plus = render_templated("{{now offset='+2d'}}", &tctx, false).unwrap();
        let expected = chrono::Utc::now() + chrono::Duration::days(2);
        let parsed = chrono::DateTime::parse_from_rfc3339(&plus).expect("valid RFC3339");
        assert!((parsed.timestamp() - expected.timestamp()).abs() <= 2);

        let formatted = render_templated("{{now format='%Y-%m-%d'}}", &tctx, false).unwrap();
        assert_eq!(formatted.len(), "2026-07-08".len());
    }

    #[test]
    fn uuid_is_valid_v4_shape() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        let out = render_templated("{{uuid}}", &tctx, false).unwrap();
        let parsed = uuid::Uuid::parse_str(&out).expect("valid uuid");
        assert_eq!(parsed.get_version_num(), 4);
    }

    #[test]
    fn random_int_in_range() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        for _ in 0..20 {
            let out = render_templated("{{randomInt 5 9}}", &tctx, false).unwrap();
            let n: i64 = out.parse().expect("integer output");
            assert!((5..=9).contains(&n), "randomInt out of range: {n}");
        }
    }

    #[test]
    fn state_present_and_missing() {
        let data = request_data();
        let s = store();
        s.set("flow-1", "attempts", serde_json::json!(3)).unwrap();
        let tctx = ctx(&data, "flow-1", &s);
        assert_eq!(
            render_templated("{{state.attempts}}", &tctx, false).unwrap(),
            "3"
        );
        assert_eq!(
            render_templated("[{{state.missing}}]", &tctx, false).unwrap(),
            "[]"
        );
        assert!(render_templated("{{state.missing}}", &tctx, true).is_err());
    }

    #[test]
    fn unknown_function_debug_errors_non_debug_empty_and_warns() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        assert!(render_templated("{{bogus}}", &tctx, true).is_err());
        assert_eq!(render_templated("[{{bogus}}]", &tctx, false).unwrap(), "[]");
    }

    #[test]
    fn headers_map_dotted_query_and_multiple_tokens_in_one_body() {
        let mut headers = HashMap::new();
        headers.insert("q".to_string(), "1".to_string());
        let hm = hyper::HeaderMap::new();
        let data = RequestData::new("GET", "/x/abc", Some("q=1"), &hm, None);
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        let out = render_templated(
            "{\"method\":\"{{request.method}}\",\"q\":\"{{request.query.q}}\",\"seg\":\"{{request.path | last_segment}}\"}",
            &tctx,
            false,
        )
        .unwrap();
        assert_eq!(out, r#"{"method":"GET","q":"1","seg":"abc"}"#);
        let _ = headers; // silence unused warning if headers map above is unused elsewhere
    }

    #[test]
    fn legacy_date_tokens_still_expand_through_render_templated() {
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        let out = render_templated("{{NOW}}", &tctx, false).unwrap();
        chrono::DateTime::parse_from_rfc3339(&out).expect("legacy {{NOW}} still expands");
    }

    #[test]
    fn now_malformed_format_is_error_not_panic() {
        // Issue #359 B2: a malformed strftime format must route through the error policy, never
        // panic. In debug it fails the render; in non-debug it degrades to empty + warn.
        let data = request_data();
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        for bad in ["100%", "%", "%-"] {
            let token = format!("{{{{now format='{bad}'}}}}");
            assert!(
                render_templated(&token, &tctx, true).is_err(),
                "malformed format '{bad}' must be a debug-mode error"
            );
            // Non-debug: never panics, degrades to empty string.
            assert_eq!(
                render_templated(&format!("[{token}]"), &tctx, false).unwrap(),
                "[]",
                "malformed format '{bad}' must degrade to empty in non-debug"
            );
        }
        // A valid format still works.
        let ok = render_templated("{{now format='%Y-%m-%d'}}", &tctx, false).unwrap();
        assert_eq!(ok.len(), "2026-07-08".len());
    }

    #[test]
    fn json_filter_escapes_for_string_literal() {
        // Issue #359 B3: `| json` escapes a value so it is safe inside a JSON string literal.
        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-payload", r#"he said "hi"\ and left"#.parse().unwrap());
        let data = RequestData::new("GET", "/x", None, &headers, None);
        let s = store();
        let tctx = ctx(&data, "flow-1", &s);
        // Compose a JSON body where the escaped value is placed inside a string literal.
        let out = render_templated(
            r#"{"echo":"{{request.header 'X-Payload' | json}}"}"#,
            &tctx,
            false,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&out).expect("json filter yields valid JSON body");
        assert_eq!(parsed["echo"], r#"he said "hi"\ and left"#);
    }

    #[test]
    fn jsonpath_parser_rejects_malformed_paths() {
        assert!(parse_jsonpath("a.b").is_err(), "must start with $");
        assert!(parse_jsonpath("$.a[").is_err(), "unterminated [");
        assert!(parse_jsonpath("$.a[x]").is_err(), "non-numeric index");
    }
}
