//! Response building and execution logic for imposters.
//!
//! This module handles creating responses from stubs, applying behaviors,
//! and managing the response cycle.

use super::conditional::{self, ConditionalSpec};
use super::types::{
    DebugResponsePreview, IsResponse, ResponseMode, RiftResponseExtension, RiftScriptConfig,
    StubResponse,
};
use crate::behaviors::{
    DecorateError, Decorated, HasRepeatBehavior, RequestContext, ResponseStatus, apply_decorate,
    is_javascript_decorate,
};
// Fallback-only (issue #357 Item 6): a JavaScript decorate runs in Boa
// (`execute_mountebank_decorate`); these textual rewrites are used only when the `javascript`
// feature is disabled and no JS engine is available.
#[cfg(feature = "javascript")]
use crate::behaviors::is_js_function_decorate;
#[cfg(not(feature = "javascript"))]
use crate::behaviors::{is_js_config_decorate, rewrite_js_config_to_rhai};
use crate::imposter::Predicate;
use std::sync::LazyLock;

/// Truncate a string with ellipsis if it exceeds the maximum byte length.
///
/// This function is unicode-safe and will not panic on multi-byte characters.
/// It finds the nearest valid UTF-8 character boundary at or before `max_len`.
pub(crate) fn truncate_with_ellipsis(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        return text.to_string();
    }

    let end = text.floor_char_boundary(max_len);
    format!("{}...", &text[..end])
}

/// `repeat` applies to every response type, as in Mountebank (issue #1188). A top-level `repeat`
/// was merged into the compiled program at parse, as its `{"repeat": n}` element (issue #1198).
impl HasRepeatBehavior for StubResponse {
    fn get_repeat(&self) -> Option<u32> {
        // The program's first element only: a later `repeat` step is an inert custom key written
        // beside a behavior in an array element, which Mountebank never reads (issue #1672).
        self.behaviors_block()?
            .as_array()?
            .first()?
            .get("repeat")
            .and_then(serde_json::Value::as_u64)
            .and_then(|r| u32::try_from(r).ok())
    }
}

/// Create response preview from a StubResponse (for debug mode)
pub fn create_response_preview(response: &StubResponse) -> DebugResponsePreview {
    match response {
        StubResponse::Is { is, .. } => {
            let body_preview = is.body.as_ref().map(|b| match b {
                serde_json::Value::String(s) => truncate_with_ellipsis(s, 500),
                other => {
                    // Serializing a `serde_json::Value` is infallible by construction, so this
                    // never defaults; it is a debug preview either way (issue #611).
                    let json = serde_json::to_string(other).unwrap_or_default();
                    truncate_with_ellipsis(&json, 500)
                }
            });
            let headers = if is.headers.is_empty() {
                None
            } else {
                Some(
                    is.headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.join(", ")))
                        .collect(),
                )
            };
            DebugResponsePreview {
                response_type: "is".to_string(),
                // A string status (issue #1558) has no code until it is served.
                status_code: is.status_text.is_none().then_some(is.status_code),
                headers,
                body_preview,
            }
        }
        StubResponse::Proxy { proxy, .. } => DebugResponsePreview {
            response_type: "proxy".to_string(),
            status_code: None,
            headers: None,
            body_preview: Some(format!("Proxy to: {}", proxy.to)),
        },
        StubResponse::Inject { inject, .. } => DebugResponsePreview {
            response_type: "inject".to_string(),
            status_code: None,
            headers: None,
            body_preview: Some(format!(
                "JavaScript inject: {}",
                truncate_with_ellipsis(inject, 50)
            )),
        },
        StubResponse::Fault { fault, .. } => DebugResponsePreview {
            response_type: "fault".to_string(),
            status_code: None,
            headers: None,
            body_preview: Some(format!("Fault: {fault}")),
        },
        StubResponse::RiftScript { rift, .. } => {
            // RiftScript uses the _rift extension namespace
            let script_info = if rift.script.is_some() {
                "Rift script response"
            } else if rift.fault.is_some() {
                "Rift fault injection"
            } else {
                "Rift extension response"
            };
            DebugResponsePreview {
                response_type: "_rift".to_string(),
                status_code: None,
                headers: None,
                body_preview: Some(script_info.to_string()),
            }
        }
    }
}

/// Whether the config headers already declare a Content-Type in any casing.
///
/// Mountebank treats header names case-insensitively, so the `application/json` default must be
/// injected only when no Content-Type exists in *any* casing — a case-sensitive check emits a
/// duplicate for a header like `CONTENT-TYPE` (issue #723). Shared by the execute path and
/// `PreparedResponse::try_build` so the two serve paths cannot drift.
fn has_content_type(headers: &crate::imposter::Headers) -> bool {
    headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("content-type"))
}

/// Whether an `is` body earns the automatic `Content-Type: application/json`: a JSON object or
/// array. A number or boolean is served as its JSON text without one, as Mountebank serves it.
fn earns_json_content_type(body: Option<&serde_json::Value>) -> bool {
    matches!(
        body,
        Some(serde_json::Value::Object(_) | serde_json::Value::Array(_))
    )
}

/// A response body as the text served: a string as it is, any other JSON value serialized. The one
/// rendering for a stub's body and for a `defaultResponse` body (issue #1369), so the two cannot
/// drift.
pub(super) fn body_text(body: Option<&serde_json::Value>) -> String {
    body.map(|b| match b.as_str() {
        Some(text) => text.to_string(),
        // Serializing a `serde_json::Value` is infallible by construction (map keys are strings,
        // numbers are finite), so this never defaults (issue #611).
        None => serde_json::to_string(b).unwrap_or_default(),
    })
    .unwrap_or_default()
}

/// Complete an executed `is` response with the imposter's `defaultResponse` (issue #1369): the
/// fields `is` omits are replaced by the default's, before templating and behaviors run. A no-op
/// when `is` omits nothing.
pub(super) fn complete_from_default(
    is: &IsResponse,
    default: &IsResponse,
    status: &mut u16,
    headers: &mut crate::imposter::Headers,
    body: &mut String,
    mode: &mut ResponseMode,
) {
    let merged = is.completed_by(default);
    *status = merged.status_code;
    if is.omitted.headers {
        // Omitted headers are empty, so the only header here is the `Content-Type` that this
        // response's own non-string body earned; it is kept unless the default sets one.
        let earned = headers.shift_remove("Content-Type");
        *headers = merged.headers.clone();
        if let Some(values) = earned
            && !has_content_type(headers)
        {
            headers.insert("Content-Type".to_string(), values);
        }
    }
    if is.omitted.body {
        *body = body_text(merged.body.as_ref());
    }
    *mode = merged.mode.clone();
}

/// Execute a stub response with Rift extensions
/// Returns (status, headers, body, behaviors, rift_extension, response_mode, is_fault)
#[allow(clippy::type_complexity)]
pub fn execute_stub_response_with_rift(
    response: &StubResponse,
) -> Option<(
    u16,
    crate::imposter::Headers,
    String,
    Option<std::sync::Arc<crate::behaviors::BehaviorProgram>>,
    Option<&RiftResponseExtension>,
    ResponseMode,
    bool,
)> {
    match response {
        StubResponse::Is {
            is,
            behaviors_parsed,
            rift,
            rendered_body,
            ..
        } => {
            let mut headers = is.headers.clone();
            let mode = is.mode.clone();

            // Issue #479: `rendered_body` is precomputed once at construction (see
            // `StubResponse::new_is`) for a non-string body — no more re-serializing on every
            // request. A string body (or no body) has no `rendered_body` and is used as-is.
            let body = match rendered_body {
                Some(rb) => {
                    if earns_json_content_type(is.body.as_ref()) && !has_content_type(&headers) {
                        headers.insert(
                            "Content-Type".to_string(),
                            vec!["application/json".to_string()],
                        );
                    }
                    rb.to_string()
                }
                None => is
                    .body
                    .as_ref()
                    .map(|b| b.as_str().unwrap_or("").to_string())
                    .unwrap_or_default(),
            };

            Some((
                is.status_code,
                headers,
                body,
                behaviors_parsed.clone(),
                // Borrowed, not cloned (issue #561): the lifetime elides from `response`, and
                // every caller only reads this on the hot path — an owned copy is materialized
                // only where a caller genuinely needs one (see `execute_stub_with_rift`).
                rift.as_deref(),
                mode,
                false,
            ))
        }
        StubResponse::Fault { fault, .. } => Some((
            0,
            crate::imposter::Headers::new(),
            fault.clone(),
            None,
            None,
            ResponseMode::Text,
            true,
        )),
        StubResponse::Proxy { .. } => None,
        StubResponse::Inject { .. } => None,
        // Issue #1685: a `_rift`-only response without a script is served as `is: {}` carrying its
        // `_rift` effects (fault, conditional, stateOps, templated). Its behaviors other than
        // `repeat` stay ignored, as documented, so none are returned.
        StubResponse::RiftScript { rift, .. } if rift.script.is_none() => Some((
            EMPTY_IS.status_code,
            EMPTY_IS.headers.clone(),
            String::new(),
            None,
            Some(rift.as_ref()),
            EMPTY_IS.mode.clone(),
            false,
        )),
        StubResponse::RiftScript { .. } => None,
    }
}

/// The `is: {}` a `_rift`-only response without a script is served as (issue #1685).
static EMPTY_IS: LazyLock<IsResponse> = LazyLock::new(IsResponse::empty);

/// The `is` this response is served as: its own, or `is: {}` for a `_rift`-only response
/// without a script (#1685). `None` for proxy/inject/fault/scripted responses.
pub(crate) fn served_is(response: &StubResponse) -> Option<&IsResponse> {
    match response {
        StubResponse::Is { is, .. } => Some(is),
        StubResponse::RiftScript { rift, .. } if rift.script.is_none() => Some(&EMPTY_IS),
        StubResponse::Fault { .. }
        | StubResponse::Proxy { .. }
        | StubResponse::Inject { .. }
        | StubResponse::RiftScript { .. } => None,
    }
}

/// A fully static `is` response materialized once at stub construction (issue #703): the status,
/// the header names/values parsed & validated into an `http::HeaderMap`, and the body as `Bytes`.
///
/// Serving it is a status copy plus a refcounted `HeaderMap` clone (`HeaderValue` is `Bytes`-backed)
/// and a refcounted `Bytes` clone — zero per-request `String` allocation, header re-parsing, or
/// template scanning. It is built (`Some`) only when *nothing* about the response depends on the
/// request; anything request-dependent stores `None` and serves through the existing slow path
/// unchanged. This is a derived, serving-only artifact — the Mountebank-shaped
/// `crate::imposter::Headers` on `IsResponse` stays the source of truth for admin-API JSON.
#[derive(Debug)]
pub struct PreparedResponse {
    status: hyper::StatusCode,
    headers: hyper::HeaderMap,
    body: bytes::Bytes,
    /// `_rift.conditional` (issue #1280) on a 2xx. The body is fixed, so its ETag is computed here
    /// once; `Last-Modified: load` belongs to the stub state, so it is supplied at serve time.
    conditional: Option<PreparedConditional>,
}

#[derive(Debug)]
struct PreparedConditional {
    spec: ConditionalSpec,
    etag: Option<String>,
}

impl PreparedResponse {
    /// Build the prepared form for a static `is` response, or `None` when any request-dependent
    /// aspect (templates, date tokens, `_behaviors`, a `_rift` serving effect, binary mode) or an
    /// invalid header/status forces the slow path. Called once at construction (`new_is`) with the
    /// already-computed `rendered_body` (#479) so body serialization is not repeated here. `pub` so
    /// the serve-path bench (issue #703) can construct it.
    #[must_use]
    pub fn try_build(
        is: &IsResponse,
        rendered_body: Option<&std::sync::Arc<str>>,
        rift: Option<&RiftResponseExtension>,
        behaviors_present: bool,
    ) -> Option<PreparedResponse> {
        // Binary mode (base64 decode + its failure signaling) and every `_rift` serving effect and
        // `_behaviors` block stay on the slow path — the fast path serves only inert `is` bodies.
        // A string `statusCode` (issue #1558) is read at serve time, and may answer 500.
        if is.mode != ResponseMode::Text || behaviors_present || is.status_text.is_some() {
            return None;
        }
        if let Some(r) = rift
            && (r.fault.is_some() || r.templated || r.script.is_some() || !r.state_ops.is_empty())
        {
            return None;
        }

        // The served body, resolved exactly as `execute_stub_response_with_rift` does: the
        // pre-rendered JSON for a non-string body, else the string body (or empty).
        let body_string: String = match rendered_body {
            Some(rb) => rb.to_string(),
            None => is
                .body
                .as_ref()
                .and_then(|b| b.as_str())
                .unwrap_or_default()
                .to_string(),
        };

        // Any `${request.*}` reflection (body or a header value) or serve-time date token means the
        // output depends on the request/clock — slow path.
        if crate::extensions::template::has_template_variables(&body_string)
            || is
                .headers
                .values()
                .flatten()
                .any(|v| crate::extensions::template::has_template_variables(v))
            || crate::extensions::contains_date_templates(&body_string)
        {
            return None;
        }

        // The Content-Type default must match the execute path exactly for byte-identity, so both
        // call the shared `has_content_type` helper (issue #723 — the invariant is now "shared
        // helper", not a textually identical check). Computed on the raw config `is.headers` before
        // the `HeaderMap` normalizes casing.
        let has_content_type = has_content_type(&is.headers);

        // Parse & validate every header once; an invalid name/value stores no prepared form so the
        // error surfaces at serve time exactly as today (issue #703: no config-validation change).
        let mut headers = hyper::HeaderMap::new();
        for (name, values) in &is.headers {
            let header_name = hyper::header::HeaderName::from_bytes(name.as_bytes()).ok()?;
            for value in values {
                let header_value = hyper::header::HeaderValue::from_str(value).ok()?;
                headers.append(header_name.clone(), header_value);
            }
        }
        if earns_json_content_type(is.body.as_ref()) && !has_content_type {
            headers.append(
                hyper::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_static("application/json"),
            );
        }
        // The imposter marker the slow path always appends (handler.rs).
        headers.append(
            hyper::header::HeaderName::from_static("x-rift-imposter"),
            hyper::header::HeaderValue::from_static("true"),
        );

        let status = hyper::StatusCode::from_u16(is.status_code).ok()?;
        let conditional = ConditionalSpec::from_config(rift.and_then(|r| r.conditional.as_ref()))
            .filter(|_| status.is_success())
            .map(|spec| PreparedConditional {
                etag: spec
                    .wants_etag()
                    .then(|| conditional::etag_for(body_string.as_bytes())),
                spec,
            });
        Some(PreparedResponse {
            status,
            headers,
            body: bytes::Bytes::from(body_string),
            conditional,
        })
    }

    /// [`serve`](Self::serve) for a request: a conditional response (issue #1280) answers `304`
    /// when the request's preconditions match, and otherwise carries its validators. Errs only on a
    /// fixed `Last-Modified` that is not a valid header value.
    pub(crate) fn serve_request(
        &self,
        method: &str,
        request_headers: &crate::util::OrderedMap<String, Vec<String>>,
        // Only read for a conditional response (issue #1307 makes it more than a field read).
        loaded_at: impl FnOnce() -> chrono::DateTime<chrono::Utc>,
    ) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, hyper::http::Error> {
        let Some(prepared) = self
            .conditional
            .as_ref()
            .filter(|_| conditional::method_applies(method))
        else {
            return Ok(self.serve());
        };
        let validators = prepared.spec.validators(prepared.etag.clone(), loaded_at());
        if validators.not_modified(request_headers) {
            return validators.not_modified_response(
                self.headers
                    .iter()
                    .filter(|(name, _)| conditional::kept_on_304(name.as_str())),
            );
        }
        let mut response = self.serve();
        let headers = response.headers_mut();
        headers.remove(hyper::header::ETAG);
        headers.remove(hyper::header::LAST_MODIFIED);
        for (name, value) in validators.headers() {
            headers.insert(name, hyper::header::HeaderValue::from_str(value)?);
        }
        Ok(response)
    }

    /// Serve the prepared response: a status copy plus refcounted `HeaderMap`/`Bytes` clones, with
    /// no builder round-trip (no per-request header string re-parsing).
    pub fn serve(&self) -> hyper::Response<http_body_util::Full<bytes::Bytes>> {
        let (mut parts, ()) = hyper::Response::new(()).into_parts();
        parts.status = self.status;
        parts.headers = self.headers.clone();
        hyper::Response::from_parts(parts, http_body_util::Full::new(self.body.clone()))
    }
}

/// Get RiftScript config if the response is a RiftScript type
pub fn get_rift_script_config(response: &StubResponse) -> Option<RiftScriptConfig> {
    match response {
        StubResponse::RiftScript { rift, .. } => rift.script.clone(),
        _ => None,
    }
}

/// Create a stub from a recorded proxy response.
///
/// If the body is valid UTF-8, it is stored as text (JSON or string).
/// If the body is not valid UTF-8 (binary content), it is base64-encoded
/// and the stub uses `_mode: "binary"` so it can be replayed correctly.
///
/// Headers are accepted as `&[(String, String)]` to preserve multi-valued
/// headers (e.g., multiple `Set-Cookie`), which are stored as separate values in
/// the stub's `IsResponse` (multi-value headers, issue #238). Hop-by-hop headers
/// are dropped.
#[allow(clippy::too_many_arguments)]
pub fn create_stub_from_proxy_response(
    predicates: Vec<Predicate>,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    latency_ms: Option<u64>,
    decorate_fn: Option<String>,
    recorded_from: Option<String>,
    response_time_ms: u64,
) -> super::types::Stub {
    // Group values per key so multiple values for one header (e.g. Set-Cookie) survive replay.
    //
    // Names are recorded Title-Cased (issue #1366). hyper only ever yields lower-case names, so the
    // upstream's wire spelling is lost; Title-Case is what Mountebank records from a typical HTTP/1
    // upstream. A name holding an upper-case letter can only come from a proxy behavior and is
    // kept as written. Spellings that differ only in case share one entry, values in input order.
    let response_headers: crate::imposter::Headers = {
        let mut map: crate::imposter::Headers = crate::imposter::Headers::new();
        for (k, v) in headers {
            if crate::util::is_hop_by_hop_header(k) {
                continue;
            }
            let existing = map
                .keys()
                .find(|name| name.eq_ignore_ascii_case(k))
                .cloned();
            let name = existing.unwrap_or_else(|| {
                if k.bytes().any(|b| b.is_ascii_uppercase()) {
                    k.clone()
                } else {
                    crate::behaviors::header_to_title_case(k)
                }
            });
            map.entry(name).or_default().push(v.clone());
        }
        map
    };

    let (body_value, is_binary) = crate::util::encode_body_for_stub(body);
    // An empty upstream body is a body that was written (`"body": ""`, as Mountebank records it),
    // not an omitted one: `None` would let the imposter's `defaultResponse` fill it on replay.
    let body_value = body_value.or_else(|| Some(serde_json::Value::String(String::new())));
    let mode = if is_binary {
        ResponseMode::Binary
    } else {
        ResponseMode::Text
    };

    let mut is_response = IsResponse::new(status, response_headers, body_value, mode);
    // The mark the clear reads (issue #1519): it travels with the response into a merged stub.
    is_response.proxy_response_time = Some(serde_json::Value::from(response_time_ms));

    // Build behaviors object if needed
    let behaviors = if latency_ms.is_some() || decorate_fn.is_some() {
        let mut behaviors_obj = serde_json::Map::new();
        if let Some(ms) = latency_ms {
            behaviors_obj.insert("wait".to_string(), serde_json::json!(ms));
        }
        if let Some(fn_str) = decorate_fn {
            behaviors_obj.insert("decorate".to_string(), serde_json::json!(fn_str));
        }
        Some(serde_json::Value::Object(behaviors_obj))
    } else {
        None
    };

    super::types::Stub {
        name: None,
        id: None,
        route_pattern: None,
        predicates,
        responses: vec![StubResponse::new_is(is_response, behaviors, None)],
        scenario_name: None,
        required_scenario_state: None,
        new_scenario_state: None,
        space: None,
        recorded_from,
        verify: None,
    }
}

/// Apply decorate behavior - handles both JavaScript and Rhai scripts.
///
/// A JavaScript decorate ([`is_javascript_decorate`]) runs on Mountebank's contract (issue
/// #1567): called as `fn(config, response, logger)` whatever its form or parameter names, a
/// returned object replaces the response, and the headers are exactly the script's. Anything else
/// runs as Rhai.
///
/// `scope` identifies the per-imposter state shared with predicate injects and response
/// injects for the same imposter (issue #355 Item 0) — decorate reads and writes the same
/// persisted state rather than a throwaway object. `stub_id` tags the script logger's tracing
/// events with the owning stub, when known (issue #355 AC1). `files` is what the imposter's
/// scripts may `require()` (issue #1611). It runs within the default script timeout; the
/// serving path, [`apply_decorate_bounded`], passes the imposter's.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn apply_js_or_rhai_decorate(
    script: &str,
    request: &RequestContext,
    body: &str,
    status: &ResponseStatus,
    headers: &crate::util::OrderedMap<String, String>,
    scope: &crate::scripting::ScriptScope,
    stub_id: Option<&str>,
    files: &std::sync::Arc<crate::imposter::ScriptFileAccess>,
) -> Result<Decorated, DecorateError> {
    apply_js_or_rhai_decorate_within(
        script,
        request,
        body,
        status,
        headers,
        scope,
        stub_id,
        files,
        std::time::Duration::from_millis(crate::scripting::DEFAULT_SCRIPT_TIMEOUT_MS),
    )
}

/// [`apply_js_or_rhai_decorate`] with the imposter's script timeout, which bounds how long a
/// JavaScript `async` decorate's jobs and timers may run (issue #1593).
#[allow(clippy::too_many_arguments)]
fn apply_js_or_rhai_decorate_within(
    script: &str,
    request: &RequestContext,
    body: &str,
    status: &ResponseStatus,
    headers: &crate::util::OrderedMap<String, String>,
    scope: &crate::scripting::ScriptScope,
    stub_id: Option<&str>,
    files: &std::sync::Arc<crate::imposter::ScriptFileAccess>,
    timeout: std::time::Duration,
) -> Result<Decorated, DecorateError> {
    // Only the JS engine call sites below consume `scope`/`stub_id`/`files`/`timeout`;
    // without the feature the parameters would otherwise be unused.
    #[cfg(not(feature = "javascript"))]
    let _ = (scope, stub_id, files, timeout);

    if !is_javascript_decorate(script) {
        return decorate_rhai(script, request, body, status, headers);
    }

    #[cfg(feature = "javascript")]
    {
        let mb_request = crate::scripting::MountebankRequest {
            method: request.method.clone(),
            path: request.path.clone(),
            query: request.query.clone(),
            headers: request.headers.clone(),
            body: request.body.clone(),
            body_undefined: request.body_undefined,
            // `RequestContext` doesn't track binary/text, so the classification is genuinely
            // unknown here — say so rather than asserting `Text` over a possibly-base64 body
            // (issue #636). `isBinary` is then absent for decorate scripts, not falsely false.
            mode: None,
            request_from: request.request_from.clone(),
            ip: request.ip.clone(),
            form: request.form.clone(),
            is_dry_run: false,
        };
        // The executor calls the script as a function value, so a bare body in the `config`
        // convention ("config.response.body = ...;") is wrapped into one first. The test is the
        // start-anchored function recogniser, NOT a substring `=>` test (B4, issue #357): a bare
        // body can contain an inner arrow, e.g. `items.map(x => x.id)`, and leaving it unwrapped
        // would produce `var __fn = <statement>` (a throw).
        let wrapped_script = if is_js_function_decorate(script) {
            std::borrow::Cow::Borrowed(script)
        } else {
            std::borrow::Cow::Owned(format!("function(config) {{ {script} }}"))
        };
        crate::scripting::execute_mountebank_decorate_within(
            &wrapped_script,
            &mb_request,
            body,
            status,
            headers,
            scope.clone(),
            stub_id,
            files,
            timeout,
        )
        .map_err(
            |e| match e.downcast_ref::<crate::scripting::ScriptTimeoutError>() {
                // Its jobs could not finish by the deadline: the same answer as the caller's timeout.
                Some(t) => DecorateError::Timeout(t.timeout_ms),
                None => DecorateError::JavaScript(e.to_string()),
            },
        )
    }

    // No JS engine compiled in (`--no-default-features`): fall back to textual rewrites so
    // simple field-access decorates still work, at the cost of real JS semantics (require(),
    // JSON.parse/stringify on nested values, closures, a returned object).
    #[cfg(not(feature = "javascript"))]
    {
        if is_js_config_decorate(script) {
            return decorate_rhai(
                &rewrite_js_config_to_rhai(script),
                request,
                body,
                status,
                headers,
            );
        }
        if script.trim().starts_with("function")
            && let Some(start) = script.find('{')
            && let Some(end) = script.rfind('}')
            && start < end
        {
            let rhai_script = script[start + 1..end].trim().replace('\'', "\"");
            return decorate_rhai(&rhai_script, request, body, status, headers);
        }
        // Any other JavaScript (an arrow not named `config`, an `async` function) has no textual
        // fallback; it must not reach Rhai as if it were Rhai (issue #1567).
        Err(DecorateError::JsParseFailure)
    }
}

/// Run a Rhai decorate over a copy of `headers`; the script changes the response it is handed.
fn decorate_rhai(
    script: &str,
    request: &RequestContext,
    body: &str,
    status: &ResponseStatus,
    headers: &crate::util::OrderedMap<String, String>,
) -> Result<Decorated, DecorateError> {
    let mut headers = headers.clone();
    let (body, status_code) = apply_decorate(script, request, body, status, &mut headers)?;
    Ok(Decorated {
        status_code,
        headers,
        body,
        returned: None,
    })
}

/// Run [`apply_js_or_rhai_decorate`] off the async worker with a wall-clock deadline
/// (issue #476) — the same `spawn_blocking` + `tokio::time::timeout` shape as
/// `scripting::bounded`. Takes the header map by value because the execution moves
/// to a blocking thread. No abort flag: Boa has no per-instruction interrupt, so after a timeout
/// the loop-iteration cap (issue #327) is what eventually frees the blocking thread; the client
/// is released at the deadline either way. The deadline is the imposter's
/// `resolve_script_timeout_ms` budget, shared with `_rift.script`.
#[allow(clippy::too_many_arguments)]
pub async fn apply_decorate_bounded(
    script: String,
    request: RequestContext,
    body: String,
    status: ResponseStatus,
    headers: crate::util::OrderedMap<String, String>,
    scope: crate::scripting::ScriptScope,
    stub_id: Option<String>,
    files: std::sync::Arc<crate::imposter::ScriptFileAccess>,
    timeout: std::time::Duration,
) -> Result<Decorated, DecorateError> {
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    // Deliberately a plain `spawn_blocking`, not `spawn_blocking_annotated` (issue #987): the
    // decorate script has no `ctx.state`, so nothing on this thread can annotate. What makes that
    // airtight is *which* code reads the store, not the thread's history: the `ctx.state.*` natives
    // are registered only by `create_ctx_object`, which the decorate path never calls. Rhai
    // decorate uses a plain `rift_rhai_engine()`, and MB-JS decorate binds only `config`/`logger`.
    // (Do NOT reason from "the thread-local is always cleared" — it is not: `execute_js_script`
    // has no RAII guard, so a panic skips its `clear_current_flow_store`, and `declared_functions_js`
    // never clears at all. A stale `Arc<dyn FlowStore>` can outlive its job on a pool thread.)
    let handle = tokio::task::spawn_blocking(move || {
        apply_js_or_rhai_decorate_within(
            &script,
            &request,
            &body,
            &status,
            &headers,
            &scope,
            stub_id.as_deref(),
            &files,
            timeout,
        )
    });
    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_err)) => Err(DecorateError::JavaScript(format!(
            "decorate task panicked: {join_err}"
        ))),
        Err(_elapsed) => {
            tracing::warn!("decorate script timed out after {timeout_ms}ms");
            Err(DecorateError::Timeout(timeout_ms))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scripting::ScriptScope;

    // A fresh port per test (disjoint from `scripting/js_engine.rs`'s test range in this binary),
    // so parallel tests' logger lines stay apart; each test's state is its own `ScriptScope`.
    /// The unconfined `require` of a local door (issue #1611): these tests write their modules to
    /// the temp dir.
    fn trusted_files() -> std::sync::Arc<crate::imposter::ScriptFileAccess> {
        std::sync::Arc::new(crate::imposter::ScriptFileAccess::trusted_cwd())
    }

    fn test_port() -> u16 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(41_100);
        NEXT.fetch_add(1, Ordering::Relaxed) as u16
    }

    // Issue #191: the JS `config =>` decorate convention runs (rewritten to Rhai) end-to-end.
    fn decorate_req() -> RequestContext {
        RequestContext {
            method: "GET".to_string(),
            path: "/orders".to_string(),
            query: Default::default(),
            headers: crate::util::OrderedMap::default(),
            body: Some("REQ-BODY".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn decorate_js_config_sets_body() {
        let headers = crate::util::OrderedMap::default();
        let Decorated {
            body,
            status_code: status,
            ..
        } = apply_js_or_rhai_decorate(
            "config => { config.response.body = 'hello'; }",
            &decorate_req(),
            "original",
            &ResponseStatus::Code(200),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .unwrap();
        assert_eq!(body, "hello");
        assert_eq!(status, ResponseStatus::Code(200));
    }

    #[test]
    fn decorate_js_config_reads_request_body() {
        let headers = crate::util::OrderedMap::default();
        let Decorated { body, .. } = apply_js_or_rhai_decorate(
            "config => { config.response.body = config.request.body; }",
            &decorate_req(),
            "original",
            &ResponseStatus::Code(200),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .unwrap();
        assert_eq!(body, "REQ-BODY");
    }

    // Issue #305: a `config =>` decorate that require()s an external module must route to the JS
    // engine (not the lossy Rhai rewrite) and actually run the module.
    #[cfg(feature = "javascript")]
    #[test]
    fn decorate_js_config_require_routes_to_boa() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let module =
            std::env::temp_dir().join(format!("rift_305_route_{}_{n}.cjs", std::process::id()));
        std::fs::write(
            &module,
            "module.exports = function (config) { config.response.body = 'REQUIRE-RAN'; };\n",
        )
        .unwrap();
        let script = format!(
            "config => {{ const s = require('{}'); s(config); }}",
            module.display()
        );
        let headers = crate::util::OrderedMap::default();
        let result = apply_js_or_rhai_decorate(
            &script,
            &decorate_req(),
            "original",
            &ResponseStatus::Code(200),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        );
        let _ = std::fs::remove_file(&module);
        let Decorated { body, .. } = result.expect("require-based config decorate should run");
        assert_eq!(body, "REQUIRE-RAN");
    }

    // Issue #357 Item 6: every `config =>` decorate now runs through real Boa execution, not the
    // lossy textual JS→Rhai transpiler. Prove it with something the transpiler's regex-based
    // `JSON.parse`/`JSON.stringify` handling could never do: parse a NESTED request body and
    // mutate the resulting object before re-serializing it — `rewrite_js_config_to_rhai`'s
    // `JSON_FN_RE` only handles a single, non-nested argument (see its doc comment), so this
    // would previously have been left un-rewritten (`JSON.parse({"nested":...})`, a syntax error).
    #[cfg(feature = "javascript")]
    #[test]
    fn decorate_js_config_real_json_parse_and_mutation_runs_through_boa() {
        let headers = crate::util::OrderedMap::default();
        let request = RequestContext {
            method: "POST".to_string(),
            path: "/orders".to_string(),
            query: Default::default(),
            headers: crate::util::OrderedMap::default(),
            body: Some(r#"{"order": {"id": 42, "items": ["a", "b"]}}"#.to_string()),
            ..Default::default()
        };
        let script = r#"config => {
            var parsed = JSON.parse(config.request.body);
            parsed.order.itemCount = parsed.order.items.length;
            delete parsed.order.items;
            config.response.body = JSON.stringify(parsed);
        }"#;
        let Decorated { body, .. } = apply_js_or_rhai_decorate(
            script,
            &request,
            "original",
            &ResponseStatus::Code(200),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("real JS JSON.parse/stringify decorate must execute via Boa");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["order"]["id"], 42);
        assert_eq!(parsed["order"]["itemCount"], 2);
        assert!(parsed["order"].get("items").is_none());
    }

    // B4 (issue #357): a BARE-BODY decorate (no `function`/arrow wrapper) that legitimately
    // contains an INNER arrow lambda must still be wrapped and run — the old `contains("=>")`
    // detection wrongly classified it as an already-wrapped function value and left it unwrapped,
    // producing `var __fn = <statement>` (a throw).
    #[cfg(feature = "javascript")]
    #[test]
    fn decorate_bare_body_with_inner_arrow_runs_through_boa() {
        let headers = crate::util::OrderedMap::default();
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/orders".to_string(),
            query: Default::default(),
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        };
        // Bare body (starts with `config.response.`, detected via that prefix), containing an
        // inner `.map(x => x.id)` arrow.
        let script =
            r#"config.response.body = JSON.stringify([{id:1},{id:2},{id:3}].map(x => x.id));"#;
        let Decorated { body, .. } = apply_js_or_rhai_decorate(
            script,
            &request,
            "original",
            &ResponseStatus::Code(200),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("bare-body decorate with an inner arrow must execute via Boa");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn decorate_js_config_sets_status() {
        let headers = crate::util::OrderedMap::default();
        let Decorated {
            status_code: status,
            ..
        } = apply_js_or_rhai_decorate(
            "config => { config.response.statusCode = 404; }",
            &decorate_req(),
            "original",
            &ResponseStatus::Code(200),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .unwrap();
        assert_eq!(status, ResponseStatus::Code(404));
    }

    #[test]
    fn decorate_js_config_function_wrapper_executes() {
        // `function(config) { ... }` must take the config-rewrite route, NOT the `function`→JS-engine
        // route (it uses the config model, not (request, response)). Locks the detection ordering.
        let headers = crate::util::OrderedMap::default();
        let Decorated { body, .. } = apply_js_or_rhai_decorate(
            "function(config) { config.response.body = 'fn'; }",
            &decorate_req(),
            "original",
            &ResponseStatus::Code(200),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .unwrap();
        assert_eq!(body, "fn");
    }

    #[test]
    fn decorate_js_config_no_wrapper_executes() {
        // A bare body (no arrow/function wrapper) is detected via `config.response.` and rewritten
        // in place (no brace stripping).
        let headers = crate::util::OrderedMap::default();
        let Decorated { body, .. } = apply_js_or_rhai_decorate(
            "config.response.body = 'bare';",
            &decorate_req(),
            "original",
            &ResponseStatus::Code(200),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .unwrap();
        assert_eq!(body, "bare");
    }

    #[test]
    fn decorate_plain_rhai_still_works() {
        let headers = crate::util::OrderedMap::default();
        let Decorated { body, .. } = apply_js_or_rhai_decorate(
            "response.body = request.path;",
            &decorate_req(),
            "original",
            &ResponseStatus::Code(200),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .unwrap();
        assert_eq!(body, "/orders", "existing Rhai decorate must be unchanged");
    }

    // Issue #1567: Mountebank's decorate contract — any function form, the return value, and
    // header deletion. The response handed over is `statusCode` with `X-Orig: 1` and `orig`.
    #[cfg(feature = "javascript")]
    fn decorate_1567(
        script: &str,
        status: u16,
    ) -> (
        String,
        ResponseStatus,
        crate::util::OrderedMap<String, String>,
    ) {
        let headers = crate::util::OrderedMap::from_iter([("X-Orig".to_string(), "1".to_string())]);
        let Decorated {
            body,
            status_code: status,
            headers,
            ..
        } = apply_js_or_rhai_decorate(
            script,
            &decorate_req(),
            "orig",
            &ResponseStatus::Code(status),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .unwrap_or_else(|e| panic!("{script}: {e}"));
        (body, status, headers)
    }

    // A decorate body `JSON.stringify` cannot serialize fails the decorate, as Mountebank's
    // `postProcess` throws on it, whether the body was mutated or returned.
    #[cfg(feature = "javascript")]
    #[test]
    fn a_decorate_unserializable_body_fails_the_decorate() {
        for script in [
            "(req, res) => { const o = {}; o.self = o; res.body = o; }",
            "(req, res) => ({ body: { n: 10n } })",
        ] {
            let result = apply_js_or_rhai_decorate(
                script,
                &decorate_req(),
                "orig",
                &ResponseStatus::Code(200),
                &crate::util::OrderedMap::default(),
                &ScriptScope::detached(test_port()),
                None,
                &trusted_files(),
            );
            assert!(result.is_err(), "{script}");
        }
    }

    // Issue #1635: a falsy status a decorate leaves is falsy, for the default to complete, as
    // Mountebank's `postProcess` does; any other value is read as Node reads it.
    #[cfg(feature = "javascript")]
    #[test]
    fn a_decorate_status_is_read_as_mountebank_reads_it() {
        for (script, want) in [
            (
                "(req, res) => { res.statusCode = null; }",
                ResponseStatus::Code(0),
            ),
            (
                "(req, res) => { res.statusCode = NaN; }",
                ResponseStatus::Code(0),
            ),
            (
                "(req, res) => { delete res.statusCode; }",
                ResponseStatus::Code(0),
            ),
            (
                "(req, res) => { res.statusCode = -1; }",
                ResponseStatus::Text("-1".to_string()),
            ),
            (
                "(req, res) => { res.statusCode = 404; }",
                ResponseStatus::Code(404),
            ),
        ] {
            let (_, status, _) = decorate_1567(script, 201);
            assert_eq!(status, want, "{script}");
        }
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn every_arrow_form_runs_as_javascript() {
        for (script, want) in [
            (
                "(request, response) => { response.body = 'mutated ' + request.path; }",
                "mutated /orders",
            ),
            ("(req, res) => { res.body = 'a'; }", "a"),
            ("request => { request.response.body = 'si'; }", "si"),
            (
                "async (request, response) => { response.body = 'as'; }",
                "as",
            ),
            ("async config => { config.response.body = 'ac'; }", "ac"),
        ] {
            let (body, status, headers) = decorate_1567(script, 200);
            assert_eq!(body, want, "{script}");
            assert_eq!(status, ResponseStatus::Code(200), "{script}");
            assert_eq!(headers.get("X-Orig").map(String::as_str), Some("1"));
        }
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn a_returned_object_replaces_the_response() {
        let (body, status, headers) = decorate_1567(
            "function (request, response) { return { body: 'returned' }; }",
            404,
        );
        assert_eq!(body, "returned");
        assert_eq!(
            status,
            ResponseStatus::Code(200),
            "an omitted status is 200"
        );
        assert!(headers.is_empty(), "the returned object has no headers");

        let (body, status, _) =
            decorate_1567("config => ({ statusCode: 201, body: 'cfgret' })", 200);
        assert_eq!(
            (body.as_str(), status),
            ("cfgret", ResponseStatus::Code(201))
        );

        let (body, status, headers) =
            decorate_1567("config => ({ headers: { 'X-New': 'n' }, body: 'b' })", 404);
        assert_eq!((body.as_str(), status), ("b", ResponseStatus::Code(200)));
        assert_eq!(
            headers,
            crate::util::OrderedMap::from_iter([("X-New".to_string(), "n".to_string())])
        );
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn a_falsy_return_keeps_the_mutated_response() {
        let (body, status, headers) =
            decorate_1567("config => { config.response.body = 'mut'; return 0; }", 404);
        assert_eq!((body.as_str(), status), ("mut", ResponseStatus::Code(404)));
        assert_eq!(headers.get("X-Orig").map(String::as_str), Some("1"));
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn a_deleted_header_is_gone_in_both_forms() {
        for script in [
            "config => { delete config.response.headers['X-Orig']; config.response.body = 'd'; }",
            "function (request, response) { delete response.headers['X-Orig']; response.body = 'd'; }",
            "(request, response) => { delete response.headers['X-Orig']; response.body = 'd'; }",
        ] {
            let (body, _, headers) = decorate_1567(script, 200);
            assert_eq!(body, "d", "{script}");
            assert!(!headers.contains_key("X-Orig"), "{script}: {headers:?}");
        }
    }

    // =========================================================================
    // Issue #116: Multi-valued headers preserved via create_stub_from_proxy_response
    // =========================================================================

    #[test]
    fn test_create_stub_preserves_multi_valued_headers() {
        // Multiple Set-Cookie headers are preserved as separate values in the stub (issue #238)
        let headers = vec![
            ("Set-Cookie".to_string(), "session=abc".to_string()),
            ("Set-Cookie".to_string(), "theme=dark".to_string()),
            ("Content-Type".to_string(), "text/html".to_string()),
        ];

        let stub =
            create_stub_from_proxy_response(vec![], 200, &headers, b"OK", None, None, None, 0);

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                let cookies = is.headers.get("Set-Cookie").unwrap();
                assert_eq!(
                    cookies,
                    &vec!["session=abc".to_string(), "theme=dark".to_string()],
                    "Multi-valued Set-Cookie headers are preserved as separate values"
                );
                assert_eq!(
                    is.headers.get("Content-Type").unwrap(),
                    &vec!["text/html".to_string()]
                );
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    fn recorded_headers(headers: &[(&str, &str)]) -> crate::imposter::Headers {
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let stub =
            create_stub_from_proxy_response(vec![], 200, &headers, b"OK", None, None, None, 0);
        match &stub.responses[0] {
            StubResponse::Is { is, .. } => is.headers.clone(),
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    // Issue #1366: hyper hands the recorder lower-case names; Mountebank records the upstream's
    // spelling, which an HTTP/1 server commonly sends Title-Cased. A name with an upper-case letter
    // can only come from a proxy behavior and is kept as written.
    #[test]
    fn recorded_stub_title_cases_upstream_header_names() {
        let headers = recorded_headers(&[
            ("content-type", "application/json"),
            ("x-request-id", "7"),
            ("X-DECORATED", "d"),
        ]);
        let mut names: Vec<&str> = headers.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["Content-Type", "X-DECORATED", "X-Request-Id"]);
    }

    #[test]
    fn recorded_stub_merges_case_variants_upstream_first() {
        let headers = recorded_headers(&[("set-cookie", "a"), ("Set-Cookie", "b")]);
        assert_eq!(headers.len(), 1, "{headers:?}");
        assert_eq!(headers["Set-Cookie"], ["a", "b"]);
    }

    #[test]
    fn test_create_stub_hop_by_hop_headers_filtered() {
        let headers = vec![
            ("Content-Type".to_string(), "text/html".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("Connection".to_string(), "keep-alive".to_string()),
            ("Keep-Alive".to_string(), "timeout=5".to_string()),
        ];

        let stub =
            create_stub_from_proxy_response(vec![], 200, &headers, b"OK", None, None, None, 0);

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                assert!(is.headers.contains_key("Content-Type"));
                assert!(
                    !is.headers.contains_key("Transfer-Encoding"),
                    "Transfer-Encoding should be filtered"
                );
                assert!(
                    !is.headers.contains_key("Connection"),
                    "Connection should be filtered"
                );
                assert!(
                    !is.headers.contains_key("Keep-Alive"),
                    "Keep-Alive should be filtered"
                );
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    // =========================================================================
    // Issue #117: Binary response bodies correctly base64-encoded
    // =========================================================================

    #[test]
    fn test_create_stub_binary_body_base64_encoded() {
        // Non-UTF-8 bytes should be base64-encoded with binary mode
        let binary_body: Vec<u8> = vec![0x00, 0xFF, 0xFE, 0xFD, 0x89, 0x50, 0x4E, 0x47];

        let stub =
            create_stub_from_proxy_response(vec![], 200, &[], &binary_body, None, None, None, 0);

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                assert_eq!(is.mode, ResponseMode::Binary, "Binary body should set mode");

                // Verify the body is base64
                use base64::Engine;
                let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&binary_body);
                assert_eq!(
                    is.body.as_ref().unwrap().as_str().unwrap(),
                    expected_b64,
                    "Binary body should be base64-encoded"
                );
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    #[test]
    fn test_create_stub_text_body_not_base64() {
        let stub = create_stub_from_proxy_response(
            vec![],
            200,
            &[],
            b"Hello, World!",
            None,
            None,
            None,
            0,
        );

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                assert_eq!(
                    is.mode,
                    ResponseMode::Text,
                    "Text body should use text mode"
                );
                assert_eq!(is.body.as_ref().unwrap().as_str().unwrap(), "Hello, World!");
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    #[test]
    fn test_create_stub_json_body_parsed() {
        let stub = create_stub_from_proxy_response(
            vec![],
            200,
            &[],
            br#"{"key": "value"}"#,
            None,
            None,
            None,
            0,
        );

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                assert_eq!(is.mode, ResponseMode::Text);
                // JSON bodies are parsed into serde_json::Value, not stored as strings
                let body = is.body.as_ref().unwrap();
                assert!(body.is_object(), "JSON body should be parsed as object");
                assert_eq!(body["key"], "value");
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    #[test]
    fn test_create_stub_empty_body() {
        let stub = create_stub_from_proxy_response(vec![], 204, &[], b"", None, None, None, 0);

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                assert_eq!(is.mode, ResponseMode::Text);
                assert_eq!(
                    is.body,
                    Some(serde_json::json!("")),
                    "an empty body is a written body, so a defaultResponse never fills it (#1369)"
                );
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    #[test]
    fn test_create_stub_with_latency_and_decorate() {
        let stub = create_stub_from_proxy_response(
            vec![],
            200,
            &[],
            b"OK",
            Some(150),
            Some("function(request, response) {}".to_string()),
            None,
            0,
        );

        match &stub.responses[0] {
            StubResponse::Is { behaviors, .. } => {
                // Compiled into a program: the recorded latency first, then the decoration.
                assert_eq!(
                    behaviors.as_ref().unwrap(),
                    &serde_json::json!([
                        {"wait": 150},
                        {"decorate": "function(request, response) {}"}
                    ])
                );
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    // =========================================================================
    // Truncation tests
    // =========================================================================

    #[test]
    fn test_truncate_with_ellipsis_short_string() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_with_ellipsis_exact_length() {
        assert_eq!(truncate_with_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_with_ellipsis_long_string() {
        assert_eq!(truncate_with_ellipsis("hello world", 5), "hello...");
    }

    #[test]
    fn test_truncate_with_ellipsis_unicode_safe() {
        // "日本語" is 9 bytes (3 bytes per character)
        // Truncating at byte 5 would be mid-character
        // floor_char_boundary(5) returns 3 (end of first char)
        let text = "日本語";
        assert_eq!(text.len(), 9);
        assert_eq!(truncate_with_ellipsis(text, 5), "日...");
    }

    #[test]
    fn test_truncate_with_ellipsis_emoji() {
        // Each emoji is 4 bytes
        // floor_char_boundary(5) returns 4 (end of first emoji)
        let text = "👋🌍🎉";
        assert_eq!(truncate_with_ellipsis(text, 5), "👋...");
    }

    #[test]
    fn test_truncate_with_ellipsis_mixed_content() {
        // "Hello " is 6 bytes, "世" is 3 bytes, "界" is 3 bytes, "!" is 1 byte = 13 bytes
        // floor_char_boundary(8) returns 6 (byte 8 is mid-character of "世")
        let text = "Hello 世界!";
        assert_eq!(truncate_with_ellipsis(text, 8), "Hello ...");
    }

    #[test]
    fn test_truncate_with_ellipsis_empty_string() {
        assert_eq!(truncate_with_ellipsis("", 10), "");
    }

    #[test]
    fn test_truncate_with_ellipsis_zero_max_len() {
        assert_eq!(truncate_with_ellipsis("hello", 0), "...");
    }

    #[test]
    fn test_create_stub_recorded_from_populated() {
        let stub = create_stub_from_proxy_response(
            vec![],
            200,
            &[],
            b"OK",
            None,
            None,
            Some("http://upstream:8080".to_string()),
            42,
        );
        assert_eq!(stub.recorded_from.as_deref(), Some("http://upstream:8080"));
    }

    // Issue #1519: every recorded response carries its round trip, the mark the clear reads.
    #[test]
    fn a_recorded_response_carries_its_proxy_response_time() {
        let stub = create_stub_from_proxy_response(vec![], 200, &[], b"OK", None, None, None, 42);
        assert!(stub.responses[0].is_recorded());
        assert_eq!(
            serde_json::to_value(&stub.responses[0]).expect("prints")["is"]["_proxyResponseTime"],
            42
        );
    }

    #[test]
    fn test_create_stub_recorded_from_none_when_not_provided() {
        let stub = create_stub_from_proxy_response(vec![], 200, &[], b"OK", None, None, None, 0);
        assert!(stub.recorded_from.is_none());
    }

    // =====================================================================================
    // Issue #476: decorate runs off the async worker with a wall-clock deadline.
    // =====================================================================================

    // AC2 (happy): a fast decorate mutates the response through the bounded path.
    #[cfg(feature = "javascript")]
    #[tokio::test]
    async fn mb_decorate_bounded_applies() {
        let Decorated {
            body,
            status_code: status,
            ..
        } = apply_decorate_bounded(
            "function (request, response) { response.body = response.body + '-decorated'; }"
                .to_string(),
            decorate_req(),
            "orig".to_string(),
            ResponseStatus::Code(200),
            crate::util::OrderedMap::default(),
            ScriptScope::detached(test_port()),
            None,
            trusted_files(),
            std::time::Duration::from_millis(60_000),
        )
        .await
        .expect("fast decorate");
        assert_eq!(body, "orig-decorated");
        assert_eq!(status, ResponseStatus::Code(200));
    }

    // AC2: a runaway decorate yields DecorateError::Timeout near the deadline instead of
    // blocking a runtime worker for its full duration.
    #[cfg(feature = "javascript")]
    #[tokio::test]
    async fn mb_decorate_bounded_times_out() {
        let start = std::time::Instant::now();
        let res = apply_decorate_bounded(
            "function (request, response) { var i = 0; while (i < 100000000) { i += 1; } }"
                .to_string(),
            decorate_req(),
            "orig".to_string(),
            ResponseStatus::Code(200),
            crate::util::OrderedMap::default(),
            ScriptScope::detached(test_port()),
            None,
            trusted_files(),
            std::time::Duration::from_millis(25),
        )
        .await;
        match res {
            Err(DecorateError::Timeout(ms)) => assert_eq!(ms, 25),
            other => panic!("expected DecorateError::Timeout, got {other:?}"),
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(3),
            "must return near the configured deadline, not after the loop cap"
        );
    }

    // Issue #1593: an `async` decorate waiting on a timer due after the imposter's script timeout
    // is the decorate timeout, not a script error.
    #[cfg(feature = "javascript")]
    #[tokio::test]
    async fn mb_decorate_bounded_timer_past_the_deadline_times_out() {
        let res = apply_decorate_bounded(
            "async config => { await new Promise(r => setTimeout(r, 10000)); }".to_string(),
            decorate_req(),
            "orig".to_string(),
            ResponseStatus::Code(200),
            crate::util::OrderedMap::default(),
            ScriptScope::detached(test_port()),
            None,
            trusted_files(),
            std::time::Duration::from_millis(100),
        )
        .await;
        match res {
            Err(DecorateError::Timeout(ms)) => assert_eq!(ms, 100),
            other => panic!("expected DecorateError::Timeout, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod prepared_response_tests {
    //! Gate for issue #703. The differential test proves the fast-path `PreparedResponse` serves
    //! byte-identically to the legacy `execute_stub_response_with_rift` + slow-path assembly; the
    //! eligibility tests pin which responses stay on the slow path.
    use super::*;
    use crate::imposter::types::{IsResponse, ResponseMode, StubResponse};
    use serde_json::json;

    fn is_response(
        status: u16,
        headers: &[(&str, &str)],
        body: Option<serde_json::Value>,
    ) -> IsResponse {
        let mut map: crate::imposter::Headers = crate::imposter::Headers::new();
        for (k, v) in headers {
            map.entry((*k).to_string())
                .or_default()
                .push((*v).to_string());
        }
        IsResponse::new(status, map, body, ResponseMode::Text)
    }

    fn prepared_of(resp: &StubResponse) -> Option<&PreparedResponse> {
        match resp {
            StubResponse::Is { prepared, .. } => prepared.as_deref(),
            _ => None,
        }
    }

    /// Header (name, value) multiset, lowercased — header order is non-deterministic (the slow path
    /// iterates a `HashMap`), so equality is by multiset, not sequence.
    fn header_multiset(pairs: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = pairs
            .into_iter()
            .map(|(k, val)| (k.to_lowercase(), val))
            .collect();
        v.sort();
        v
    }

    /// The served (status, header-multiset, body-bytes) the legacy handler slow path produces for a
    /// static `is` response: execute → per-value headers + `x-rift-imposter` → `apply_date_templates`.
    fn legacy_serve(resp: &StubResponse) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let (status, headers, body, _behaviors, _rift, mode, _fault) =
            execute_stub_response_with_rift(resp).expect("static is executes");
        assert_eq!(mode, ResponseMode::Text, "corpus is text-only");
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (k, values) in &headers {
            for v in values {
                pairs.push((k.clone(), v.clone()));
            }
        }
        pairs.push(("x-rift-imposter".to_string(), "true".to_string()));
        let body_bytes = crate::extensions::apply_date_templates(&body).into_bytes();
        (status, header_multiset(pairs), body_bytes)
    }

    fn prepared_serve(prep: &PreparedResponse) -> (u16, Vec<(String, String)>, Vec<u8>) {
        (
            prep.status.as_u16(),
            header_multiset(
                prep.headers
                    .iter()
                    .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap().to_string())),
            ),
            prep.body.to_vec(),
        )
    }

    #[test]
    fn prepared_matches_legacy_for_static_corpus() {
        let corpus = vec![
            StubResponse::new_is(
                is_response(200, &[], Some(json!("plain string body"))),
                None,
                None,
            ),
            StubResponse::new_is(
                is_response(201, &[], Some(json!({"id": 1, "name": "x"}))),
                None,
                None,
            ),
            StubResponse::new_is(is_response(204, &[], None), None, None),
            StubResponse::new_is(
                is_response(
                    200,
                    &[("Content-Type", "text/plain"), ("X-A", "1")],
                    Some(json!("ok")),
                ),
                None,
                None,
            ),
            StubResponse::new_is(
                is_response(
                    200,
                    &[("Set-Cookie", "a=1"), ("Set-Cookie", "b=2")],
                    Some(json!("multi")),
                ),
                None,
                None,
            ),
            // JSON (non-string) body with no Content-Type: exercises the application/json default.
            StubResponse::new_is(is_response(200, &[], Some(json!({"k": "v"}))), None, None),
            // JSON body with an oddly-cased Content-Type header: after #723 the case-insensitive
            // check suppresses the injected default on both paths (one Content-Type, not a
            // duplicate) — this entry locks fast/slow-path parity on that fixed behavior.
            StubResponse::new_is(
                is_response(
                    200,
                    &[("CONTENT-TYPE", "application/xml")],
                    Some(json!({"k": "v"})),
                ),
                None,
                None,
            ),
            // Non-object rendered bodies (array, number) take the same rendered_body path.
            StubResponse::new_is(is_response(200, &[], Some(json!([1, 2, 3]))), None, None),
            StubResponse::new_is(is_response(200, &[], Some(json!(42))), None, None),
        ];
        for resp in &corpus {
            let prep = prepared_of(resp).expect("static response must be prepared");
            assert_eq!(
                prepared_serve(prep),
                legacy_serve(resp),
                "fast path diverged from legacy"
            );
        }
    }

    // A number or boolean body is served as its JSON text with no `Content-Type`, on both serve
    // paths, as in Mountebank; only an object or array earns `application/json`.
    #[test]
    fn a_scalar_json_body_earns_no_content_type() {
        for body in [json!(42), json!(true)] {
            let resp = StubResponse::new_is(is_response(200, &[], Some(body.clone())), None, None);
            let (_s, prepared, b) = prepared_serve(prepared_of(&resp).unwrap());
            assert_eq!(b, body.to_string().into_bytes());
            assert!(
                !prepared
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type")),
                "{body}: {prepared:?}"
            );
            let (_s, legacy, _b) = legacy_serve(&resp);
            assert!(
                !legacy
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type")),
                "{body}: {legacy:?}"
            );
        }
    }

    #[test]
    fn content_type_default_and_marker_present_for_json_body() {
        let resp = StubResponse::new_is(is_response(200, &[], Some(json!({"k": "v"}))), None, None);
        let (_s, headers, _b) = prepared_serve(prepared_of(&resp).unwrap());
        assert!(headers.contains(&("content-type".to_string(), "application/json".to_string())));
        assert!(headers.contains(&("x-rift-imposter".to_string(), "true".to_string())));
    }

    #[test]
    fn explicit_content_type_is_not_overridden() {
        let resp = StubResponse::new_is(
            is_response(
                200,
                &[("content-type", "application/xml")],
                Some(json!({"k": "v"})),
            ),
            None,
            None,
        );
        let (_s, headers, _b) = prepared_serve(prepared_of(&resp).unwrap());
        let cts: Vec<_> = headers
            .iter()
            .filter(|(k, _)| k == "content-type")
            .collect();
        assert_eq!(
            cts.len(),
            1,
            "no duplicate Content-Type when one is configured"
        );
        assert_eq!(cts[0].1, "application/xml");
    }

    #[test]
    fn oddly_cased_content_type_not_duplicated() {
        // #723: a config header spelled in a nonstandard casing (`CONTENT-TYPE`) with a JSON body
        // must NOT receive a second, injected `Content-Type: application/json`. Both serve paths
        // must emit exactly one Content-Type, and it must be the configured value.
        let resp = StubResponse::new_is(
            is_response(
                200,
                &[("CONTENT-TYPE", "application/xml")],
                Some(json!({"k": "v"})),
            ),
            None,
            None,
        );
        for (label, served) in [
            ("execute", legacy_serve(&resp)),
            ("prepared", prepared_serve(prepared_of(&resp).unwrap())),
        ] {
            let cts: Vec<_> = served
                .1
                .iter()
                .filter(|(k, _)| k == "content-type")
                .collect();
            assert_eq!(
                cts.len(),
                1,
                "{label}: no duplicate Content-Type for an oddly-cased config header"
            );
            assert_eq!(
                cts[0].1, "application/xml",
                "{label}: keeps the configured value"
            );
        }
    }

    #[test]
    fn behaviors_force_slow_path() {
        let resp = StubResponse::new_is(
            is_response(200, &[], Some(json!("x"))),
            Some(json!({"wait": 5})),
            None,
        );
        assert!(
            prepared_of(&resp).is_none(),
            "_behaviors must not be prepared"
        );
    }

    #[test]
    fn rift_templated_and_fault_force_slow_path() {
        let templated = RiftResponseExtension {
            dataset: None,
            templated: true,
            ..Default::default()
        };
        let resp = StubResponse::new_is(
            is_response(200, &[], Some(json!("x"))),
            None,
            Some(Box::new(templated)),
        );
        assert!(
            prepared_of(&resp).is_none(),
            "_rift.templated must not be prepared"
        );

        let faulted = RiftResponseExtension {
            dataset: None,
            fault: Some(crate::imposter::types::RiftFaultConfig::default()),
            ..Default::default()
        };
        let resp = StubResponse::new_is(
            is_response(200, &[], Some(json!("x"))),
            None,
            Some(Box::new(faulted)),
        );
        assert!(
            prepared_of(&resp).is_none(),
            "_rift.fault must not be prepared"
        );

        let scripted = RiftResponseExtension {
            dataset: None,
            script: Some(crate::imposter::types::RiftScriptConfig::default()),
            ..Default::default()
        };
        let resp = StubResponse::new_is(
            is_response(200, &[], Some(json!("x"))),
            None,
            Some(Box::new(scripted)),
        );
        assert!(
            prepared_of(&resp).is_none(),
            "_rift.script must not be prepared"
        );
    }

    #[test]
    fn request_templates_in_body_or_header_force_slow_path() {
        let body_tpl = StubResponse::new_is(
            is_response(200, &[], Some(json!("hello ${request.path}"))),
            None,
            None,
        );
        assert!(
            prepared_of(&body_tpl).is_none(),
            "${{request.*}} body must not be prepared"
        );

        let header_tpl = StubResponse::new_is(
            is_response(200, &[("X-Echo", "${request.query.q}")], Some(json!("x"))),
            None,
            None,
        );
        assert!(
            prepared_of(&header_tpl).is_none(),
            "${{request.*}} header must not be prepared"
        );
    }

    #[test]
    fn date_templates_force_slow_path() {
        let resp = StubResponse::new_is(
            is_response(200, &[], Some(json!("expires {{DAYS+7}} at {{NOW}}"))),
            None,
            None,
        );
        assert!(
            prepared_of(&resp).is_none(),
            "date-token body must not be prepared"
        );
    }

    #[test]
    fn binary_mode_forces_slow_path() {
        let mut is = is_response(200, &[], Some(json!("aGVsbG8=")));
        is.mode = ResponseMode::Binary;
        let resp = StubResponse::new_is(is, None, None);
        assert!(
            prepared_of(&resp).is_none(),
            "binary mode must not be prepared"
        );
    }

    #[test]
    fn reconstruction_recomputes_prepared() {
        // The RCU stub-mutation flow rebuilds responses through `new_is`, so `prepared` is derived
        // fresh: a static response is prepared, and rebuilding the same slot with a now-dynamic body
        // drops the prepared form (no stale fast path survives a mutation).
        let stat = StubResponse::new_is(is_response(200, &[], Some(json!("x"))), None, None);
        assert!(prepared_of(&stat).is_some());
        let dynamic = StubResponse::new_is(
            is_response(200, &[], Some(json!("${request.path}"))),
            None,
            None,
        );
        assert!(prepared_of(&dynamic).is_none());
    }
}

#[cfg(test)]
mod rift_only_tests {
    //! Issue #1685: a `_rift`-only response without a script is served as `is: {}`.
    use super::*;
    use serde_json::json;

    fn rift_only(rift: serde_json::Value) -> StubResponse {
        StubResponse::RiftScript {
            rift: Box::new(serde_json::from_value(rift).expect("_rift parses")),
            ignored_behaviors: Some(json!({ "wait": 10 })),
        }
    }

    #[test]
    fn script_less_rift_executes_as_empty_is() {
        let response = rift_only(json!({ "fault": { "tcp": "malformed" } }));
        let (status, headers, body, behaviors, rift, mode, is_fault) =
            execute_stub_response_with_rift(&response).expect("served as `is: {}`");
        assert_eq!(status, 200);
        assert!(headers.is_empty(), "{headers:?}");
        assert_eq!(body, "");
        assert!(
            behaviors.is_none(),
            "behaviors other than repeat stay ignored"
        );
        assert!(
            rift.is_some_and(|r| r.fault.is_some()),
            "its `_rift` rides along"
        );
        assert_eq!(mode, ResponseMode::Text);
        assert!(!is_fault);
        let served = served_is(&response).expect("served as `is: {}`");
        assert!(
            served.omitted.status_code && served.omitted.body,
            "every field omitted"
        );
    }

    #[test]
    fn scripted_rift_still_executes_as_none() {
        let response = rift_only(json!({ "script": { "code": "response.body = 'x';" } }));
        assert!(execute_stub_response_with_rift(&response).is_none());
        assert!(served_is(&response).is_none());
    }
}
