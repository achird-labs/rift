//! Request handling and fault injection logic.
//!
//! This module contains the core request handling logic including:
//! - Script rule matching and execution
//! - YAML rule matching and fault injection
//! - Response behavior application (wait, copy, lookup, shell, decorate)

use super::forwarding::{error_response, forward_request_with_body, forward_with_recording};
use super::headers::{
    RiftHeadersExt, VALUE_ERROR, VALUE_LATENCY, VALUE_TCP, VALUE_TRUE, X_RIFT_BEHAVIOR_COPY,
    X_RIFT_BEHAVIOR_DECORATE, X_RIFT_BEHAVIOR_LOOKUP, X_RIFT_BEHAVIOR_SHELL, X_RIFT_BEHAVIOR_WAIT,
    X_RIFT_FAULT, X_RIFT_LATENCY_MS, X_RIFT_RULE_ID, X_RIFT_SCRIPT, X_RIFT_TCP_FAULT,
};
use super::response_ext::ResponseExt;
use crate::behaviors::{
    RequestContext, apply_copy_behaviors, apply_decorate, apply_lookup_behaviors,
    apply_shell_transform,
};
use crate::config::TcpFault;
use crate::extensions::fault::{FaultDecision, apply_latency, create_error_response, decide_fault};
use crate::extensions::matcher::CompiledRule;
use crate::extensions::metrics;
use crate::extensions::routing::Router;
use crate::extensions::template::{RequestData, has_template_variables, process_template};
use crate::proxy::context::{
    ForwardingContext, RequestHandlerContext, RequestInfo, ScriptingContext, UpstreamService,
};
use crate::scripting::{CacheKey, FaultDecision as ScriptFaultDecision, ScriptRequest};

use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::body::Bytes;
use hyper::{Request, Response};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Classify a request body for `ScriptRequest::raw_body`/`mode` (issue #636). A binary body
/// (protobuf, gzip, an image upload) is not valid UTF-8; `from_utf8_lossy` used to silently
/// replace the offending bytes with U+FFFD, corrupting `raw_body` for every script that reads
/// it. Base64-encode it instead, like the imposter serve path (`imposter/handler.rs`) and the
/// response-side precedent (`util::encode_body_for_stub`, issue #117).
fn classify_script_request_body(body_bytes: &[u8]) -> (String, crate::imposter::ResponseMode) {
    match std::str::from_utf8(body_bytes) {
        Ok(text) => (text.to_string(), crate::imposter::ResponseMode::Text),
        Err(_) => {
            use base64::Engine;
            (
                base64::engine::general_purpose::STANDARD.encode(body_bytes),
                crate::imposter::ResponseMode::Binary,
            )
        }
    }
}

/// Handle an incoming request with fault injection and forwarding.
pub async fn handle_request(
    ctx: &RequestHandlerContext<'_>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Infallible> {
    let start_time = std::time::Instant::now();
    // Method is an enum tag for standard verbs (effectively free to clone) and is needed for
    // metrics after `req` is consumed below. Uri/HeaderMap stay borrowed until a fault path
    // actually needs owned copies (issue #545).
    let method = req.method().clone();

    debug!("Received request: {} {}", method, req.uri());

    let upstream = select_upstream(ctx.router, ctx.upstreams, &req)
        .map(|(url, name)| UpstreamService {
            url: Some(url),
            name: Some(name),
        })
        .unwrap_or_default();

    // Check script rules first (if configured) - optimized path with pool and cache
    let req = if let (Some(compiled_scripts), Some(script_pool), Some(decision_cache)) =
        (ctx.compiled_scripts, ctx.script_pool, ctx.decision_cache)
    {
        let scripting = ScriptingContext {
            compiled_scripts,
            script_pool,
            decision_cache,
        };

        match handle_script_rules(ctx, &scripting, req, &upstream, start_time).await {
            RuleHandlingResult::Response(response) => return Ok(response),
            RuleHandlingResult::NoFault(req) => req,
        }
    } else {
        req
    };

    // Find matching YAML rule that applies to selected upstream
    let matched_rule_index = ctx
        .compiled_rules
        .iter()
        .enumerate()
        .find(|(idx, rule)| {
            rule.matches(req.method(), req.uri(), req.headers())
                && rule_applies_to_upstream(&ctx.rule_upstreams[*idx], upstream.name.as_deref())
        })
        .map(|(idx, _)| idx);

    if let Some(rule_idx) = matched_rule_index {
        let rule = &ctx.compiled_rules[rule_idx];
        info!("Request matched rule: {}", rule.id);

        match handle_yaml_rule(ctx, rule, req, upstream.url.as_deref(), start_time).await {
            RuleHandlingResult::Response(response) => return Ok(response),
            RuleHandlingResult::NoFault(r) => {
                // Continue to forward without fault
                let upstream_url = upstream.url.as_deref().unwrap_or(ctx.upstream_uri);
                let response = forward_with_recording(
                    ctx.http_client,
                    ctx.recording_store,
                    ctx.recording_signature_headers,
                    r,
                    upstream_url,
                )
                .await;
                let status = response.status().as_u16();
                let duration_ms = start_time.elapsed().as_secs_f64() * 1000.0;
                metrics::record_proxy_duration(method.as_str(), duration_ms, "none");
                metrics::record_request(method.as_str(), status);
                return Ok(response);
            }
        }
    }

    // Forward request without fault (with recording support if enabled)
    let upstream_url = upstream.url.as_deref().unwrap_or(ctx.upstream_uri);
    let response = forward_with_recording(
        ctx.http_client,
        ctx.recording_store,
        ctx.recording_signature_headers,
        req,
        upstream_url,
    )
    .await;
    let status = response.status().as_u16();
    let duration_ms = start_time.elapsed().as_secs_f64() * 1000.0;
    metrics::record_proxy_duration(method.as_str(), duration_ms, "none");
    metrics::record_request(method.as_str(), status);
    Ok(response)
}

/// Result of rule handling - either a response or the request back
pub enum RuleHandlingResult {
    /// A rule matched and returned a response
    Response(Response<BoxBody<Bytes, hyper::Error>>),
    /// No fault injected, here's the request back for forwarding
    NoFault(Request<hyper::body::Incoming>),
}

/// Handle script rules - returns either a response or the request back if no script matched.
async fn handle_script_rules(
    ctx: &RequestHandlerContext<'_>,
    scripting: &ScriptingContext<'_>,
    req: Request<hyper::body::Incoming>,
    upstream: &UpstreamService,
    start_time: std::time::Instant,
) -> RuleHandlingResult {
    // Match against borrows from `req` first so a request that hits no script rule (the common
    // case when scripts are configured but this one doesn't apply) never pays for the
    // Method/Uri/HeaderMap clones RequestInfo would build.
    let matching_script =
        scripting
            .compiled_scripts
            .iter()
            .find(|(_, compiled_rule, rule_upstream)| {
                compiled_rule.matches(req.method(), req.uri(), req.headers())
                    && rule_applies_to_upstream(rule_upstream, upstream.name.as_deref())
            });

    let Some((compiled_script, compiled_rule, _)) = matching_script else {
        return RuleHandlingResult::NoFault(req);
    };
    info!("Request matched script rule: {}", compiled_rule.id);

    // A script matched, so `req` is committed to being consumed below (script context, cache
    // key, forwarding) — build the owned RequestInfo now, immediately before body collection.
    let request_info = RequestInfo::from_request(&req);

    // Collect body for script (needed for script context)
    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!("Failed to collect request body: {}", e);
            return RuleHandlingResult::Response(
                error_response(500, "Failed to read request body").into_boxed(),
            );
        }
    };

    // Convert to script request
    let mut headers_map = HashMap::new();
    for (k, v) in request_info.headers.iter() {
        if let Ok(value_str) = v.to_str() {
            headers_map.insert(k.as_str().to_string(), value_str.to_string());
        }
    }

    let body_json: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);

    let (raw_body, body_mode) = classify_script_request_body(&body_bytes);

    // Parse query parameters from URI
    let query_params = crate::predicate::parse_query_string(request_info.uri.query());

    // Key first: `key_headers` only borrows, so the map can then move into the script request
    // rather than being cloned (it was cloned only because the old key build consumed it).
    //
    // Only the KEY is filtered — the script still receives every header, which is what makes the
    // allowlist a user assertion rather than something rift could infer (issue #630).
    let cache_key = CacheKey::new(
        request_info.method.to_string(),
        request_info.uri.path().to_string(),
        scripting.decision_cache.key_headers(&headers_map),
        &body_json,
        compiled_rule.id.clone(),
    );

    let script_request = ScriptRequest {
        raw_body: Some(raw_body),
        mode: body_mode,
        method: request_info.method.to_string(),
        path: request_info.uri.path().to_string(),
        headers: headers_map,
        body: body_json.clone(),
        query: query_params,
        path_params: HashMap::new(),
    };

    // Determine if caching should be used
    // If flow_state is configured (not NoOpFlowStore), disable caching
    // because scripts using flow_store are stateful and results vary
    let use_cache = !ctx.flow_state_configured;

    // Check cache first (only for stateless scripts), then execute via pool
    let script_start = std::time::Instant::now();
    let result = if use_cache {
        if let Some(cached_decision) = scripting.decision_cache.get(&cache_key) {
            debug!("Cache hit for rule: {} (stateless)", compiled_rule.id);
            Ok(cached_decision)
        } else {
            debug!("Cache miss for rule: {}", compiled_rule.id);

            // Execute via pool
            let pool_result = scripting
                .script_pool
                .execute(
                    compiled_script.clone(),
                    script_request,
                    Arc::clone(ctx.flow_store),
                )
                .await;

            // Cache the result if successful (stateless only)
            if let Ok(ref decision) = pool_result {
                scripting.decision_cache.insert(cache_key, decision.clone());
            }

            pool_result
        }
    } else {
        // Stateful script: always execute, never cache
        debug!("Executing stateful script (no cache): {}", compiled_rule.id);
        scripting
            .script_pool
            .execute(
                compiled_script.clone(),
                script_request,
                Arc::clone(ctx.flow_store),
            )
            .await
    };
    let script_duration = script_start.elapsed().as_secs_f64() * 1000.0;

    let forwarding_ctx = ForwardingContext {
        info: request_info,
        body_bytes,
        start_time,
        upstream_service: upstream.to_owned(),
    };

    RuleHandlingResult::Response(
        handle_script_result(
            ctx,
            result.map_err(|e| e.to_string()),
            compiled_rule,
            &forwarding_ctx,
            script_duration,
        )
        .await,
    )
}

/// Handle the result of a script execution.
async fn handle_script_result(
    ctx: &RequestHandlerContext<'_>,
    result: Result<ScriptFaultDecision, String>,
    compiled_rule: &CompiledRule,
    forwarding_ctx: &ForwardingContext,
    script_duration: f64,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    let request_info = &forwarding_ctx.info;
    match result {
        Ok(ScriptFaultDecision::Error {
            status,
            body,
            rule_id,
            headers: script_headers,
        }) => {
            warn!(
                "Script injecting error fault: status={}, rule={}",
                status, rule_id
            );

            // Record metrics
            metrics::record_script_execution(&rule_id, script_duration, "inject");
            metrics::record_script_fault("error", &rule_id, None);
            metrics::record_error_injection(&rule_id, status);

            let duration_ms = forwarding_ctx.start_time.elapsed().as_secs_f64() * 1000.0;
            metrics::record_proxy_duration(request_info.method.as_str(), duration_ms, "script");
            metrics::record_request(request_info.method.as_str(), status);

            // Find fixed headers from matching YAML rule (if any)
            let fixed_headers = ctx
                .compiled_rules
                .iter()
                .enumerate()
                .find(|(idx, rule)| {
                    rule.matches(
                        &request_info.method,
                        &request_info.uri,
                        &request_info.headers,
                    ) && rule_applies_to_upstream(
                        &ctx.rule_upstreams[*idx],
                        forwarding_ctx.upstream_service.name.as_deref(),
                    ) && rule.rule.fault.error.is_some()
                })
                .and_then(|(_, rule)| rule.rule.fault.error.as_ref().map(|e| e.headers.clone()));

            let mut response =
                create_error_response(status, body, fixed_headers.as_ref(), Some(&script_headers))
                    .unwrap();
            response.set_header(&X_RIFT_FAULT, &VALUE_ERROR);
            response.set_header_value(&X_RIFT_RULE_ID, &rule_id);
            response.set_header(&X_RIFT_SCRIPT, &VALUE_TRUE);
            response.into_boxed()
        }
        Ok(ScriptFaultDecision::Latency {
            duration_ms,
            rule_id,
        }) => {
            info!(
                "Script injecting latency fault: {}ms, rule={}",
                duration_ms, rule_id
            );

            // Record metrics
            metrics::record_script_execution(&rule_id, script_duration, "inject");
            metrics::record_script_fault("latency", &rule_id, Some(duration_ms));

            apply_latency(duration_ms).await;

            // Forward with body for latency fault
            let upstream_url = forwarding_ctx
                .upstream_service
                .url
                .as_deref()
                .unwrap_or(ctx.upstream_uri);
            let mut response = forward_request_with_body(
                ctx.http_client,
                request_info.method.clone(),
                request_info.uri.clone(),
                request_info.headers.clone(),
                forwarding_ctx.body_bytes.clone(),
                upstream_url,
            )
            .await;
            let status = response.status().as_u16();

            let total_duration = forwarding_ctx.start_time.elapsed().as_secs_f64() * 1000.0;
            metrics::record_proxy_duration(request_info.method.as_str(), total_duration, "script");
            metrics::record_request(request_info.method.as_str(), status);

            response.set_header(&X_RIFT_FAULT, &VALUE_LATENCY);
            response.set_header_value(&X_RIFT_RULE_ID, &rule_id);
            response.set_header(&X_RIFT_SCRIPT, &VALUE_TRUE);
            response.set_header_value(&X_RIFT_LATENCY_MS, &duration_ms.to_string());
            response.into_boxed()
        }
        Ok(ScriptFaultDecision::Reset { rule_id }) => {
            // The proxy path has no transport-level reset hook (unlike the imposter serve loop's
            // `FaultIo`), so `reset()` is simulated the same way a config-driven `TcpFault` is on
            // this path: a synthesized 502 tagged with the tcp-fault headers.
            warn!("Script injecting connection reset fault: rule={}", rule_id);

            metrics::record_script_execution(&rule_id, script_duration, "inject");
            metrics::record_script_fault("reset", &rule_id, None);
            metrics::record_error_injection(&rule_id, 0);

            let duration_ms = forwarding_ctx.start_time.elapsed().as_secs_f64() * 1000.0;
            metrics::record_proxy_duration(request_info.method.as_str(), duration_ms, "script");

            let mut response = create_error_response(
                502,
                r#"{"error": "Connection reset by peer"}"#.to_string(),
                None,
                None,
            )
            .unwrap();
            response.set_header(&X_RIFT_FAULT, &VALUE_TCP);
            response.set_header_value(&X_RIFT_RULE_ID, &rule_id);
            response.set_header(&X_RIFT_SCRIPT, &VALUE_TRUE);
            response.into_boxed()
        }
        Ok(ScriptFaultDecision::None) => {
            debug!(
                "Script decided not to inject fault for rule: {}",
                compiled_rule.id
            );
            metrics::record_script_execution(&compiled_rule.id, script_duration, "pass");

            // Forward request
            let upstream_url = forwarding_ctx
                .upstream_service
                .url
                .as_deref()
                .unwrap_or(ctx.upstream_uri);
            let response = forward_request_with_body(
                ctx.http_client,
                request_info.method.clone(),
                request_info.uri.clone(),
                request_info.headers.clone(),
                forwarding_ctx.body_bytes.clone(),
                upstream_url,
            )
            .await;
            let status = response.status().as_u16();
            let duration_ms = forwarding_ctx.start_time.elapsed().as_secs_f64() * 1000.0;
            metrics::record_proxy_duration(request_info.method.as_str(), duration_ms, "none");
            metrics::record_request(request_info.method.as_str(), status);
            response.into_boxed()
        }
        Err(e) => {
            error!(
                "Script execution error for rule {}: {}",
                compiled_rule.id, e
            );
            metrics::record_script_execution(&compiled_rule.id, script_duration, "error");
            metrics::record_script_error(&compiled_rule.id, "runtime");

            // Forward request on error
            let upstream_url = forwarding_ctx
                .upstream_service
                .url
                .as_deref()
                .unwrap_or(ctx.upstream_uri);
            let response = forward_request_with_body(
                ctx.http_client,
                request_info.method.clone(),
                request_info.uri.clone(),
                request_info.headers.clone(),
                forwarding_ctx.body_bytes.clone(),
                upstream_url,
            )
            .await;
            let status = response.status().as_u16();
            let duration_ms = forwarding_ctx.start_time.elapsed().as_secs_f64() * 1000.0;
            metrics::record_proxy_duration(request_info.method.as_str(), duration_ms, "none");
            metrics::record_request(request_info.method.as_str(), status);
            response.into_boxed()
        }
    }
}

/// Handle a matched YAML rule - returns response or the request back if no fault.
#[allow(clippy::too_many_arguments)]
async fn handle_yaml_rule(
    ctx: &RequestHandlerContext<'_>,
    rule: &CompiledRule,
    req: Request<hyper::body::Incoming>,
    selected_upstream_url: Option<&str>,
    start_time: std::time::Instant,
) -> RuleHandlingResult {
    // Decide fault
    let fault_decision = decide_fault(&rule.rule.fault, &rule.id);

    match fault_decision {
        // Sampled no-fault is the common case at low fault probabilities, and this arm hands
        // `req` straight back — hoisted above the RequestInfo build so that path stays
        // allocation-free (issue #545).
        FaultDecision::None => {
            debug!("No fault injected for matched rule: {}", rule.id);
            RuleHandlingResult::NoFault(req)
        }
        FaultDecision::TcpFault {
            fault_type,
            rule_id,
        } => {
            warn!("Injecting TCP fault: {:?}, rule={}", fault_type, rule_id);

            // Record metrics
            metrics::record_error_injection(&rule_id, 0);
            let duration_ms = start_time.elapsed().as_secs_f64() * 1000.0;
            metrics::record_proxy_duration(req.method().as_str(), duration_ms, "tcp_fault");

            // Return appropriate error based on fault type
            let (status, body) = match fault_type {
                TcpFault::ConnectionResetByPeer => {
                    (502, r#"{"error": "Connection reset by peer"}"#.to_string())
                }
                TcpFault::RandomDataThenClose => (
                    502,
                    r#"{"error": "Connection closed unexpectedly"}"#.to_string(),
                ),
            };

            let mut response = create_error_response(status, body, None, None).unwrap();
            response.set_header(&X_RIFT_FAULT, &VALUE_TCP);
            response.set_header_value(&X_RIFT_RULE_ID, &rule_id);
            response.set_header_value(&X_RIFT_TCP_FAULT, &format!("{fault_type:?}").to_lowercase());
            RuleHandlingResult::Response(response.into_boxed())
        }
        FaultDecision::Error {
            status,
            body,
            rule_id,
            headers: fault_headers,
            behaviors,
        } => {
            warn!("Injecting error fault: status={}, rule={}", status, rule_id);

            // Apply wait behavior if present (Mountebank-compatible)
            if let Some(ref bhvs) = behaviors
                && let Some(ref wait) = bhvs.wait
            {
                let wait_ms = wait.get_duration_ms();
                debug!("Applying wait behavior: {}ms", wait_ms);
                apply_latency(wait_ms).await;
            }

            // Record metrics
            metrics::record_error_injection(&rule_id, status);
            let duration_ms = start_time.elapsed().as_secs_f64() * 1000.0;
            metrics::record_proxy_duration(req.method().as_str(), duration_ms, "error");
            metrics::record_request(req.method().as_str(), status);

            // Build request context for behaviors
            let request_context = RequestContext::from_request(
                req.method().as_str(),
                req.uri(),
                req.headers(),
                None, // Body not available for YAML rules
            );

            // Process template variables in response body if present
            let mut processed_body = if has_template_variables(&body) {
                let request_data = RequestData::new(
                    req.method().as_str(),
                    req.uri().path(),
                    req.uri().query(),
                    req.headers(),
                    None,
                );
                process_template(&body, &request_data)
            } else {
                body
            };

            // Clone headers for mutation. copy/lookup operate on multi-value headers; proxy
            // fault responses are single-value, so wrap losslessly for the substitution and fold
            // the (still single) result back for decorate / response building.
            let mut response_headers = fault_headers.clone();
            if let Some(ref bhvs) = behaviors
                && (!bhvs.copy.is_empty() || !bhvs.lookup.is_empty())
            {
                let mut multi: std::collections::HashMap<String, Vec<String>> = response_headers
                    .drain()
                    .map(|(k, v)| (k, vec![v]))
                    .collect();
                if !bhvs.copy.is_empty() {
                    debug!("Applying {} copy behaviors", bhvs.copy.len());
                    processed_body = apply_copy_behaviors(
                        &processed_body,
                        &mut multi,
                        &bhvs.copy,
                        &request_context,
                    );
                }
                if !bhvs.lookup.is_empty() {
                    debug!("Applying {} lookup behaviors", bhvs.lookup.len());
                    processed_body = apply_lookup_behaviors(
                        &processed_body,
                        &mut multi,
                        &bhvs.lookup,
                        &request_context,
                        ctx.csv_cache,
                    );
                }
                response_headers = multi.into_iter().map(|(k, v)| (k, v.join(", "))).collect();
            }

            // Apply shell transform (Mountebank-compatible)
            if let Some(ref bhvs) = behaviors {
                for cmd in &bhvs.shell_transform {
                    debug!("Applying shell transform: {}", cmd);
                    // Off the tokio worker (issue #478): the synchronous subprocess would
                    // otherwise stall the worker for its whole lifetime.
                    let shell_result = {
                        let cmd = cmd.clone();
                        let rc = request_context.clone();
                        let body_in = processed_body.clone();
                        tokio::task::spawn_blocking(move || {
                            apply_shell_transform(&cmd, &rc, &body_in, status)
                        })
                        .await
                    };
                    match shell_result {
                        Ok(Ok(transformed)) => {
                            processed_body = transformed;
                        }
                        Ok(Err(e)) => {
                            warn!("Shell transform failed: {}", e);
                        }
                        Err(join_err) => {
                            warn!("Shell transform task panicked: {}", join_err);
                        }
                    }
                }
            }

            // Apply decorate behavior (Mountebank-compatible Rhai script)
            let mut final_status = status;
            if let Some(ref bhvs) = behaviors
                && let Some(ref script) = bhvs.decorate
            {
                debug!("Applying decorate behavior");
                match apply_decorate(
                    script,
                    &request_context,
                    &processed_body,
                    status,
                    &mut response_headers,
                ) {
                    Ok((new_body, new_status)) => {
                        processed_body = new_body;
                        final_status = new_status;
                    }
                    Err(e) => {
                        warn!("Decorate behavior failed: {}", e);
                    }
                }
            }

            let mut response =
                create_error_response(final_status, processed_body, Some(&response_headers), None)
                    .unwrap();
            response.set_header(&X_RIFT_FAULT, &VALUE_ERROR);
            response.set_header_value(&X_RIFT_RULE_ID, &rule_id);

            // Add behavior headers for debugging/testing
            if let Some(ref bhvs) = behaviors {
                if bhvs.wait.is_some() {
                    response.set_header(&X_RIFT_BEHAVIOR_WAIT, &VALUE_TRUE);
                }
                if !bhvs.copy.is_empty() {
                    response.set_header(&X_RIFT_BEHAVIOR_COPY, &VALUE_TRUE);
                }
                if !bhvs.lookup.is_empty() {
                    response.set_header(&X_RIFT_BEHAVIOR_LOOKUP, &VALUE_TRUE);
                }
                if !bhvs.shell_transform.is_empty() {
                    response.set_header(&X_RIFT_BEHAVIOR_SHELL, &VALUE_TRUE);
                }
                if bhvs.decorate.is_some() {
                    response.set_header(&X_RIFT_BEHAVIOR_DECORATE, &VALUE_TRUE);
                }
            }

            RuleHandlingResult::Response(response.into_boxed())
        }
        FaultDecision::Latency {
            duration_ms,
            rule_id,
        } => {
            let request_info = RequestInfo::from_request(&req);
            info!(
                "Injecting latency fault: {}ms, rule={}",
                duration_ms, rule_id
            );

            // Record metrics
            metrics::record_latency_injection(&rule_id, duration_ms);

            apply_latency(duration_ms).await;

            // Collect body for retry capability
            let body_bytes = match req.collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(e) => {
                    error!("Failed to collect request body: {}", e);
                    let mut response = error_response(500, "Failed to read request body");
                    response.set_header(&X_RIFT_FAULT, &VALUE_LATENCY);
                    response.set_header_value(&X_RIFT_RULE_ID, &rule_id);
                    return RuleHandlingResult::Response(response.into_boxed());
                }
            };

            // Forward request with latency header
            let upstream_url = selected_upstream_url.unwrap_or(ctx.upstream_uri);
            let mut response = forward_request_with_body(
                ctx.http_client,
                request_info.method.clone(),
                request_info.uri.clone(),
                request_info.headers.clone(),
                body_bytes,
                upstream_url,
            )
            .await;
            let status = response.status().as_u16();
            let total_duration = start_time.elapsed().as_secs_f64() * 1000.0;
            metrics::record_proxy_duration(request_info.method.as_str(), total_duration, "latency");
            metrics::record_request(request_info.method.as_str(), status);

            response.set_header(&X_RIFT_FAULT, &VALUE_LATENCY);
            response.set_header_value(&X_RIFT_RULE_ID, &rule_id);
            response.set_header_value(&X_RIFT_LATENCY_MS, &duration_ms.to_string());
            RuleHandlingResult::Response(response.into_boxed())
        }
    }
}

/// Select upstream for the request based on routing rules.
/// Returns the upstream URL and name if matched, None for sidecar mode.
fn select_upstream<B>(
    router: Option<&Router>,
    upstreams: &[crate::config::Upstream],
    req: &Request<B>,
) -> Option<(String, String)> {
    // If no router configured, use sidecar mode (return None)
    let router = router?;

    // Match request to an upstream name
    let upstream_name = router.match_request(req)?;

    // Find upstream by name
    let upstream = upstreams.iter().find(|u| u.name == upstream_name)?;
    debug!("Routed to upstream: {} ({})", upstream_name, upstream.url);
    Some((upstream.url.clone(), upstream_name.to_string()))
}

/// Check if a rule applies to the given upstream.
/// Returns true if:
/// - Rule has no upstream filter (applies to all)
/// - Rule's upstream matches the selected upstream name
/// - No upstream is selected (sidecar mode - applies to all)
pub fn rule_applies_to_upstream(
    rule_upstream_filter: &Option<String>,
    selected_upstream_name: Option<&str>,
) -> bool {
    match (rule_upstream_filter, selected_upstream_name) {
        // Rule has no filter - applies to all upstreams
        (None, _) => true,
        // No upstream selected (sidecar mode) - rule applies
        (Some(_), None) => true,
        // Both specified - must match
        (Some(rule_upstream), Some(selected)) => rule_upstream == selected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Issue #636: a binary body must round-trip through base64, not get mangled by
    // `from_utf8_lossy`'s U+FFFD replacement.
    #[test]
    fn classify_script_request_body_base64_round_trips_invalid_utf8() {
        let original: &[u8] = &[0xFF, 0xFE, 0x00, 0x01, 0x02];
        let (raw_body, mode) = classify_script_request_body(original);
        assert_eq!(mode, crate::imposter::ResponseMode::Binary);
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&raw_body)
            .expect("valid base64");
        assert_eq!(decoded, original);
    }

    // A valid-UTF-8 body stays plain text with `Text` mode — the common case is unaffected.
    #[test]
    fn classify_script_request_body_text_stays_text() {
        let (raw_body, mode) = classify_script_request_body(b"hello world");
        assert_eq!(mode, crate::imposter::ResponseMode::Text);
        assert_eq!(raw_body, "hello world");
    }

    #[test]
    fn test_rule_applies_to_upstream_no_filter() {
        // Rule with no upstream filter should apply to all upstreams
        assert!(rule_applies_to_upstream(&None, None));
        assert!(rule_applies_to_upstream(&None, Some("backend-a")));
        assert!(rule_applies_to_upstream(&None, Some("backend-b")));
    }

    #[test]
    fn test_rule_applies_to_upstream_sidecar_mode() {
        // Sidecar mode (no upstream selected) - rule should apply
        assert!(rule_applies_to_upstream(
            &Some("backend-a".to_string()),
            None
        ));
        assert!(rule_applies_to_upstream(
            &Some("backend-b".to_string()),
            None
        ));
    }

    #[test]
    fn test_rule_applies_to_upstream_matching() {
        // Rule upstream matches selected upstream
        assert!(rule_applies_to_upstream(
            &Some("backend-a".to_string()),
            Some("backend-a")
        ));
    }

    #[test]
    fn test_rule_applies_to_upstream_non_matching() {
        // Rule upstream does NOT match selected upstream
        assert!(!rule_applies_to_upstream(
            &Some("backend-a".to_string()),
            Some("backend-b")
        ));
        assert!(!rule_applies_to_upstream(
            &Some("backend-x".to_string()),
            Some("backend-y")
        ));
    }

    #[test]
    fn test_rule_applies_to_upstream_empty_strings() {
        // Empty string cases
        assert!(rule_applies_to_upstream(&Some("".to_string()), Some("")));
        assert!(!rule_applies_to_upstream(
            &Some("backend".to_string()),
            Some("")
        ));
    }
}
