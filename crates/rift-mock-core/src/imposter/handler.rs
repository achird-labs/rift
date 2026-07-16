//! Request handling logic for imposters.
//!
//! This module handles incoming HTTP requests to imposters, including
//! debug mode, proxy handling, inject execution, and response generation.

use super::core::Imposter;
use super::predicates::parse_query_string;
use super::response::{
    apply_decorate_bounded, execute_stub_response_with_rift, get_rift_script_config,
};
use super::types::{
    DebugMatchResult, DebugRequest, DebugResponse, ProxyResponse, RecordedRequest, ResponseMode,
    StubResponse,
};
use crate::behaviors::{
    CsvCache, RequestContext, apply_copy_behaviors, apply_lookup_behaviors, apply_shell_transform,
    header_to_title_case,
};
use crate::extensions::decorate::{
    ResponseDecorator, ResponsePhase, backend_error_response, with_annotation_scope,
};
use crate::extensions::template::{RequestData, has_template_variables, process_template};
use crate::scripting::{
    FaultDecision, ScriptCtxExtras, ScriptRequest, ScriptStubContext, resolve_script_timeout_ms,
    should_inject_bounded_with_ctx, should_inject_bounded_with_ctx_traced,
};
#[cfg(feature = "javascript")]
use crate::scripting::{MountebankRequest, execute_mountebank_inject_bounded};
use crate::util::build_response_with_headers;
use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};

use rand::Rng;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

/// Maximum allowed request body size (10 MB)
const MAX_REQUEST_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Why a request body could not be collected (issue #694). `Limited::collect` funnels both the
/// size-cap breach and a genuine transport failure (connection reset, truncated stream) through one
/// `Err`; conflating them reported every network failure to the client as `413` "body too large"
/// and logged nothing. Splitting the two lets the handler answer the right status and log the cause.
#[derive(Debug)]
enum BodyReadError {
    /// The body exceeded the size cap — a real `413`.
    TooLarge,
    /// The body could not be read to completion — a client transmission failure (`400`). The cause
    /// is kept boxed, not stringified (the #688 lesson), so the log site renders it in full.
    Read(Box<dyn std::error::Error + Send + Sync>),
}

/// Collect a request body under a size cap, distinguishing the cap breach from a read failure.
/// Body-generic so the classification is unit-testable against a synthetic body without a live
/// listener (mirrors `admin_api::collect_limited`, issue #546 — but kept here, not shared, because
/// `rift-http-proxy` depends on this crate, not the reverse).
async fn collect_body_limited<B>(body: B, limit: usize) -> Result<Bytes, BodyReadError>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    match Limited::new(body, limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        // `Limited` boxes the error; a cap breach is specifically `LengthLimitError`, anything else
        // is the underlying body failing to produce its bytes.
        Err(e)
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some() =>
        {
            Err(BodyReadError::TooLarge)
        }
        Err(e) => Err(BodyReadError::Read(e)),
    }
}

/// The `400` door for a request body that could not be read (issue #694). Logs the real cause —
/// previously this failure was silent — then serves the canonical envelope. A read failure is the
/// client's transmission going wrong, so it is a `400`, distinct from the `413` size-cap door; a
/// `5xx` here would blame the server for the client's dropped connection.
fn body_read_error_response(e: &dyn std::error::Error) -> Response<Full<Bytes>> {
    warn!("failed to read request body: {e}");
    build_response_with_headers(
        StatusCode::BAD_REQUEST,
        [
            ("x-rift-imposter", "true"),
            ("content-type", "application/json"),
        ],
        crate::response::error_body(StatusCode::BAD_REQUEST, "Failed to read request body"),
    )
}

/// Process-wide cache for `lookup` behavior CSV data sources, shared across all
/// imposters so a file is parsed once and reused on subsequent requests.
fn csv_cache() -> &'static CsvCache {
    static CSV_CACHE: std::sync::OnceLock<CsvCache> = std::sync::OnceLock::new();
    CSV_CACHE.get_or_init(CsvCache::new)
}

/// Marker header (issue #499) attached to every timeout-mapped response: a 504 carrying this
/// header means a script hook exceeded its wall-clock deadline (transient/retry-worthy), as
/// distinct from a broken-script 400/500 (a permanent config error).
const SCRIPT_TIMEOUT_HEADER: &str = "x-rift-script-timeout";

/// Build the 504 response for a timed-out response/predicate `inject` (issue #499): the Mountebank
/// `{"errors":[{code,message}]}` envelope with a timeout-specific `code`, plus the shared
/// `x-rift-inject-error` marker and the [`SCRIPT_TIMEOUT_HEADER`] that tells a timeout apart from a
/// genuinely broken inject (which stays a 400).
fn inject_timeout_response(code: &str, message: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({
        "errors": [{ "code": code, "message": message }]
    })
    .to_string();
    build_response_with_headers(
        StatusCode::GATEWAY_TIMEOUT,
        [
            ("x-rift-imposter", "true"),
            ("x-rift-inject-error", "true"),
            (SCRIPT_TIMEOUT_HEADER, "true"),
            ("content-type", "application/json"),
        ],
        body,
    )
}

/// Build the 500 for a failed `{{ }}` render (issue #359). Split out from the request path — as
/// `debug_serialize_or_500` was for #611 — so this door can be pinned by a test: it fires only
/// under `RIFT_DEBUG`, a process-global a shared-process test cannot set without racing its
/// siblings, so end-to-end coverage is not available to it.
fn template_error_response(e: &str) -> Response<Full<Bytes>> {
    build_response_with_headers(
        StatusCode::INTERNAL_SERVER_ERROR,
        [
            ("x-rift-imposter", "true"),
            ("x-rift-template-error", "true"),
            ("content-type", "application/json"),
        ],
        crate::response::error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("template rendering failed: {e}"),
        ),
    )
}

// The debug-endpoint doors (issue #695). The debug success path answers JSON (`X-Rift-Debug-Response`),
// so these error paths do too — plain text here was the same-endpoint gap #687 closed elsewhere.
// Split out from the match arms so each can be pinned by a test: neither is safely provokable
// end-to-end (a task panic must be injected; a timeout needs a busy-loop that starves the shared
// script pool). The caller keeps its own `warn!` — these build the response only.

/// A `_rift`-debug matching task that panicked → 500, canonical envelope.
fn debug_matching_error_response() -> Response<Full<Bytes>> {
    build_response_with_headers(
        StatusCode::INTERNAL_SERVER_ERROR,
        [
            ("x-rift-imposter", "true"),
            ("content-type", "application/json"),
        ],
        crate::response::error_body(StatusCode::INTERNAL_SERVER_ERROR, "Debug matching failed"),
    )
}

/// A debug matching run that missed the `_rift.scriptEngine.timeoutMs` deadline → 504 + the timeout
/// marker (issues #476/#499), like every other script deadline. It was a 500 before #695, which
/// contradicted that transient-vs-permanent contract.
fn debug_matching_timeout_response() -> Response<Full<Bytes>> {
    build_response_with_headers(
        StatusCode::GATEWAY_TIMEOUT,
        [
            ("x-rift-imposter", "true"),
            (SCRIPT_TIMEOUT_HEADER, "true"),
            ("content-type", "application/json"),
        ],
        crate::response::error_body(StatusCode::GATEWAY_TIMEOUT, "Debug matching timed out"),
    )
}

/// The terminal 500 for a response whose own header/body hyper rejected (issue #695) — a bad
/// upstream/inject/script/stub header value that fails `.body(...)`. Serves the canonical envelope
/// so the last-resort door matches every other imposter error door instead of a bare string, and
/// logs the caller's `what` context so the failing door is named. Safe as a terminal fallback:
/// `error_body` is infallible by construction and `build_response_with_headers` with literal headers
/// falls back to `internal_error_fallback` — the chain cannot itself panic.
fn build_failure_response(e: &hyper::http::Error, log_context: &str) -> Response<Full<Bytes>> {
    warn!("{log_context}: {e}");
    build_response_with_headers(
        StatusCode::INTERNAL_SERVER_ERROR,
        [
            ("x-rift-imposter", "true"),
            ("content-type", "application/json"),
        ],
        crate::response::error_body(StatusCode::INTERNAL_SERVER_ERROR, "Response build error"),
    )
}

/// Map a matcher error (`find_matching_stub_with_client`) to its response.
///
/// A predicate-`inject` failure (issue #440 — an object-build failure or the script itself
/// throwing) is Mountebank-shaped error parity with the response-inject case just below: a 400
/// with `{"errors":[{"code":"invalid predicate injection","message":"..."}]}`, not a bare 500 —
/// the script failed to produce a valid match decision, a client (config) problem, not a server
/// fault. A predicate-matching *deadline* (issue #499) is instead a 504 `predicate injection
/// timeout` a client can retry — checked first, since it too surfaces during matching. Every other
/// matcher error (e.g. a scenario-state backend failure, issue #318) keeps the existing
/// [`backend_error_response`] 5xx mapping.
/// Log a failure with its whole cause chain (issue #679).
///
/// `{e:#}` is anyhow's alternate Display: the entire chain on one line, which is what a log pipeline
/// wants (`{e:?}` renders a multi-line `Caused by:` block for a terminal). Plain `{}` renders only
/// the outermost context — which is how a DNS failure, a certificate rejection and a refused
/// connection all became the same opaque line, on a 502 whose only job is to say why the upstream
/// did not answer. `rift-ffi`'s CA setup path reaches for `{err:#}` for exactly this reason.
///
/// Every site that logs an `anyhow` failure here routes through this function, so the chain cannot
/// be dropped again one `warn!` at a time.
fn log_upstream_failure(context: &str, e: &anyhow::Error) {
    warn!("{context}: {e:#}");
}

/// The 502 for an upstream a proxy response could not reach, plus the log line that explains it.
///
/// Issue #679: the two audiences get different detail, deliberately. The operator gets the whole
/// chain (see [`log_upstream_failure`]); the client gets `client_prefix` plus the outermost context
/// only, since a chain can name internal hosts and resolver detail that is none of the caller's
/// business — and the operator already has it in the log.
///
/// The body goes through the crate's canonical Mountebank builder rather than a JSON string
/// literal, which produced invalid JSON the moment a message held a quote (the #611 class), and
/// which left this door emitting a legacy `{"error"}` shape that 0.13.6 had already unified
/// everywhere else.
fn upstream_error_response(
    e: &anyhow::Error,
    log_context: &str,
    marker: &'static str,
    client_prefix: &str,
) -> Response<Full<Bytes>> {
    log_upstream_failure(log_context, e);
    build_response_with_headers(
        StatusCode::BAD_GATEWAY,
        [
            ("x-rift-imposter", "true"),
            (marker, "true"),
            ("content-type", "application/json"),
        ],
        // The client's message is built here, from a prefix, rather than taken ready-made: `{e}`
        // and `{e:#}` differ by exactly the chain, so a caller that formatted its own message could
        // leak it with a one-character edit and no test would notice. Taking a prefix leaves the
        // caller nothing to get wrong.
        crate::response::error_body(StatusCode::BAD_GATEWAY, &format!("{client_prefix}: {e}")),
    )
}

fn matcher_error_response(e: &anyhow::Error) -> Response<Full<Bytes>> {
    if let Some(t) = e.downcast_ref::<crate::scripting::ScriptTimeoutError>() {
        return inject_timeout_response("predicate injection timeout", &t.to_string());
    }
    #[cfg(feature = "javascript")]
    {
        if let Some(pred_err) = e.downcast_ref::<crate::scripting::PredicateInjectionError>() {
            let body = serde_json::json!({
                "errors": [{
                    "code": "invalid predicate injection",
                    "message": pred_err.to_string(),
                }]
            })
            .to_string();
            return build_response_with_headers(
                StatusCode::BAD_REQUEST,
                [
                    ("x-rift-imposter", "true"),
                    ("x-rift-inject-error", "true"),
                    ("content-type", "application/json"),
                ],
                body,
            );
        }
    }
    backend_error_response(e)
}

/// [`handle_imposter_request`] inside a per-request annotation scope, with the configured
/// [`ResponseDecorator`](crate::extensions::decorate::ResponseDecorator) applied (phase
/// `DataPlane`, the imposter's `port`) before the response is written (issue #318). This
/// is the serve-loop wrapper; it is public so custom listeners can reuse the exact
/// production wiring.
pub async fn handle_imposter_request_decorated(
    req: Request<Incoming>,
    imposter: Arc<Imposter>,
    client_addr: SocketAddr,
    port: u16,
    decorator: Option<Arc<dyn ResponseDecorator>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let (response, annotations) =
        with_annotation_scope(handle_imposter_request(req, imposter, client_addr)).await;
    let mut response = response?;
    if let Some(decorator) = decorator {
        decorator.decorate(
            ResponsePhase::DataPlane,
            Some(port),
            &annotations,
            response.headers_mut(),
        );
    }
    Ok(response)
}

/// Handle a request to an imposter
pub async fn handle_imposter_request(
    req: Request<Incoming>,
    imposter: Arc<Imposter>,
    client_addr: SocketAddr,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let allow_cors = imposter.config.allow_cors;
    // Capture the method before `req` is consumed so we can record the request metric (issue #269).
    let method = req.method().to_string();
    let mut response = handle_request_inner(req, imposter, client_addr).await?;
    // Record `rift_requests_total` once per request the imposter serves (issue #269). The imposter
    // serve path recorded no Prometheus metrics before; the recording proxy engine
    // (`proxy/handler.rs`) is a disjoint path, so there is no double-count.
    crate::extensions::record_request(&method, response.status().as_u16());
    if allow_cors {
        inject_cors_headers(response.headers_mut());
    }
    Ok(response)
}

fn inject_cors_headers(headers: &mut hyper::HeaderMap) {
    use hyper::header::{HeaderName, HeaderValue};
    for (name, value) in [
        ("access-control-allow-origin", "*"),
        ("access-control-allow-headers", "*"),
        ("access-control-allow-methods", "*"),
    ] {
        let header_name = HeaderName::from_static(name);
        if !headers.contains_key(&header_name) {
            headers.insert(header_name, HeaderValue::from_static(value));
        }
    }
}

/// Make a `{{ }}`-templated header value safe to emit (issue #359 B3, header injection).
///
/// A templated header value can resolve to attacker-controlled request data (a header/query/json
/// value) containing CR, LF, or other control characters — a classic HTTP header-injection vector.
/// Strip every control character so the value can never terminate the header line early or smuggle
/// a second header. If anything was removed (or the sanitized value still isn't a valid header
/// value), emit a `tracing::warn!` so the rejection is visible rather than silent.
fn sanitize_header_value(value: &str) -> String {
    let sanitized: String = value.chars().filter(|c| !c.is_control()).collect();
    if sanitized.len() != value.len() {
        tracing::warn!(
            target: "rift::template",
            original = %value,
            "stripped control characters from a templated header value (possible header-injection attempt)"
        );
    } else if hyper::header::HeaderValue::from_str(&sanitized).is_err() {
        tracing::warn!(
            target: "rift::template",
            value = %value,
            "templated header value is not a representable header value"
        );
    }
    sanitized
}

async fn handle_request_inner(
    req: Request<Incoming>,
    imposter: Arc<Imposter>,
    client_addr: SocketAddr,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Check if enabled
    if !imposter.is_enabled() {
        return Ok(build_response_with_headers(
            StatusCode::SERVICE_UNAVAILABLE,
            [
                ("x-rift-imposter-disabled", "true"),
                ("content-type", "application/json"),
            ],
            crate::response::error_body(StatusCode::SERVICE_UNAVAILABLE, "Imposter is disabled"),
        ));
    }

    // Increment request count
    imposter.increment_request_count();

    // Extract parts we need before consuming the request body. `into_parts()` (issue #561) hands
    // back owned `method`/`uri`/`headers` for free — no `.clone()` needed to get an owned
    // `HeaderMap` for the request context, unlike the old `req.headers().clone()`.
    let (parts, body) = req.into_parts();
    let method = parts.method.to_string();
    let uri = parts.uri;
    let headers_for_context = parts.headers;
    let headers_clone: HashMap<String, String> = headers_for_context
        .iter()
        .map(|(k, v)| {
            (
                header_to_title_case(k.as_str()),
                v.to_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    // Capture ALL values per header for the recorded request (issue #238) — hyper's HeaderMap
    // yields one entry per value, so a header sent twice is preserved here (headers_clone above
    // collapses to one value and stays the single-value view used for matching/context).
    let headers_multi: HashMap<String, Vec<String>> = if imposter.config.record_requests {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in headers_for_context.iter() {
            map.entry(header_to_title_case(k.as_str()))
                .or_default()
                .push(v.to_str().unwrap_or("").to_string());
        }
        map
    } else {
        HashMap::new()
    };
    let path = uri.path().to_string();
    let query_str = uri.query().unwrap_or("").to_string();

    if method.eq_ignore_ascii_case("OPTIONS") && imposter.config.allow_cors {
        return Ok(build_response_with_headers(
            StatusCode::OK,
            [("x-rift-imposter", "true")],
            Bytes::new(),
        ));
    }

    // Collect request body with size limit to prevent memory exhaustion. A cap breach stays a 413;
    // a transport failure mid-read (connection reset, truncated stream) is the client's problem — a
    // 400 — not the size error it was previously mislabeled as (issue #694).
    let body_bytes = match collect_body_limited(body, MAX_REQUEST_BODY_SIZE).await {
        Ok(bytes) => {
            if bytes.is_empty() {
                None
            } else {
                Some(bytes)
            }
        }
        Err(BodyReadError::TooLarge) => {
            return Ok(build_response_with_headers(
                StatusCode::PAYLOAD_TOO_LARGE,
                [
                    ("x-rift-imposter", "true"),
                    ("content-type", "application/json"),
                ],
                crate::response::error_body(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    &format!("Request body exceeds maximum size of {MAX_REQUEST_BODY_SIZE} bytes"),
                ),
            ));
        }
        Err(BodyReadError::Read(e)) => {
            return Ok(body_read_error_response(e.as_ref()));
        }
    };
    // Borrow the body as UTF-8 without forcing an allocation for the common valid-UTF-8 case
    // (issue #561): valid UTF-8 stays `Cow::Borrowed`, so only genuinely invalid UTF-8 pays a
    // copy here. `body_bytes` stays alive for the rest of the function so this borrow remains
    // valid; callers that need an owned `String` (`RecordedRequest`, `ScriptRequest`,
    // thread-moved debug/inject requests) materialize one at that boundary.
    //
    // Invalid UTF-8 (a binary body: protobuf, gzip, an image upload) used to go through
    // `from_utf8_lossy`, which silently replaces the offending bytes with U+FFFD — the recorded
    // request, predicate matches, and script/inject bodies then no longer reflect what the
    // client sent, irreversibly (issue #636). Base64-encode it instead, mirroring the response
    // side's `encode_body_for_stub` (issue #117): every consumer below gets a lossless string
    // representation, and `mode` tells them which kind they have.
    let (body_string, body_mode): (Option<std::borrow::Cow<'_, str>>, ResponseMode) =
        match body_bytes.as_deref() {
            None => (None, ResponseMode::Text),
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => (Some(std::borrow::Cow::Borrowed(text)), ResponseMode::Text),
                Err(_) => (
                    Some(std::borrow::Cow::Owned(
                        base64::engine::general_purpose::STANDARD.encode(bytes),
                    )),
                    ResponseMode::Binary,
                ),
            },
        };

    // Record request if enabled
    if imposter.config.record_requests {
        let recorded = RecordedRequest {
            request_from: client_addr.to_string(),
            method: method.clone(),
            path: path.clone(),
            query: parse_query_string(&query_str),
            headers: headers_multi,
            body: body_string.as_deref().map(str::to_string),
            mode: body_mode.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        imposter.record_request(recorded);
    }

    // Find matching stub
    let method_str = method.as_str();
    let path_str = path.as_str();
    let query_opt = if query_str.is_empty() {
        None
    } else {
        Some(query_str.as_str())
    };

    // Check for X-Rift-Debug header (Rift extension)
    // If present, return match information instead of processing the request
    let is_debug_mode = headers_clone
        .get("X-Rift-Debug")
        .or_else(|| headers_clone.get("x-rift-debug"))
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);

    // Every script execution below (debug-mode matching, predicate inject during matching,
    // response inject, decorate) shares the imposter's `_rift.scriptEngine.timeoutMs`
    // wall-clock budget (issue #476), like `_rift.script`.
    let script_timeout = std::time::Duration::from_millis(
        crate::scripting::resolve_script_timeout_ms(&imposter.config),
    );

    if is_debug_mode {
        // Debug matching evaluates the full predicate set — inject predicates included — so it
        // runs off the async worker under the script deadline like the data-plane matcher
        // (issue #476). Unconditional spawn_blocking: this is an opt-in diagnostic path.
        let debug_imposter = Arc::clone(&imposter);
        let (dbg_method, dbg_path, dbg_query) = (method.clone(), path.clone(), query_str.clone());
        let dbg_headers = headers_clone.clone();
        // Materialize an owned copy here (issue #561): `body_string` borrows from `body_bytes`,
        // which does not outlive this `spawn_blocking` closure's `'static` bound.
        let dbg_body = body_string.as_deref().map(str::to_string);
        let handle = tokio::task::spawn_blocking(move || {
            handle_debug_request(
                &debug_imposter,
                &dbg_method,
                &dbg_path,
                &dbg_query,
                &dbg_headers,
                &dbg_body,
                client_addr,
            )
        });
        return match tokio::time::timeout(script_timeout, handle).await {
            Ok(Ok(response)) => response,
            Ok(Err(join_err)) => {
                warn!("debug matching task panicked: {join_err}");
                Ok(debug_matching_error_response())
            }
            Err(_elapsed) => {
                warn!(
                    "debug matching timed out after {}ms",
                    script_timeout.as_millis()
                );
                Ok(debug_matching_timeout_response())
            }
        };
    }

    // Get client address info for requestFrom, ip predicates
    let request_from = client_addr.to_string();
    let client_ip = client_addr.ip().to_string();
    let matched = match imposter
        .find_matching_stub_with_client_bounded(
            method_str,
            path_str,
            &headers_clone,
            query_opt,
            body_string.as_deref(),
            Some(&request_from),
            Some(&client_ip),
            script_timeout,
        )
        .await
    {
        Ok(matched) => matched,
        // A backend consulted during matching failed (issue #318), or a predicate-`inject`
        // errored (issue #440): surface it, never fall through to "no match" (which would
        // serve the wrong response).
        Err(e) => return Ok(matcher_error_response(&e)),
    };
    if let Some((stub_state, stub_index)) = matched {
        // Scenario FSM: apply the matched stub's newScenarioState transition (no-op unless set).
        // Resolve flow_id from the same single-value header map the matcher used (headers_clone)
        // so the transition writes the exact key the gate read.
        let scenario_flow_id = imposter.resolve_flow_id(&headers_clone);
        // Offload the FSM transition to spawn_blocking on a blocking backend (Redis) so it can't
        // stall the tokio worker; inline on the in-memory backend (issue #475).
        let transition = {
            let flow_id = scenario_flow_id.clone();
            let stub_state = Arc::clone(&stub_state);
            imposter
                .run_flow_blocking(move |imp| {
                    imp.apply_scenario_transition(&flow_id, &stub_state.stub)
                })
                .await
        };
        if let Err(e) = transition {
            return Ok(backend_error_response(&e));
        }

        // Advance the shared response cycler exactly ONCE for this matched request and dispatch on
        // the returned response. Previously each response type was classified by a non-advancing
        // peek and then advanced through a separate cycler call, so a concurrent request could move
        // the cursor between the peek and the advance and serve the wrong branch — or a bogus empty
        // `x-rift-no-match` 200 (issue #559). A consequence of advancing once up front: the cursor
        // now advances even when proxy/inject/script handling fails (a shared atomic cursor cannot
        // be safely un-advanced under concurrency), whereas before a failed handling left the
        // cursor for the next request to retry.
        let response: Option<&StubResponse> = match imposter.next_stub_response(&stub_state) {
            Ok(r) => r,
            Err(e) => return Ok(backend_error_response(&e)),
        };

        // Check if this is a proxy response
        if let Some(StubResponse::Proxy {
            proxy: proxy_config,
        }) = response
        {
            debug!("Handling proxy request to {}", proxy_config.to);
            match imposter
                .handle_proxy_request(
                    proxy_config,
                    method_str,
                    &uri,
                    &headers_clone,
                    body_string.as_deref(),
                )
                .await
            {
                Ok((status, response_headers, body, latency)) => {
                    let mut response = Response::builder().status(status);

                    for (k, v) in &response_headers {
                        if !crate::util::is_hop_by_hop_header(k) {
                            response = response.header(k, v);
                        }
                    }

                    response = response.header("x-rift-imposter", "true");
                    response = response.header("x-rift-proxy", "true");

                    if let Some(ms) = latency {
                        response = response.header("x-rift-proxy-latency", ms.to_string());
                    }

                    return Ok(response
                        .body(Full::new(Bytes::from(body)))
                        .unwrap_or_else(|e| {
                            build_failure_response(
                                &e,
                                "proxy response build failed (bad upstream header?)",
                            )
                        }));
                }
                Err(e) => {
                    return Ok(upstream_error_response(
                        &e,
                        "Proxy request failed",
                        "x-rift-proxy-error",
                        "Proxy error",
                    ));
                }
            }
        }

        // Check if this is an inject response (JavaScript function)
        #[cfg(feature = "javascript")]
        if let Some(StubResponse::Inject { inject: inject_fn }) = response {
            debug!("Handling inject response");

            // Build request for inject function
            let mb_request = MountebankRequest {
                method: method.clone(),
                path: path.clone(),
                query: parse_query_string(&query_str),
                headers: headers_clone.clone(),
                // Owned boundary (issue #561): the inject job is submitted to a `'static` worker
                // closure, so `body_string`'s borrow can't cross it.
                body: body_string.as_deref().map(str::to_string),
                mode: Some(body_mode.clone()),
            };

            match execute_mountebank_inject_bounded(
                inject_fn.clone(),
                mb_request,
                imposter.script_state_key(),
                stub_state.stub.id.clone(),
                script_timeout,
            )
            .await
            {
                Ok(inject_response) => {
                    let mut response = Response::builder().status(inject_response.status_code);

                    for (k, v) in &inject_response.headers {
                        response = response.header(k, v);
                    }

                    response = response.header("x-rift-imposter", "true");
                    response = response.header("x-rift-inject", "true");

                    return Ok(response
                        .body(Full::new(Bytes::from(inject_response.body)))
                        .unwrap_or_else(|e| {
                            build_failure_response(
                                &e,
                                "inject response build failed (bad injected header?)",
                            )
                        }));
                }
                Err(e) => {
                    log_upstream_failure("Inject function failed", &e);
                    // A deadline miss (issue #499) is a transient 504 a client can retry, not the
                    // permanent-config 400 below — distinguish it before the parity mapping.
                    if let Some(t) = e.downcast_ref::<crate::scripting::ScriptTimeoutError>() {
                        return Ok(inject_timeout_response("injection timeout", &t.to_string()));
                    }
                    // Mountebank-shaped error parity (issue #355 Item 5): a failing inject is a
                    // 400 with `{"errors":[{"code":"invalid injection","message":"..."}]}`, not a
                    // bare 500 — the script failed to produce a valid response, which is a client
                    // (config) problem, not a server fault.
                    let body = serde_json::json!({
                        "errors": [{
                            "code": "invalid injection",
                            "message": format!("{e}"),
                        }]
                    })
                    .to_string();
                    return Ok(build_response_with_headers(
                        StatusCode::BAD_REQUEST,
                        [
                            ("x-rift-imposter", "true"),
                            ("x-rift-inject-error", "true"),
                            ("content-type", "application/json"),
                        ],
                        body,
                    ));
                }
            }
        }

        // Check if this is a RiftScript response (_rift.script)
        if let Some(script_config) = response.and_then(get_rift_script_config) {
            // `code` is populated by the config-time resolve-scripts pass (issue #356), which
            // runs before an imposter carrying `file:`/`ref:` scripts is ever created. A `None`
            // here means that pass was skipped (e.g. a stub added through a sub-resource
            // endpoint that doesn't resolve scripts) — surface it as a clear error instead of
            // silently running an empty script.
            let Some(code) = script_config.code.clone() else {
                return Ok(build_response_with_headers(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [
                        ("x-rift-imposter", "true"),
                        ("x-rift-script-error", "true"),
                        ("content-type", "application/json"),
                    ],
                    crate::response::error_body(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "script not resolved: `file:`/`ref:` sources must be resolved before serving",
                    ),
                ));
            };
            let engine = script_config
                .engine
                .clone()
                .unwrap_or_else(|| "rhai".to_string());
            debug!("Handling Rift script response (engine: {})", engine);

            // Build script request. Expose headers with lowercase keys so scripts can read
            // them case-insensitively (e.g. `request.headers["x-flow-id"]`) regardless of the
            // wire casing; this matches the engine docs and HTTP header semantics.
            let script_request = ScriptRequest {
                // Owned boundary (issue #561): `ScriptRequest` is moved into the script engine's
                // worker job, so the body must be materialized here rather than borrowed.
                raw_body: Some(body_string.as_deref().unwrap_or_default().to_string()),
                mode: body_mode.clone(),
                method: method.clone(),
                path: path.clone(),
                headers: headers_clone
                    .iter()
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                    .collect(),
                // Domain-optional parse: a request body may legitimately not be JSON, and the
                // script contract exposes that as `null` rather than an error (issue #611).
                body: body_string
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(serde_json::Value::Null),
                query: parse_query_string(&query_str),
                // Issue #433: populate path params from the matched stub's route pattern, if any.
                path_params: stub_state
                    .stub
                    .route_pattern
                    .as_deref()
                    .map(|pattern| crate::extensions::template::extract_path_params(pattern, &path))
                    .unwrap_or_default(),
            };

            // Execute the script off the async worker under a wall-clock deadline (issue
            // #308): a runaway script is interrupted at `_rift.scriptEngine.timeoutMs`
            // (default 5s) instead of wedging the whole engine.
            let timeout_ms = resolve_script_timeout_ms(&imposter.config);
            let flow_store = imposter.flow_store.clone();

            // ctx.stub (issue #357 Item 1): thread the matched stub's identity through, resolving
            // its current scenario state (if it belongs to a scenario) the same way the FSM gate
            // already does. A backend failure here must surface, not silently drop to `None`
            // (this epic's "nothing fails silently" principle).
            let scenario_state = match &stub_state.stub.scenario_name {
                Some(name) => {
                    // Same spawn_blocking offload as the FSM gate/transition (issue #475).
                    let flow_id = scenario_flow_id.clone();
                    let name = name.clone();
                    match imposter
                        .run_flow_blocking(move |imp| imp.scenario_state(&flow_id, &name))
                        .await
                    {
                        Ok(s) => Some(s),
                        Err(e) => return Ok(backend_error_response(&e)),
                    }
                }
                None => None,
            };
            let ctx_extra = ScriptCtxExtras {
                flow_id: Some(scenario_flow_id.clone()),
                stub: ScriptStubContext {
                    scenario_name: stub_state.stub.scenario_name.clone(),
                    scenario_state,
                    stub_id: stub_state.stub.id.clone(),
                },
                port: imposter.config.port.unwrap_or(0),
            };

            // Debug-mode script trace (issue #360 Item 3): which hook ran, its decision,
            // duration, and ctx.logger lines, so "why didn't my script run the way I expected"
            // is answerable from the response alone. Zero-cost when debug mode is off — the
            // capturing-subscriber/Instant path in the `_traced` variant is only ever built when
            // this flag is set, so the hot (non-debug) path calls the original, unchanged
            // `should_inject_bounded_with_ctx`.
            let (script_result, trace_header): (Result<FaultDecision, anyhow::Error>, _) =
                if crate::util::rift_debug_env() {
                    let (result, mut entry) = should_inject_bounded_with_ctx_traced(
                        engine.clone(),
                        code,
                        format!("rift_script_{stub_index}"),
                        script_request,
                        flow_store,
                        Duration::from_millis(timeout_ms),
                        ctx_extra,
                    )
                    .await;
                    // Cap a chatty script's logger output: the trace ships on a response header,
                    // which must stay bounded (issue #360).
                    entry.logs = crate::scripting::cap_trace_logs(entry.logs);
                    // `ScriptTraceEntry` is plain strings/numbers, so this can't realistically
                    // fail — but on the off chance it does, trace the failure instead of
                    // silently dropping the header.
                    let header = serde_json::to_string(&[entry])
                        .inspect_err(|e| warn!("failed to serialize x-rift-script-trace: {}", e))
                        .ok();
                    (result, header)
                } else {
                    let result = should_inject_bounded_with_ctx(
                        engine.clone(),
                        code,
                        format!("rift_script_{stub_index}"),
                        script_request,
                        flow_store,
                        Duration::from_millis(timeout_ms),
                        ctx_extra,
                    )
                    .await;
                    (result, None)
                };

            match script_result {
                Ok(FaultDecision::Error {
                    status,
                    body,
                    headers,
                    ..
                }) => {
                    let mut response = Response::builder().status(status);
                    for (k, v) in &headers {
                        response = response.header(k, v);
                    }
                    response = response.header("x-rift-imposter", "true");
                    response = response.header("x-rift-script", &engine);
                    if let Some(trace) = &trace_header {
                        response = response.header("x-rift-script-trace", trace.as_str());
                    }

                    return Ok(response
                        .body(Full::new(Bytes::from(body)))
                        .unwrap_or_else(|e| {
                            build_failure_response(
                                &e,
                                "script response build failed (bad script header?)",
                            )
                        }));
                }
                Ok(FaultDecision::Latency { duration_ms, .. }) => {
                    // Apply latency then return 200 OK
                    tokio::time::sleep(Duration::from_millis(duration_ms)).await;
                    let mut headers = vec![
                        ("x-rift-imposter".to_string(), "true".to_string()),
                        ("x-rift-script".to_string(), engine.clone()),
                        ("x-rift-latency-ms".to_string(), duration_ms.to_string()),
                    ];
                    if let Some(trace) = trace_header {
                        headers.push(("x-rift-script-trace".to_string(), trace));
                    }
                    return Ok(build_response_with_headers(
                        StatusCode::OK,
                        headers,
                        Bytes::new(),
                    ));
                }
                Ok(FaultDecision::None) => {
                    // Script says no fault - return 200 OK
                    let mut headers = vec![
                        ("x-rift-imposter".to_string(), "true".to_string()),
                        ("x-rift-script".to_string(), engine.clone()),
                    ];
                    if let Some(trace) = trace_header {
                        headers.push(("x-rift-script-trace".to_string(), trace));
                    }
                    return Ok(build_response_with_headers(
                        StatusCode::OK,
                        headers,
                        Bytes::new(),
                    ));
                }
                Ok(FaultDecision::Reset { .. }) => {
                    // v2 `reset()` result constructor (issue #357 Item 3): a connection reset,
                    // applied the same real way as `_rift.fault.tcp` / a top-level `fault` (see
                    // `handle_fault_response`) — attach the parsed TcpFaultKind as a response
                    // extension; the serve loop's FaultIo aborts the connection before this
                    // carrier response is ever sent, so its status/body are never observed.
                    let mut headers = vec![
                        ("x-rift-imposter".to_string(), "true".to_string()),
                        ("x-rift-script".to_string(), engine.clone()),
                    ];
                    if let Some(trace) = trace_header {
                        headers.push(("x-rift-script-trace".to_string(), trace));
                    }
                    let mut response =
                        build_response_with_headers(StatusCode::BAD_GATEWAY, headers, Bytes::new());
                    response
                        .extensions_mut()
                        .insert(super::fault_io::TcpFaultKind::Reset);
                    return Ok(response);
                }
                Err(e) => {
                    log_upstream_failure("Rift script execution failed", &e);
                    // A deadline miss (issue #499) is a transient 504 + `x-rift-script-timeout`,
                    // distinct from the permanent 500 a broken script produces.
                    let timed_out = e
                        .downcast_ref::<crate::scripting::ScriptTimeoutError>()
                        .is_some();
                    let mut headers = vec![
                        ("x-rift-imposter".to_string(), "true".to_string()),
                        ("x-rift-script-error".to_string(), "true".to_string()),
                    ];
                    if timed_out {
                        headers.push((SCRIPT_TIMEOUT_HEADER.to_string(), "true".to_string()));
                    }
                    if let Some(trace) = trace_header {
                        headers.push(("x-rift-script-trace".to_string(), trace));
                    }
                    headers.push(("content-type".to_string(), "application/json".to_string()));
                    let (status, body) = if timed_out {
                        (
                            StatusCode::GATEWAY_TIMEOUT,
                            crate::response::error_body(
                                StatusCode::GATEWAY_TIMEOUT,
                                &format!("Script timeout: {e}"),
                            ),
                        )
                    } else {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            crate::response::error_body(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &format!("Script error: {e}"),
                            ),
                        )
                    };
                    return Ok(build_response_with_headers(status, headers, body));
                }
            }
        }

        if let Some((
            mut status,
            mut headers,
            mut body,
            behaviors,
            rift_ext,
            response_mode,
            is_fault,
        )) = response.and_then(execute_stub_response_with_rift)
        {
            // Handle faults - simulate connection errors
            if is_fault {
                return handle_fault_response(&body);
            }

            // Apply _rift.fault extensions (probabilistic faults)
            if let Some(rift) = rift_ext
                && let Some(ref fault_config) = rift.fault
                && let Some(response) = apply_rift_fault(fault_config, &mut status, &mut body).await
            {
                return Ok(response);
            }

            // Issue #375: strict mode makes a requested response behavior that fails serve a 500
            // (still carrying the #323 signal header) instead of the fallback body. Enabled
            // per-imposter (`strictBehaviors`) or process-wide (`RIFT_STRICT_BEHAVIORS`). Default
            // false preserves the lenient #269/#323 contract in the Err arms below.
            let strict_behaviors =
                imposter.config.strict_behaviors || crate::util::strict_behaviors_env();

            // Declarative response templating (issue #359): opt-in via `_rift.templated`. This
            // `{{ }}` render runs FIRST — on the *config-authored* body/headers — and BEFORE the
            // `${request.*}` reflection substitution below (issue #359 B1, security). Ordering is
            // load-bearing: because `${request.*}` injects reflected request data only *after* this
            // pass, any `{{ }}` that arrives inside reflected request data is never scanned or
            // evaluated here — it is served verbatim. Evaluating reflected `{{ }}` would be a
            // template-injection hole (an unauthenticated caller could reach `state.*`/force errors)
            // and would also break the module's "a literal `{{` is served verbatim" promise for
            // reflected text. Off by default so recorded fixtures with a literal `{{` are untouched.
            if rift_ext.is_some_and(|r| r.templated) {
                let request_data = RequestData::new(
                    method_str,
                    path_str,
                    query_opt,
                    &headers_for_context,
                    body_string.as_deref(),
                )
                .with_route_pattern(stub_state.stub.route_pattern.as_deref());
                // In debug mode (`RIFT_DEBUG`), a malformed/unknown/failed `{{ }}` token fails the
                // request loudly instead of silently degrading to an empty string (issue #359 AC3).
                let template_debug = crate::util::rift_debug_env();
                let template_ctx = crate::extensions::template_fn::TemplateContext {
                    request: &request_data,
                    flow_id: &scenario_flow_id,
                    flow_store: imposter.flow_store.as_ref(),
                };

                let template_error = match crate::extensions::template_fn::render_templated(
                    &body,
                    &template_ctx,
                    template_debug,
                ) {
                    Ok(rendered) => {
                        body = rendered;
                        None
                    }
                    Err(e) => Some(e),
                };
                let template_error = template_error.or_else(|| {
                    for values in headers.values_mut() {
                        for v in values.iter_mut() {
                            match crate::extensions::template_fn::render_templated(
                                v,
                                &template_ctx,
                                template_debug,
                            ) {
                                // Issue #359 B3 (header injection): a templated value can resolve to
                                // attacker-controlled request data containing CR/LF/control chars.
                                // Strip control characters before the value ever reaches the header
                                // map so it cannot inject an extra header line; warn (never silently)
                                // if anything had to be removed.
                                Ok(rendered) => *v = sanitize_header_value(&rendered),
                                Err(e) => return Some(e),
                            }
                        }
                    }
                    None
                });
                if let Some(e) = template_error {
                    warn!("Response template rendering failed: {e}");
                    return Ok(template_error_response(&e));
                }
            }

            // Expand `${request.*}` request templates (issue #269) BEFORE behaviors — matching the
            // proxy path's ordering so `shellTransform`/`decorate` operate on the expanded body.
            // Header values are templated too (the static path's AC1 requirement; the proxy path
            // templates only the body). Serve-time date templates ({{NOW}}/{{DAYS+N}}) are expanded
            // later, at body finalization. Runs AFTER the `{{ }}` pass above (issue #359 B1): this
            // pass only substitutes `${...}` and never re-scans for `{{ }}`, so reflected request
            // data injected here is never templated.
            {
                let need_body = has_template_variables(&body);
                let need_headers = headers
                    .values()
                    .flatten()
                    .any(|v| has_template_variables(v));
                if need_body || need_headers {
                    let request_data = RequestData::new(
                        method_str,
                        path_str,
                        query_opt,
                        &headers_for_context,
                        body_string.as_deref(),
                    )
                    .with_route_pattern(stub_state.stub.route_pattern.as_deref());
                    if need_body {
                        body = process_template(&body, &request_data);
                    }
                    for values in headers.values_mut() {
                        for v in values.iter_mut() {
                            if has_template_variables(v) {
                                *v = process_template(v, &request_data);
                            }
                        }
                    }
                }
            }

            // Apply behaviors if present. Issue #479: `behaviors` is now the precomputed
            // `Option<Arc<ResponseBehaviors>>` (parsed once at stub construction, see
            // `StubResponse::new_is`) — no more re-parsing `_behaviors` JSON on every request.
            if let Some(ref parsed_behaviors) = behaviors {
                // Apply wait behavior
                if let Some(ref wait) = parsed_behaviors.wait {
                    let wait_ms = wait.get_duration_ms();
                    if wait_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                    }
                }

                // Lazy request context (issue #561): only copy/lookup/decorate/shellTransform read
                // it, and `RequestContext::from_request` re-parses the query, retitles every
                // header, and copies the body — so a wait/repeat-only stub (or any non-`is`
                // response) must not pay for it.
                //
                // Built on first read rather than behind a hand-maintained "does anything below
                // need this?" predicate: such a predicate has to be kept in sync with consumers
                // 100+ lines away, and getting it wrong would hand one an empty-but-valid context —
                // wrong output, no error. Here a consumer that forgets to ask simply cannot exist.
                let request_context: OnceCell<RequestContext> = OnceCell::new();
                let build_request_context = || {
                    RequestContext::from_request(
                        &method,
                        &uri,
                        &headers_for_context,
                        body_string.as_deref(),
                    )
                };

                // copy/lookup are pure token substitution — apply them across each value of
                // multi-value headers so multiplicity survives (e.g. multiple Set-Cookie;
                // RFC 7230 §3.2.2 forbids folding Set-Cookie). decorate uses a single-value
                // JS/Rhai object model, so only that path collapses — and even there Set-Cookie
                // is held aside, never comma-folded.
                if !parsed_behaviors.copy.is_empty() {
                    body = apply_copy_behaviors(
                        &body,
                        &mut headers,
                        &parsed_behaviors.copy,
                        request_context.get_or_init(build_request_context),
                    );
                }
                if !parsed_behaviors.lookup.is_empty() {
                    body = apply_lookup_behaviors(
                        &body,
                        &mut headers,
                        &parsed_behaviors.lookup,
                        request_context.get_or_init(build_request_context),
                        csv_cache(),
                    );
                }
                if let Some(ref decorate_script) = parsed_behaviors.decorate {
                    // decorate uses a single-value JS/Rhai object model. Set-Cookie is held
                    // aside and never folded (RFC 7230 §3.2.2); other multi-value headers
                    // degrade to single-value for the script (issue #238 boundary) — warn so
                    // the collapse is not silent (e.g. WWW-Authenticate is also corrupted by
                    // comma-folding).
                    let is_set_cookie = |k: &str| k.eq_ignore_ascii_case("set-cookie");
                    let folded: Vec<&String> = headers
                        .iter()
                        .filter(|(k, v)| v.len() > 1 && !is_set_cookie(k))
                        .map(|(k, _)| k)
                        .collect();
                    if !folded.is_empty() {
                        warn!(
                            "decorate uses a single-value object model; multi-value headers \
                                 {folded:?} are comma-folded (issue #238 boundary). Set-Cookie is \
                                 exempt; other headers that forbid list-folding will be corrupted."
                        );
                    }

                    let set_cookie: Vec<(String, Vec<String>)> = headers
                        .iter()
                        .filter(|(k, _)| is_set_cookie(k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    let single: HashMap<String, String> = headers
                        .iter()
                        .filter(|(k, _)| !is_set_cookie(k))
                        .map(|(k, v)| (k.clone(), v.join(", ")))
                        .collect();
                    match apply_decorate_bounded(
                        decorate_script.clone(),
                        request_context.get_or_init(build_request_context).clone(),
                        body.clone(),
                        status,
                        single,
                        imposter.script_state_key(),
                        stub_state.stub.id.clone(),
                        script_timeout,
                    )
                    .await
                    {
                        Ok((new_body, new_status, single)) => {
                            body = new_body;
                            status = new_status;
                            // Restore the held-aside Set-Cookie lines unless the script set its
                            // own (case-insensitively) — a script override wins deterministically.
                            let script_set_cookie = single.keys().any(|k| is_set_cookie(k));
                            headers = single.into_iter().map(|(k, v)| (k, vec![v])).collect();
                            if !script_set_cookie {
                                headers.extend(set_cookie);
                            }
                        }
                        // Behave as if decorate was absent: keep the original multi-value
                        // `headers` and pre-decorate body/status rather than serving a folded,
                        // undecorated response. Attach a visible signal so the skipped behavior
                        // isn't a silent success (issue #323); the body is still served (#269).
                        Err(e) => {
                            warn!("Decorate script error: {e}");
                            // A deadline miss (issue #499) carries `x-rift-script-timeout` and,
                            // under strict mode, a 504 rather than the broken-script 500 — so a
                            // retry-worthy timeout is distinguishable from a permanent failure.
                            let timed_out =
                                matches!(e, crate::behaviors::DecorateError::Timeout(_));
                            if strict_behaviors {
                                let status = if timed_out {
                                    StatusCode::GATEWAY_TIMEOUT
                                } else {
                                    StatusCode::INTERNAL_SERVER_ERROR
                                };
                                let mut hdrs = vec![
                                    ("x-rift-imposter", "true"),
                                    ("x-rift-decorate-error", "true"),
                                    ("content-type", "application/json"),
                                ];
                                if timed_out {
                                    hdrs.push((SCRIPT_TIMEOUT_HEADER, "true"));
                                }
                                return Ok(build_response_with_headers(
                                    status,
                                    hdrs,
                                    crate::response::error_body(
                                        status,
                                        &format!("decorate failed (strictBehaviors): {e}"),
                                    ),
                                ));
                            }
                            headers.insert(
                                "x-rift-decorate-error".to_string(),
                                vec!["true".to_string()],
                            );
                            if timed_out {
                                headers.insert(
                                    SCRIPT_TIMEOUT_HEADER.to_string(),
                                    vec!["true".to_string()],
                                );
                            }
                        }
                    }
                }

                // shellTransform (issue #269): pipe the body through external command(s);
                // stdout becomes the new body. Runs on the static `is` path too, not only the
                // proxy path, and independently of copy/lookup/decorate.
                for cmd in &parsed_behaviors.shell_transform {
                    // Run the fork/exec/wait off the tokio worker (issue #478): a synchronous
                    // subprocess run inline would stall the worker for its whole lifetime,
                    // starving unrelated requests multiplexed on it.
                    let shell_result = {
                        let cmd = cmd.clone();
                        let rc = request_context.get_or_init(build_request_context).clone();
                        let body_in = body.clone();
                        tokio::task::spawn_blocking(move || {
                            apply_shell_transform(&cmd, &rc, &body_in, status)
                        })
                        .await
                        .unwrap_or_else(|e| {
                            Err(std::io::Error::other(format!(
                                "shellTransform task panicked: {e}"
                            )))
                        })
                    };
                    match shell_result {
                        Ok(transformed) => body = transformed,
                        // Keep the body unchanged (issue #269) but signal the failure so it
                        // isn't a silent success (issue #323).
                        Err(e) => {
                            warn!("shellTransform command {cmd:?} failed: {e}");
                            if strict_behaviors {
                                return Ok(build_response_with_headers(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    [
                                        ("x-rift-imposter", "true"),
                                        ("x-rift-shelltransform-error", "true"),
                                        ("content-type", "application/json"),
                                    ],
                                    crate::response::error_body(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        &format!("shellTransform failed (strictBehaviors): {e}"),
                                    ),
                                ));
                            }
                            headers.insert(
                                "x-rift-shelltransform-error".to_string(),
                                vec!["true".to_string()],
                            );
                        }
                    }
                }
            }
            let mut response = Response::builder().status(status);

            // One header line per value (issue #238 multi-value headers, e.g. multiple Set-Cookie).
            for (k, values) in &headers {
                for v in values {
                    response = response.header(k, v);
                }
            }

            response = response.header("x-rift-imposter", "true");

            // Handle binary mode - decode base64 body if _mode is "binary"
            let mut binary_decode_failed = false;
            let body_bytes = match response_mode {
                ResponseMode::Binary => {
                    // Decode base64-encoded body
                    match base64::engine::general_purpose::STANDARD.decode(&body) {
                        Ok(decoded) => Bytes::from(decoded),
                        Err(e) => {
                            if strict_behaviors {
                                warn!(
                                    "Failed to decode base64 body: {e}; failing loud (strictBehaviors)"
                                );
                                return Ok(build_response_with_headers(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    [
                                        ("x-rift-imposter", "true"),
                                        ("x-rift-binary-error", "true"),
                                        ("content-type", "application/json"),
                                    ],
                                    crate::response::error_body(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        &format!(
                                            "binary base64 decode failed (strictBehaviors): {e}"
                                        ),
                                    ),
                                ));
                            }
                            warn!("Failed to decode base64 body: {e}, using raw body");
                            binary_decode_failed = true;
                            Bytes::from(body)
                        }
                    }
                }
                // Expand serve-time date templates ({{DAYS+N}}/{{MONTHS+N}}/{{NOW}}, issue #195).
                ResponseMode::Text => Bytes::from(crate::extensions::apply_date_templates(&body)),
            };
            // Signal a failed binary decode so serving the raw (still-encoded) body isn't silent (#323).
            if binary_decode_failed {
                response = response.header("x-rift-binary-error", "true");
            }

            return Ok(response.body(Full::new(body_bytes)).unwrap_or_else(|e| {
                build_failure_response(&e, "stub response build failed (bad stub header?)")
            }));
        }
    }

    // No matching rule. Issue #196: if `defaultForward` is set, transparently forward the
    // request to the configured upstream before falling back to a static default. A
    // defaultForward-only imposter runs in ProxyTransparent mode, so the upstream response is
    // never cached/replayed. (The request still appears in the audit log when `recordRequests`
    // is enabled — that is the separate, opt-in recording feature.)
    if let Some(upstream) = &imposter.config.default_forward {
        let proxy_config = ProxyResponse {
            to: upstream.clone(),
            ..Default::default()
        };
        return match imposter
            .handle_proxy_request(
                &proxy_config,
                method_str,
                &uri,
                &headers_clone,
                body_string.as_deref(),
            )
            .await
        {
            Ok((status, response_headers, body, _latency)) => {
                let mut response = Response::builder().status(status);
                for (k, v) in &response_headers {
                    if !crate::util::is_hop_by_hop_header(k) {
                        response = response.header(k, v);
                    }
                }
                response = response.header("x-rift-imposter", "true");
                response = response.header("x-rift-default-forward", "true");
                Ok(response
                    .body(Full::new(Bytes::from(body)))
                    .unwrap_or_else(|e| {
                        build_failure_response(
                            &e,
                            "defaultForward response build failed (bad upstream header?)",
                        )
                    }))
            }
            Err(e) => Ok(upstream_error_response(
                &e,
                &format!("defaultForward proxy to {upstream} failed"),
                "x-rift-default-forward-error",
                "defaultForward upstream error",
            )),
        };
    }

    // No matching rule — return the configured `defaultResponse`, else fall through to a 200
    // with an empty body below (Mountebank parity — Rift never returns 404 for an unmatched
    // request). A `defaultForward` upstream, if configured, was already handled above.
    if let Some(ref default) = imposter.config.default_response {
        let body_str = default
            .body
            .as_ref()
            .map(|b| {
                if b.is_string() {
                    b.as_str().unwrap_or("").to_string()
                } else {
                    // Serializing a `serde_json::Value` is infallible by construction (map keys
                    // are strings, numbers are finite), so this never defaults (issue #611).
                    serde_json::to_string(b).unwrap_or_default()
                }
            })
            .unwrap_or_default();

        // Handle binary mode for default response
        let body_bytes = match default.mode {
            ResponseMode::Binary => {
                match base64::engine::general_purpose::STANDARD.decode(&body_str) {
                    Ok(decoded) => Bytes::from(decoded),
                    Err(e) => {
                        warn!(
                            "Failed to decode base64 default body: {}, using raw body",
                            e
                        );
                        Bytes::from(body_str)
                    }
                }
            }
            ResponseMode::Text => Bytes::from(crate::extensions::apply_date_templates(&body_str)),
        };

        let mut response = Response::builder().status(default.status_code);
        for (k, values) in &default.headers {
            for v in values {
                response = response.header(k, v);
            }
        }
        response = response.header("x-rift-imposter", "true");
        response = response.header("x-rift-default-response", "true");

        return Ok(response.body(Full::new(body_bytes)).unwrap_or_else(|e| {
            build_failure_response(&e, "defaultResponse build failed (bad default header?)")
        }));
    }

    // No match and no default - Mountebank returns 200 with empty body
    Ok(build_response_with_headers(
        StatusCode::OK,
        [("x-rift-imposter", "true"), ("x-rift-no-match", "true")],
        Bytes::new(),
    ))
}

/// Handle debug mode request
#[allow(clippy::too_many_arguments)]
fn handle_debug_request(
    imposter: &Arc<Imposter>,
    method: &str,
    path: &str,
    query_str: &str,
    headers_clone: &HashMap<String, String>,
    body_string: &Option<String>,
    client_addr: SocketAddr,
) -> Result<Response<Full<Bytes>>, Infallible> {
    debug!("Debug mode enabled for request {} {}", method, path);

    // Build debug request info
    let debug_request = DebugRequest {
        method: method.to_string(),
        path: path.to_string(),
        query: if query_str.is_empty() {
            None
        } else {
            Some(query_str.to_string())
        },
        headers: headers_clone
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("x-rift-debug"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        body: body_string.clone(),
    };

    // Get imposter info
    let debug_imposter = imposter.get_debug_imposter_info();

    // Find matching stub for debug info (with client address)
    let request_from = client_addr.to_string();
    let client_ip = client_addr.ip().to_string();
    let query_opt = if query_str.is_empty() {
        None
    } else {
        Some(query_str)
    };

    let matched = match imposter.find_matching_stub_with_client(
        method,
        path,
        headers_clone,
        query_opt,
        body_string.as_deref(),
        Some(&request_from),
        Some(&client_ip),
    ) {
        Ok(matched) => matched,
        Err(e) => return Ok(matcher_error_response(&e)),
    };
    let match_result = if let Some((stub_state, stub_index)) = matched {
        // Match found
        let response_preview = match imposter.get_response_preview(&stub_state) {
            Ok(preview) => preview,
            Err(e) => return Ok(backend_error_response(&e)),
        };
        DebugMatchResult {
            matched: true,
            stub_index: Some(stub_index),
            stub_id: stub_state.stub.id.clone(),
            predicates: Some(stub_state.stub.predicates.clone()),
            response_preview: Some(response_preview),
            all_stubs: None,
            reason: None,
        }
    } else {
        // No match - return all stubs for inspection
        let all_stubs = imposter.get_all_stubs_info();
        let reason = if all_stubs.is_empty() {
            "No stubs configured for this imposter".to_string()
        } else {
            "No stub predicates matched the request".to_string()
        };
        DebugMatchResult {
            matched: false,
            stub_index: None,
            stub_id: None,
            predicates: None,
            response_preview: None,
            all_stubs: Some(all_stubs),
            reason: Some(reason),
        }
    };

    let debug_response = DebugResponse {
        debug: true,
        request: debug_request,
        imposter: debug_imposter,
        match_result,
    };

    let (status, json_body) = debug_serialize_or_500(&debug_response);

    Ok(build_response_with_headers(
        status,
        [
            ("Content-Type", "application/json"),
            ("X-Rift-Debug-Response", "true"),
        ],
        json_body,
    ))
}

/// Serialize a debug payload into the `(status, body)` the debug endpoint should answer with.
/// Split out from `handle_debug_request` so the serde error path — unreachable with the real
/// `DebugResponse` — can be pinned by a test (issue #611).
fn debug_serialize_or_500<T: serde::Serialize>(payload: &T) -> (StatusCode, String) {
    match serde_json::to_string_pretty(payload) {
        Ok(body) => (StatusCode::OK, body),
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize debug response");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::response::error_body(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to serialize debug response",
                ),
            )
        }
    }
}

/// Handle a top-level `fault` response type. A Mountebank `fault` is a transport-level event, so
/// it reuses the real `_rift.fault.tcp` path (issue #309): the parsed [`TcpFaultKind`] is attached
/// as a response extension that the serve loop's `FaultIo` applies as a real reset/close/etc.,
/// instead of framing a clean HTTP 502 (which a migrated Mountebank config would not expect).
fn handle_fault_response(fault_type: &str) -> Result<Response<Full<Bytes>>, Infallible> {
    if let Some(kind) = super::fault_io::TcpFaultKind::parse(fault_type) {
        // Carrier response: `FaultIo` aborts the connection before this is sent, so the
        // status/body here are never observed by the client (mirrors the `_rift.fault.tcp` path).
        let mut response = build_response_with_headers(
            StatusCode::BAD_GATEWAY,
            [("x-rift-fault", fault_type)],
            Bytes::new(),
        );
        response.extensions_mut().insert(kind);
        return Ok(response);
    }
    // Unrecognized fault type: a defined error, never a silent connection drop.
    Ok(build_response_with_headers(
        StatusCode::INTERNAL_SERVER_ERROR,
        [("x-rift-fault", fault_type)],
        format!("Unknown fault: {fault_type}"),
    ))
}

/// Apply Rift fault configuration (probabilistic faults)
async fn apply_rift_fault(
    fault_config: &super::types::RiftFaultConfig,
    _status: &mut u16,
    _body: &mut String,
) -> Option<Response<Full<Bytes>>> {
    // Generate all random values before any await points (ThreadRng is not Send)
    let (apply_latency, latency_delay_ms) = {
        let mut rng = rand::thread_rng();
        if let Some(ref latency) = fault_config.latency {
            if rng.r#gen::<f64>() < latency.probability {
                let delay_ms = if let Some(fixed_ms) = latency.ms {
                    fixed_ms
                } else if latency.max_ms > latency.min_ms {
                    rng.gen_range(latency.min_ms..=latency.max_ms)
                } else {
                    latency.min_ms
                };
                (true, delay_ms)
            } else {
                (false, 0)
            }
        } else {
            (false, 0)
        }
    };

    let apply_error = {
        let mut rng = rand::thread_rng();
        if let Some(ref error) = fault_config.error {
            rng.r#gen::<f64>() < error.probability
        } else {
            false
        }
    };

    // A `tcp` fault fires with its own probability (issue #531): the bare string form is always
    // 1.0; the object form carries a chosen probability. When the roll fails the fault is treated
    // as absent for this request, falling through to the `error` fault and normal response — the
    // same semantics `latency`/`error` already use.
    let apply_tcp = {
        let mut rng = rand::thread_rng();
        fault_config
            .tcp
            .as_ref()
            .is_some_and(|tcp| rng.r#gen::<f64>() < tcp.probability())
    };

    // Apply latency fault (this is async)
    if apply_latency && latency_delay_ms > 0 {
        debug!("Applying _rift.fault latency: {}ms", latency_delay_ms);
        tokio::time::sleep(Duration::from_millis(latency_delay_ms)).await;
    }

    // Check for TCP fault before the HTTP error fault. A `tcp` fault is a transport-level event:
    // the connection is reset before any HTTP response can be written, so it must win over an
    // `error` fault — otherwise a configured `tcp` is silently dropped whenever `error` also fires
    // (issue #271). Latency above still applies first, keeping delay-then-drop coherent.
    if apply_tcp && let Some(ref tcp_fault) = fault_config.tcp {
        if let Some(kind) = super::fault_io::TcpFaultKind::parse(tcp_fault.kind()) {
            debug!("Applying _rift.fault.tcp: {:?}", kind);
            if apply_error {
                debug!("_rift.fault.error preempted by tcp fault (connection reset)");
            }
            // Carrier response: the serve loop's `FaultIo` aborts the connection before this is
            // ever sent. The `TcpFaultKind` extension tells it which real transport fault to apply
            // (issue #239) — the status/body here are never observed by the client.
            let mut response = build_response_with_headers(
                StatusCode::BAD_GATEWAY,
                [("x-rift-fault", tcp_fault.kind())],
                Bytes::new(),
            );
            response.extensions_mut().insert(kind);
            return Some(response);
        }
        warn!("Unknown TCP fault type: {}", tcp_fault.kind());
    }

    // Apply error fault
    if apply_error && let Some(ref error) = fault_config.error {
        debug!("Applying _rift.fault error: status {}", error.status);

        let mut response = Response::builder().status(error.status);

        // Apply custom headers
        for (k, v) in &error.headers {
            response = response.header(k, v);
        }

        response = response.header("x-rift-imposter", "true");
        response = response.header("x-rift-fault", "error");

        let error_body = error.body.clone().unwrap_or_default();
        return Some(
            response
                .body(Full::new(Bytes::from(error_body)))
                .unwrap_or_else(|e| {
                    build_failure_response(
                        &e,
                        "error-fault response build failed (bad fault header?)",
                    )
                }),
        );
    }

    None
}

// Issue #500: `matcher_error_response` splits a matcher failure two ways — a predicate-`inject`
// error (`PredicateInjectionError`) is a Mountebank-shaped 400, while every other matcher error
// (e.g. a scenario-state backend failure) keeps the 5xx backend mapping. The end-to-end handler
// tests exercise the 400 branch over the wire; this pins the "other error → 5xx" branch directly,
// since it needs a non-predicate error that a listener test can't easily provoke.
#[cfg(all(test, feature = "javascript"))]
mod matcher_error_response_tests {
    use super::{inject_timeout_response, matcher_error_response};
    use hyper::StatusCode;

    // Issue #499: the shared builder for a timed-out response/predicate `inject` — a 504 with the
    // Mountebank error envelope (timeout-specific code) plus the inject-error + script-timeout
    // markers. The Boa `inject` timeout paths route through this; they are unit-tested here rather
    // than end-to-end because a real Boa busy-loop parks a shared-pool worker (see the note in
    // tests/handler_error_responses.rs).
    #[tokio::test]
    async fn inject_timeout_response_is_504_with_code_and_markers() {
        use http_body_util::BodyExt;
        let resp = inject_timeout_response("injection timeout", "inject timed out after 1ms");
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(resp.headers().contains_key("x-rift-inject-error"));
        assert!(
            resp.headers().contains_key("x-rift-script-timeout"),
            "the timeout marker distinguishes a deadline miss from a broken inject"
        );
        // Issue #687: this door's 400 sibling (`matcher_error_response`) declares the envelope it
        // serves, so the 504 must too — the two are one contract, reached by one predicate failing
        // two different ways.
        assert_eq!(
            resp.headers()["content-type"],
            "application/json",
            "the inject-timeout 504 serves the JSON envelope, so it must declare it"
        );
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let body = String::from_utf8(bytes.to_vec()).expect("utf8");
        assert!(
            body.contains("\"code\":\"injection timeout\""),
            "body must carry the timeout-specific code, got: {body}"
        );
    }

    #[test]
    fn predicate_inject_error_maps_to_400() {
        let err: anyhow::Error =
            crate::scripting::PredicateInjectionError("bad predicate".to_string()).into();
        let resp = matcher_error_response(&err);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(
            resp.headers().contains_key("x-rift-inject-error"),
            "a predicate-inject error must carry the inject-error marker"
        );
    }

    #[test]
    fn other_matcher_error_maps_to_5xx() {
        let err = anyhow::anyhow!("scenario-state backend read failed");
        let resp = matcher_error_response(&err);
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a non-predicate matcher error keeps the 5xx backend mapping, not a 400"
        );
        assert!(
            !resp.headers().contains_key("x-rift-inject-error"),
            "a generic backend error is not an inject error"
        );
    }

    // Issue #499: a predicate-matching deadline is a 504 with the timeout marker, distinct from
    // the broken-predicate 400 above — so a client can tell a retry-worthy timeout apart.
    #[test]
    fn predicate_inject_timeout_maps_to_504() {
        let err: anyhow::Error = crate::scripting::ScriptTimeoutError {
            hook: "predicate inject",
            timeout_ms: 50,
        }
        .into();
        let resp = matcher_error_response(&err);
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(
            resp.headers().contains_key("x-rift-script-timeout"),
            "a matching timeout must carry the script-timeout marker"
        );
        assert!(
            resp.headers().contains_key("x-rift-inject-error"),
            "a predicate-inject timeout keeps the inject-error marker"
        );
    }
}

#[cfg(test)]
mod template_error_tests {
    use super::template_error_response;
    use http_body_util::BodyExt;
    use hyper::StatusCode;

    // Issue #687: the template door fires only under `RIFT_DEBUG` (a process-global this binary's
    // parallel tests cannot toggle safely), so its contract is pinned here instead of end-to-end.
    #[tokio::test]
    async fn template_error_response_is_500_with_marker_and_json_envelope() {
        let resp = template_error_response("template error in `{{ nope }}`: unknown token");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(resp.headers().contains_key("x-rift-template-error"));
        assert_eq!(
            resp.headers()["content-type"],
            "application/json",
            "the template door serves the JSON envelope, so it must declare it"
        );
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let body = String::from_utf8(bytes.to_vec()).expect("utf8");
        let v: serde_json::Value = serde_json::from_str(&body).expect("body must be valid JSON");
        assert_eq!(v["errors"][0]["code"], "500", "envelope code is the status");
        assert!(
            v["errors"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("template rendering failed")),
            "the 500 must name the render failure, got: {body}"
        );
    }
}

#[cfg(test)]
mod body_collect_tests {
    use super::{
        BodyReadError, MAX_REQUEST_BODY_SIZE, body_read_error_response, collect_body_limited,
    };
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::StatusCode;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tracing_test::traced_test;

    // A body that fails mid-read — the shape of a connection reset or a truncated chunked stream,
    // distinct from a body that merely exceeds the size cap.
    struct FailingBody;
    impl hyper::body::Body for FailingBody {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
            Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ))))
        }
    }

    #[tokio::test]
    async fn under_limit_returns_the_bytes() {
        let got = collect_body_limited(Full::new(Bytes::from_static(b"hello")), 4096)
            .await
            .expect("under limit");
        assert_eq!(got, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn at_the_cap_boundary_is_accepted() {
        // Exactly at the limit is not over it — guards the `>` vs `>=` off-by-one in the cap check.
        let got = collect_body_limited(Full::new(Bytes::from(vec![b'x'; 4096])), 4096)
            .await
            .expect("a body exactly at the cap must be accepted");
        assert_eq!(got.len(), 4096);
    }

    #[tokio::test]
    async fn oversize_is_flagged_too_large_not_read() {
        let body = Full::new(Bytes::from(vec![b'x'; 4097]));
        let err = collect_body_limited(body, 4096)
            .await
            .expect_err("over limit");
        assert!(
            matches!(err, BodyReadError::TooLarge),
            "a size-cap hit must be TooLarge, not a Read"
        );
    }

    // Issue #694: a transport failure must be classified Read — not silently folded into the 413
    // cap door — and the boxed cause preserved (not stringified) so the log can name it.
    #[tokio::test]
    async fn transport_failure_is_flagged_read_with_cause_preserved() {
        let err = collect_body_limited(FailingBody, MAX_REQUEST_BODY_SIZE)
            .await
            .expect_err("transport failure");
        match err {
            BodyReadError::Read(e) => assert!(
                e.to_string().contains("connection reset"),
                "the underlying cause must survive to the sink, got: {e}"
            ),
            BodyReadError::TooLarge => panic!("a transport failure is not a size-cap hit"),
        }
    }

    #[traced_test]
    #[tokio::test]
    async fn read_error_door_is_400_envelope_and_logs_the_cause() {
        let e = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "truncated chunked body");
        let resp = body_read_error_response(&e);
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "a read failure is the client's 400, not the 413 cap door nor a 500"
        );
        assert_eq!(resp.headers()["content-type"], "application/json");
        assert!(resp.headers().contains_key("x-rift-imposter"));
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON envelope");
        assert_eq!(v["errors"][0]["code"], "400", "envelope code is the status");
        assert!(
            v["errors"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("Failed to read request body")),
            "got: {v}"
        );
        assert!(
            logs_contain("truncated chunked body"),
            "the previously-silent read failure must now be logged with its cause"
        );
    }
}

#[cfg(test)]
mod plaintext_door_tests {
    use super::{
        SCRIPT_TIMEOUT_HEADER, build_failure_response, debug_matching_error_response,
        debug_matching_timeout_response,
    };
    use http_body_util::BodyExt;
    use hyper::StatusCode;
    use tracing_test::traced_test;

    async fn envelope(
        resp: hyper::Response<http_body_util::Full<bytes::Bytes>>,
    ) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("body must be the canonical JSON envelope")
    }

    // Issue #695: the debug endpoint's success path already answers JSON (`X-Rift-Debug-Response`),
    // so its error path must too — plain text was the same-endpoint gap #687 closed elsewhere.
    #[tokio::test]
    async fn debug_matching_error_door_is_500_envelope() {
        let resp = debug_matching_error_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(resp.headers()["content-type"], "application/json");
        assert!(resp.headers().contains_key("x-rift-imposter"));
        let v = envelope(resp).await;
        assert_eq!(v["errors"][0]["code"], "500");
        assert!(
            v["errors"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("Debug matching failed")),
            "got: {v}"
        );
    }

    // The timeout door is a miss of the same `_rift.scriptEngine.timeoutMs` budget every other
    // script deadline maps to 504 + the timeout marker (#499/#476) — not the 500 it used to answer.
    #[tokio::test]
    async fn debug_matching_timeout_door_is_504_with_timeout_marker() {
        let resp = debug_matching_timeout_response();
        assert_eq!(
            resp.status(),
            StatusCode::GATEWAY_TIMEOUT,
            "a debug-matching deadline miss is a 504, like every other script timeout"
        );
        assert!(
            resp.headers().contains_key(SCRIPT_TIMEOUT_HEADER),
            "the timeout marker distinguishes a deadline miss from a permanent failure"
        );
        assert_eq!(resp.headers()["content-type"], "application/json");
        assert!(resp.headers().contains_key("x-rift-imposter"));
        let v = envelope(resp).await;
        assert_eq!(
            v["errors"][0]["code"], "504",
            "envelope code tracks the status"
        );
        assert!(
            v["errors"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("Debug matching timed out")),
            "got: {v}"
        );
    }

    // The terminal fallback shared by all seven response-build sites: it must answer the canonical
    // envelope (not a bare string) and still log the caller's context so the failing door is named.
    #[traced_test]
    #[tokio::test]
    async fn build_failure_response_is_500_envelope_and_logs_context() {
        let e = hyper::Response::builder()
            .header("x-bad", "line1\nline2")
            .body(())
            .expect_err("an invalid header value must be a build error");
        let resp = build_failure_response(&e, "stub response build failed (bad stub header?)");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(resp.headers()["content-type"], "application/json");
        assert!(resp.headers().contains_key("x-rift-imposter"));
        let v = envelope(resp).await;
        assert_eq!(v["errors"][0]["code"], "500");
        assert!(
            v["errors"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("Response build error")),
            "got: {v}"
        );
        assert!(
            logs_contain("stub response build failed"),
            "the failing door's context must still be logged"
        );
    }
}

#[cfg(test)]
mod debug_serialize_tests {
    use super::debug_serialize_or_500;
    use hyper::StatusCode;
    use serde::{Serialize, Serializer};

    /// A payload whose `Serialize` always fails — the only way to drive serde's error path, since
    /// the real `DebugResponse` is plain structs of strings and JSON values (issue #611, mirroring
    /// the #606 technique in `admin_api/types.rs`).
    struct Unserializable;
    impl Serialize for Unserializable {
        fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("nope"))
        }
    }

    // Issue #611: a serialize failure used to be answered as `200 OK` carrying an error string —
    // the #606 shape one endpoint over. It is a server fault and must be a 500.
    #[test]
    fn debug_serialize_failure_maps_to_500() {
        let (status, body) = debug_serialize_or_500(&Unserializable);
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "a serialize failure must be a 500, not a 200 carrying an error string"
        );
        // Issue #682: the body is the canonical envelope now, not a hand-built `{"error"}` literal.
        let v: serde_json::Value = serde_json::from_str(&body).expect("body must be valid JSON");
        assert_eq!(v["errors"][0]["code"], "500", "envelope code is the status");
    }

    #[test]
    fn debug_serialize_success_keeps_200_and_the_payload() {
        let (status, body) = debug_serialize_or_500(&serde_json::json!({"debug": true}));
        assert_eq!(status, StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(parsed["debug"], serde_json::json!(true));
    }
}

#[cfg(test)]
mod fault_precedence_tests {
    use super::super::fault_io::TcpFaultKind;
    use super::super::types::{RiftErrorFault, RiftFaultConfig, RiftLatencyFault, RiftTcpFault};
    use super::{Bytes, Full, Response, apply_rift_fault, handle_fault_response};
    use std::time::Instant;

    // Issue #309: a top-level `fault` response must reset/close the connection (via the same
    // TcpFaultKind extension the serve loop's FaultIo reads for `_rift.fault.tcp`), not return a
    // framed HTTP 502. Before the fix `handle_fault_response` attached no extension.
    #[test]
    fn top_level_fault_carries_real_tcp_fault_extension() {
        let reset = handle_fault_response("CONNECTION_RESET_BY_PEER").expect("infallible");
        assert_eq!(
            reset.extensions().get::<TcpFaultKind>().copied(),
            Some(TcpFaultKind::Reset),
            "CONNECTION_RESET_BY_PEER must carry a real TCP-reset fault, not a plain 502"
        );

        let random = handle_fault_response("RANDOM_DATA_THEN_CLOSE").expect("infallible");
        assert_eq!(
            random.extensions().get::<TcpFaultKind>().copied(),
            Some(TcpFaultKind::RandomData),
            "RANDOM_DATA_THEN_CLOSE must carry a real random-data-then-close fault"
        );

        // The two remaining WireMock kinds also route to a real transport fault (unified on the
        // same parser as `_rift.fault.tcp`), no longer falling into the 500 "Unknown fault" branch.
        let empty = handle_fault_response("EMPTY_RESPONSE").expect("infallible");
        assert_eq!(
            empty.extensions().get::<TcpFaultKind>().copied(),
            Some(TcpFaultKind::Empty)
        );
        let malformed = handle_fault_response("MALFORMED_RESPONSE_CHUNK").expect("infallible");
        assert_eq!(
            malformed.extensions().get::<TcpFaultKind>().copied(),
            Some(TcpFaultKind::MalformedChunk)
        );

        // An unrecognized fault type stays a defined error, not a silent transport fault.
        let unknown = handle_fault_response("NOT_A_FAULT").expect("infallible");
        assert!(unknown.extensions().get::<TcpFaultKind>().is_none());
    }

    fn error_fault(status: u16) -> RiftErrorFault {
        RiftErrorFault {
            probability: 1.0,
            status,
            body: None,
            headers: Default::default(),
        }
    }

    fn latency_fault(ms: u64) -> RiftLatencyFault {
        RiftLatencyFault {
            probability: 1.0,
            min_ms: 0,
            max_ms: 0,
            ms: Some(ms),
        }
    }

    async fn apply(config: &RiftFaultConfig) -> Response<Full<Bytes>> {
        let mut status = 200;
        let mut body = String::new();
        apply_rift_fault(config, &mut status, &mut body)
            .await
            .expect("a fault response")
    }

    /// A configured `tcp` fault must win over a certain `error` fault: the response is the TCP
    /// carrier (parsed `TcpFaultKind` in extensions), never the HTTP error status (issue #271).
    #[tokio::test]
    async fn tcp_takes_precedence_over_error() {
        let config = RiftFaultConfig {
            latency: Some(latency_fault(0)),
            error: Some(error_fault(503)),
            tcp: Some(RiftTcpFault::Kind("CONNECTION_RESET_BY_PEER".to_string())),
        };
        let response = apply(&config).await;

        assert_eq!(
            response.extensions().get::<TcpFaultKind>().copied(),
            Some(TcpFaultKind::Reset),
            "tcp carrier must drive the connection reset"
        );
        assert_ne!(
            response.status(),
            503,
            "error fault must not win over the tcp fault"
        );
        assert_eq!(
            response.headers().get("x-rift-fault").unwrap(),
            "CONNECTION_RESET_BY_PEER"
        );
    }

    /// With no `tcp` fault, a certain `error` fault still produces the HTTP error response.
    #[tokio::test]
    async fn error_fault_applies_when_no_tcp() {
        let config = RiftFaultConfig {
            latency: Some(latency_fault(0)),
            error: Some(error_fault(503)),
            tcp: None,
        };
        let response = apply(&config).await;

        assert_eq!(response.status(), 503);
        assert_eq!(response.headers().get("x-rift-fault").unwrap(), "error");
        assert!(response.extensions().get::<TcpFaultKind>().is_none());
    }

    /// An unparseable `tcp` string must not swallow a configured `error` fault: it warns and falls
    /// through to the HTTP error response (guards the path most adjacent to the issue #271 drop).
    #[tokio::test]
    async fn unparseable_tcp_falls_through_to_error() {
        let config = RiftFaultConfig {
            latency: None,
            error: Some(error_fault(503)),
            tcp: Some(RiftTcpFault::Kind("NONSENSE".to_string())),
        };
        let response = apply(&config).await;

        assert_eq!(response.status(), 503);
        assert_eq!(response.headers().get("x-rift-fault").unwrap(), "error");
        assert!(response.extensions().get::<TcpFaultKind>().is_none());
    }

    /// A `tcp` fault on its own still resets (regression guard).
    #[tokio::test]
    async fn tcp_fault_alone_resets() {
        let config = RiftFaultConfig {
            latency: None,
            error: None,
            tcp: Some(RiftTcpFault::Kind("CONNECTION_RESET_BY_PEER".to_string())),
        };
        let response = apply(&config).await;

        assert_eq!(
            response.extensions().get::<TcpFaultKind>().copied(),
            Some(TcpFaultKind::Reset)
        );
    }

    /// Latency is still applied before the tcp fault wins (delay-then-drop stays coherent).
    #[tokio::test]
    async fn latency_applies_before_tcp() {
        let config = RiftFaultConfig {
            latency: Some(latency_fault(60)),
            error: Some(error_fault(503)),
            tcp: Some(RiftTcpFault::Kind("CONNECTION_RESET_BY_PEER".to_string())),
        };
        let start = Instant::now();
        let response = apply(&config).await;

        assert!(
            start.elapsed().as_millis() >= 50,
            "latency must be applied before the tcp reset"
        );
        assert_eq!(
            response.extensions().get::<TcpFaultKind>().copied(),
            Some(TcpFaultKind::Reset)
        );
    }

    /// Issue #531: a `tcp` fault with `probability: 0.0` must never reset — with no other fault it
    /// falls through to the normal response (no fault response at all).
    #[tokio::test]
    async fn tcp_object_probability_zero_never_resets() {
        let config = RiftFaultConfig {
            latency: None,
            error: None,
            tcp: Some(RiftTcpFault::Probabilistic {
                probability: 0.0,
                kind: "CONNECTION_RESET_BY_PEER".to_string(),
            }),
        };
        let (mut status, mut body) = (200, String::new());
        for _ in 0..200 {
            let response = apply_rift_fault(&config, &mut status, &mut body).await;
            assert!(
                response.is_none(),
                "probability 0.0 tcp fault must never fire"
            );
        }
    }

    /// A `probability: 0.0` tcp fault falls through to a certain `error` fault (treated as absent).
    #[tokio::test]
    async fn tcp_object_probability_zero_falls_through_to_error() {
        let config = RiftFaultConfig {
            latency: None,
            error: Some(error_fault(503)),
            tcp: Some(RiftTcpFault::Probabilistic {
                probability: 0.0,
                kind: "CONNECTION_RESET_BY_PEER".to_string(),
            }),
        };
        let response = apply(&config).await;
        assert_eq!(response.status(), 503);
        assert!(response.extensions().get::<TcpFaultKind>().is_none());
    }

    /// The object form with `probability: 1.0` behaves exactly like the bare string.
    #[tokio::test]
    async fn tcp_object_probability_one_matches_bare_string() {
        let config = RiftFaultConfig {
            latency: None,
            error: Some(error_fault(503)),
            tcp: Some(RiftTcpFault::Probabilistic {
                probability: 1.0,
                kind: "CONNECTION_RESET_BY_PEER".to_string(),
            }),
        };
        let response = apply(&config).await;
        assert_eq!(
            response.extensions().get::<TcpFaultKind>().copied(),
            Some(TcpFaultKind::Reset)
        );
        assert_ne!(response.status(), 503);
        assert_eq!(
            response.headers().get("x-rift-fault").unwrap(),
            "CONNECTION_RESET_BY_PEER"
        );
    }

    /// Statistical gate: a `probability: 0.3` tcp fault fires at roughly the configured rate.
    #[tokio::test]
    async fn tcp_object_fires_at_configured_probability() {
        let config = RiftFaultConfig {
            latency: None,
            error: None,
            tcp: Some(RiftTcpFault::Probabilistic {
                probability: 0.3,
                kind: "CONNECTION_RESET_BY_PEER".to_string(),
            }),
        };
        let iterations = 4000;
        let mut resets = 0;
        let (mut status, mut body) = (200, String::new());
        for _ in 0..iterations {
            if apply_rift_fault(&config, &mut status, &mut body)
                .await
                .and_then(|r| r.extensions().get::<TcpFaultKind>().copied())
                .is_some()
            {
                resets += 1;
            }
        }
        let observed = f64::from(resets) / f64::from(iterations);
        assert!(
            (observed - 0.3).abs() < 0.05,
            "expected ~0.3 reset rate, got {observed}"
        );
    }
}

// Issue #679: an upstream failure logged only its outermost context, so a DNS failure, a cert
// rejection and a refused connection were the same opaque line — the cause was captured by
// `.with_context()` and then dropped by the `{}` format specifier. The 502 body had the same
// hole, plus the string-interpolated JSON of the #611 class.
#[cfg(test)]
mod upstream_error_tests {
    use super::{log_upstream_failure, upstream_error_response};
    use http_body_util::BodyExt;
    use hyper::StatusCode;
    use tracing_test::traced_test;

    /// A three-level chain shaped like the real one from the issue: the outermost context is what
    /// the client may see, the root cause is the operator's alone.
    fn chained_error() -> anyhow::Error {
        anyhow::anyhow!("failed to lookup address information: Name does not resolve")
            .context("dns error")
            .context("Failed to send proxy request to https://upstream.internal/")
    }

    async fn body_of(response: hyper::Response<http_body_util::Full<bytes::Bytes>>) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("collect")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    // Covers all four anyhow log sites at once: they share this function, so none of them can
    // regress to a bare `{}` without failing here.
    #[traced_test]
    #[tokio::test]
    async fn log_upstream_failure_names_the_whole_chain() {
        log_upstream_failure("Inject function failed", &chained_error());
        assert!(
            logs_contain("Inject function failed"),
            "context must survive"
        );
        assert!(
            logs_contain("dns error"),
            "the log must name the CAUSE — that is the whole point of #679"
        );
        assert!(
            logs_contain("Name does not resolve"),
            "the log must reach the ROOT cause, not stop one level down"
        );
    }

    #[traced_test]
    #[tokio::test]
    async fn upstream_error_response_logs_the_whole_chain() {
        let _ = upstream_error_response(
            &chained_error(),
            "Proxy request failed",
            "x-rift-proxy-error",
            "Proxy error",
        );
        assert!(logs_contain("Proxy request failed"));
        assert!(
            logs_contain("Name does not resolve"),
            "the 502 path must log the root cause, not just build the body"
        );
    }

    #[tokio::test]
    async fn upstream_error_body_is_the_canonical_envelope() {
        let response =
            upstream_error_response(&chained_error(), "l", "x-rift-proxy-error", "Proxy error");
        let body = body_of(response).await;
        let v: serde_json::Value = serde_json::from_str(&body).expect("body must be valid JSON");
        // The shape the standalone proxy door already emits via crate::response::error_response —
        // matching it is what makes 0.13.6's "all proxy modes emit one envelope" actually true.
        assert_eq!(v["errors"][0]["code"], "502", "envelope code is the status");
        assert!(
            v["errors"][0]["message"]
                .as_str()
                .expect("message is a string")
                .contains("Proxy error: Failed to send proxy request"),
            "the client keeps the prefix and the outermost context: {body}"
        );
        assert!(
            v.get("error").is_none(),
            "the legacy {{\"error\"}} shape must be gone"
        );
    }

    // The helper — not the caller — decides what the client sees, so this test is load-bearing:
    // it hands over a full chain and asserts the boundary holds.
    #[tokio::test]
    async fn upstream_error_body_withholds_the_cause_chain() {
        let response =
            upstream_error_response(&chained_error(), "l", "x-rift-proxy-error", "Proxy error");
        let body = body_of(response).await;
        assert!(
            !body.contains("dns error"),
            "cause chain must not cross to the client: {body}"
        );
        assert!(
            !body.contains("Name does not resolve"),
            "root cause must not cross to the client: {body}"
        );
    }

    #[tokio::test]
    async fn a_quote_in_the_upstream_error_still_yields_valid_json() {
        // Issue #611's class: interpolating into a JSON string literal breaks the instant the
        // message holds a quote. The quote has to ride on the error's *outermost* context — that is
        // what reaches the body — and a target URL is caller-supplied config, so this is not
        // hypothetical.
        let e = anyhow::anyhow!("root cause")
            .context(r#"Failed to send proxy request to https://x/?q="a""#);
        let response = upstream_error_response(&e, "l", "x-rift-proxy-error", "Proxy error");
        let body = body_of(response).await;
        let v: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|err| panic!("a quote must not break the body: {err}\nbody: {body}"));
        assert!(
            v["errors"][0]["message"]
                .as_str()
                .expect("message is a string")
                .contains(r#"?q="a""#),
            "the quoted text must survive escaping, not be mangled: {body}"
        );
    }

    #[tokio::test]
    async fn upstream_error_keeps_status_and_rift_markers() {
        let response = upstream_error_response(
            &chained_error(),
            "l",
            "x-rift-default-forward-error",
            "defaultForward upstream error",
        );
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(response.headers()["x-rift-imposter"], "true");
        assert_eq!(
            response.headers()["x-rift-default-forward-error"],
            "true",
            "the per-door marker must survive"
        );
        assert_eq!(response.headers()["content-type"], "application/json");
    }
}
