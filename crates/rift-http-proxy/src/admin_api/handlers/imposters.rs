//! Imposter CRUD handlers.

use crate::admin_api::types::{
    collect_body, error_response, json_response, make_imposter_links, make_stub_links,
    ImposterDetail, ImposterListEntry, ImposterQueryParams, ImposterSummary, ListImpostersResponse,
    RiftImposterExtensions, StubWithLinks,
};
use crate::extensions::stub_analysis::analyze_stubs;
use crate::imposter::{ImposterConfig, ImposterError, ImposterManager, StubResponse};
use crate::scripting::validate_stubs;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, info, warn};

/// POST /imposters - Create a new imposter
pub async fn handle_create(
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
    };

    let config: ImposterConfig = match serde_json::from_slice(&body) {
        Ok(c) => c,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid imposter JSON: {e}"),
            )
        }
    };

    // Validate all scripts in stubs before creating the imposter
    let validation_result = validate_stubs(&config.stubs);
    if !validation_result.is_valid() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "Script validation failed: {}",
                validation_result.into_error_message().unwrap_or_default()
            ),
        );
    }

    match manager.create_imposter(config).await {
        Ok(assigned_port) => {
            info!("Created imposter on port {}", assigned_port);
            // Return the full imposter details with 201 Created
            let response = handle_get(assigned_port, None, base_url, manager).await;
            let (parts, body) = response.into_parts();
            let mut new_parts = parts;
            new_parts.status = StatusCode::CREATED;
            Response::from_parts(new_parts, body)
        }
        Err(e) => e.into(),
    }
}

/// GET /imposters - List all imposters
pub async fn handle_list(
    manager: Arc<ImposterManager>,
    query: Option<&str>,
    base_url: &str,
) -> Response<Full<Bytes>> {
    let params = ImposterQueryParams::parse(query);
    let imposters = manager.list_imposters();

    if params.replayable {
        let configs: Vec<ImposterConfig> = imposters
            .iter()
            .map(|i| {
                if params.remove_proxies {
                    filter_proxy_responses(&i.config)
                } else {
                    i.config.clone()
                }
            })
            .collect();
        let body = serde_json::json!({ "imposters": configs });
        json_response(StatusCode::OK, &body)
    } else if params.list {
        // Mountebank-compatible abbreviated listing: port, protocol, name, numberOfRequests, _links
        let entries: Vec<ImposterListEntry> = imposters
            .iter()
            .filter_map(|i| {
                i.config.port.map(|port| ImposterListEntry {
                    protocol: i.config.protocol.clone(),
                    port,
                    name: i.config.name.clone(),
                    number_of_requests: i.get_request_count(),
                    links: make_imposter_links(base_url, port),
                })
            })
            .collect();
        json_response(StatusCode::OK, &serde_json::json!({ "imposters": entries }))
    } else {
        let summaries: Vec<ImposterSummary> = imposters
            .iter()
            .filter_map(|i| {
                i.config.port.map(|port| ImposterSummary {
                    protocol: i.config.protocol.clone(),
                    port,
                    name: i.config.name.clone(),
                    number_of_requests: i.get_request_count(),
                    enabled: i.is_enabled(),
                    links: make_imposter_links(base_url, port),
                })
            })
            .collect();

        let response = ListImpostersResponse {
            imposters: summaries,
        };
        json_response(StatusCode::OK, &response)
    }
}

/// PUT /imposters - Replace all imposters
pub async fn handle_replace_all(
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
    };

    #[derive(Deserialize)]
    struct BatchRequest {
        imposters: Vec<ImposterConfig>,
    }

    let batch: BatchRequest = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("Invalid batch JSON: {e}"))
        }
    };

    // Validate all scripts in all imposters before making any changes
    for (idx, config) in batch.imposters.iter().enumerate() {
        let validation_result = validate_stubs(&config.stubs);
        if !validation_result.is_valid() {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!(
                    "Script validation failed in imposter[{}] (port {:?}): {}",
                    idx,
                    config.port,
                    validation_result.into_error_message().unwrap_or_default()
                ),
            );
        }
    }

    manager.delete_all().await;

    for config in batch.imposters {
        if let Err(e) = manager.create_imposter(config.clone()).await {
            error!("Failed to create imposter on port {:?}: {}", config.port, e);
        }
    }

    handle_list(manager, None, base_url).await
}

/// DELETE /imposters - Delete all imposters
pub async fn handle_delete_all(
    manager: Arc<ImposterManager>,
    _base_url: &str,
) -> Response<Full<Bytes>> {
    let configs = manager.delete_all().await;
    let body = serde_json::json!({ "imposters": configs });
    json_response(StatusCode::OK, &body)
}

/// GET /imposters/:port - Get a specific imposter
pub async fn handle_get(
    port: u16,
    query: Option<&str>,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let params = ImposterQueryParams::parse(query);

    match manager.get_imposter(port) {
        Ok(imposter) => {
            let mut stubs = imposter.get_stubs();

            if params.remove_proxies {
                stubs = filter_proxy_stubs(stubs);
            }

            let analysis = analyze_stubs(&stubs);
            let rift_extensions = if analysis.has_warnings() {
                for warning in &analysis.warnings {
                    warn!(
                        port = port,
                        warning_type = ?warning.warning_type,
                        "Stub analysis warning: {}",
                        warning.message
                    );
                }
                Some(RiftImposterExtensions {
                    warnings: analysis.warnings,
                })
            } else {
                None
            };

            let stubs_with_links: Vec<StubWithLinks> = stubs
                .into_iter()
                .enumerate()
                .map(|(index, stub)| StubWithLinks {
                    stub,
                    links: make_stub_links(base_url, port, index),
                })
                .collect();

            let detail = ImposterDetail {
                protocol: imposter.config.protocol.clone(),
                port: imposter.config.port.unwrap_or(port),
                name: imposter.config.name.clone(),
                number_of_requests: imposter.get_request_count(),
                enabled: imposter.is_enabled(),
                record_requests: imposter.config.record_requests,
                requests: imposter.get_recorded_requests(),
                stubs: stubs_with_links,
                links: make_imposter_links(base_url, port),
                rift: rift_extensions,
            };
            json_response(StatusCode::OK, &detail)
        }
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port - Delete a specific imposter
pub async fn handle_delete(
    port: u16,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.delete_imposter(port).await {
        Ok(config) => {
            info!("Deleted imposter on port {}", port);
            let stubs_with_links: Vec<StubWithLinks> = config
                .stubs
                .iter()
                .enumerate()
                .map(|(index, stub)| StubWithLinks {
                    stub: stub.clone(),
                    links: make_stub_links(base_url, port, index),
                })
                .collect();
            let response = serde_json::json!({
                "protocol": config.protocol,
                "port": config.port,
                "name": config.name,
                "numberOfRequests": 0,
                "recordRequests": config.record_requests,
                "requests": [],
                "stubs": stubs_with_links,
                "_links": make_imposter_links(base_url, port)
            });
            json_response(StatusCode::OK, &response)
        }
        Err(ImposterError::NotFound(_)) => error_response(
            StatusCode::NOT_FOUND,
            &format!("No imposter exists on port {port}"),
        ),
        Err(e) => e.into(),
    }
}

/// POST /imposters/:port/enable - Enable imposter
pub async fn handle_enable(port: u16, manager: Arc<ImposterManager>) -> Response<Full<Bytes>> {
    handle_set_enabled(port, true, manager).await
}

/// POST /imposters/:port/disable - Disable imposter
pub async fn handle_disable(port: u16, manager: Arc<ImposterManager>) -> Response<Full<Bytes>> {
    handle_set_enabled(port, false, manager).await
}

/// Set enabled state for an imposter
async fn handle_set_enabled(
    port: u16,
    enabled: bool,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            imposter.set_enabled(enabled);
            let state = if enabled { "enabled" } else { "disabled" };
            json_response(
                StatusCode::OK,
                &serde_json::json!({"message": format!("Imposter {}", state)}),
            )
        }
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port/savedRequests - Clear recorded requests
pub async fn handle_clear_requests(
    port: u16,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            imposter.clear_recorded_requests();
            handle_get(port, None, base_url, manager).await
        }
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port/savedProxyResponses - Clear proxy responses
pub async fn handle_clear_proxy_responses(
    port: u16,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            imposter.clear_proxy_responses();
            handle_get(port, None, base_url, manager).await
        }
        Err(e) => e.into(),
    }
}

// =============================================================================
// Helper functions
// =============================================================================

/// Filter out proxy responses from stubs
fn filter_proxy_responses(config: &ImposterConfig) -> ImposterConfig {
    let mut filtered = config.clone();
    filtered.stubs = filter_proxy_stubs(config.stubs.clone());
    filtered
}

/// Filter proxy responses from a list of stubs
fn filter_proxy_stubs(stubs: Vec<crate::imposter::Stub>) -> Vec<crate::imposter::Stub> {
    stubs
        .into_iter()
        .filter_map(|stub| {
            let non_proxy_responses: Vec<_> = stub
                .responses
                .iter()
                .filter(|r| !matches!(r, StubResponse::Proxy { .. }))
                .cloned()
                .collect();

            if non_proxy_responses.is_empty() {
                None
            } else {
                Some(crate::imposter::Stub {
                    id: stub.id,
                    predicates: stub.predicates,
                    responses: non_proxy_responses,
                    scenario_name: stub.scenario_name,
                    recorded_from: stub.recorded_from,
                })
            }
        })
        .collect()
}
