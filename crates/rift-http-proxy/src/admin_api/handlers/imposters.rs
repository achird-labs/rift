//! Imposter CRUD handlers.

use crate::admin_api::request_filter::{parse_match_clauses, request_matches};
use crate::admin_api::types::{
    collect_body, error_response, json_response, make_imposter_links, make_stub_links,
    ImposterDetail, ImposterListEntry, ImposterQueryParams, ImposterSummary, ListImpostersResponse,
    RiftImposterExtensions, StubWithLinks,
};
use crate::extensions::stub_analysis::analyze_stubs;
use crate::imposter::{
    Imposter, ImposterConfig, ImposterError, ImposterManager, RecordedRequest, StubResponse,
};
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

/// Resolve the imposter's `flow_id_source` (default `"imposter_port"`).
fn flow_id_source(imposter: &Imposter) -> String {
    imposter
        .config
        .rift
        .as_ref()
        .and_then(|r| r.flow_state.as_ref())
        .and_then(|fs| fs.mountebank_state_mapping.as_ref())
        .map(|m| m.flow_id_source.clone())
        .unwrap_or_else(|| "imposter_port".to_string())
}

/// GET /imposters/:port/requests[?match=...] - recorded requests, optionally
/// filtered by `header:<Name>=<Value>` / `flow_id=<Value>` clauses (AND'd).
pub async fn handle_get_requests(
    port: u16,
    query: Option<&str>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let clauses = match parse_match_clauses(query) {
        Ok(c) => c,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, &msg),
    };
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let source = flow_id_source(&imposter);
            let filtered: Vec<RecordedRequest> = imposter
                .get_recorded_requests()
                .into_iter()
                .filter(|r| request_matches(r, &clauses, &source, port))
                .collect();
            json_response(StatusCode::OK, &filtered)
        }
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port/savedRequests - Clear recorded requests.
/// With `match=...` clauses, only the matching slice is removed (the rest are kept);
/// without clauses, all recorded requests are cleared.
pub async fn handle_clear_requests(
    port: u16,
    query: Option<&str>,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let clauses = match parse_match_clauses(query) {
        Ok(c) => c,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, &msg),
    };
    match manager.get_imposter(port) {
        Ok(imposter) => {
            if clauses.is_empty() {
                imposter.clear_recorded_requests();
            } else {
                let source = flow_id_source(&imposter);
                imposter.retain_recorded_requests(|r| !request_matches(r, &clauses, &source, port));
            }
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
                    required_scenario_state: stub.required_scenario_state,
                    new_scenario_state: stub.new_scenario_state,
                    space: stub.space,
                    recorded_from: stub.recorded_from,
                    verify: stub.verify,
                })
            }
        })
        .collect()
}

// Issue #201: filter recorded requests by header / flow_id via
// GET /imposters/:port/requests?match=...  (and targeted DELETE).
#[cfg(test)]
mod requests_filter_tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::collections::HashMap;

    fn rec(space: &str, tenant: Option<&str>) -> RecordedRequest {
        let mut headers = HashMap::new();
        headers.insert("X-Mock-Space".to_string(), vec![space.to_string()]);
        if let Some(t) = tenant {
            headers.insert("X-Tenant".to_string(), vec![t.to_string()]);
        }
        RecordedRequest {
            request_from: "127.0.0.1".to_string(),
            method: "GET".to_string(),
            path: "/p".to_string(),
            query: HashMap::new(),
            headers,
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    async fn manager_with(
        port: u16,
        flow_id_source: Option<&str>,
        recorded: &[RecordedRequest],
    ) -> Arc<ImposterManager> {
        let manager = Arc::new(ImposterManager::new());
        let mut cfg = serde_json::json!({
            "port": port, "protocol": "http", "recordRequests": true, "stubs": []
        });
        if let Some(src) = flow_id_source {
            cfg["_rift"] = serde_json::json!({
                "flowState": { "mountebankStateMapping": { "flowIdSource": src } }
            });
        }
        let config = serde_json::from_value(cfg).expect("config");
        manager.create_imposter(config).await.expect("create");
        let imposter = manager.get_imposter(port).expect("imposter");
        for r in recorded {
            imposter.record_request(r);
        }
        manager
    }

    async fn requests(resp: Response<Full<Bytes>>) -> Vec<RecordedRequest> {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("decode requests")
    }

    #[tokio::test]
    async fn get_requests_returns_all_without_match() {
        let m = manager_with(19731, None, &[rec("A", None), rec("B", None)]).await;
        let resp = handle_get_requests(19731, None, m.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(requests(resp).await.len(), 2);
        let _ = m.delete_imposter(19731).await;
    }

    #[tokio::test]
    async fn get_requests_filters_by_header() {
        let m = manager_with(
            19732,
            None,
            &[rec("A", None), rec("B", None), rec("A", None)],
        )
        .await;
        let resp = handle_get_requests(19732, Some("match=header:X-Mock-Space=A"), m.clone()).await;
        let got = requests(resp).await;
        assert_eq!(got.len(), 2, "only the two X-Mock-Space=A requests");
        assert!(got.iter().all(|r| r
            .headers
            .get("X-Mock-Space")
            .and_then(|v| v.first())
            .map(String::as_str)
            == Some("A")));
        let _ = m.delete_imposter(19732).await;
    }

    #[tokio::test]
    async fn get_requests_no_match_is_empty_200() {
        let m = manager_with(19733, None, &[rec("A", None)]).await;
        let resp =
            handle_get_requests(19733, Some("match=header:X-Mock-Space=ZZZ"), m.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(requests(resp).await.is_empty());
        let _ = m.delete_imposter(19733).await;
    }

    #[tokio::test]
    async fn get_requests_flow_id_header_source_parity() {
        let m = manager_with(
            19734,
            Some("header:X-Mock-Space"),
            &[rec("A", None), rec("B", None)],
        )
        .await;
        let by_flow_id =
            requests(handle_get_requests(19734, Some("match=flow_id=A"), m.clone()).await).await;
        let by_header = requests(
            handle_get_requests(19734, Some("match=header:X-Mock-Space=A"), m.clone()).await,
        )
        .await;
        assert_eq!(by_flow_id.len(), 1);
        assert_eq!(by_flow_id[0].headers.get("X-Mock-Space").unwrap()[0], "A");
        assert_eq!(
            serde_json::to_value(&by_flow_id).unwrap(),
            serde_json::to_value(&by_header).unwrap(),
            "flow_id= and header: must return the identical slice when flow_id_source is that header"
        );
        let _ = m.delete_imposter(19734).await;
    }

    #[tokio::test]
    async fn get_requests_flow_id_imposter_port() {
        let m = manager_with(19735, None, &[rec("A", None), rec("B", None)]).await;
        // default flow_id_source = imposter_port → every request shares flow_id = port
        let all = handle_get_requests(19735, Some("match=flow_id=19735"), m.clone()).await;
        assert_eq!(requests(all).await.len(), 2);
        let none = handle_get_requests(19735, Some("match=flow_id=9999"), m.clone()).await;
        assert!(requests(none).await.is_empty());
        let _ = m.delete_imposter(19735).await;
    }

    #[tokio::test]
    async fn get_requests_multiple_match_anded() {
        let m = manager_with(
            19736,
            None,
            &[
                rec("A", Some("t1")),
                rec("A", Some("t2")),
                rec("B", Some("t1")),
            ],
        )
        .await;
        let resp = handle_get_requests(
            19736,
            Some("match=header:X-Mock-Space=A&match=header:X-Tenant=t1"),
            m.clone(),
        )
        .await;
        let got = requests(resp).await;
        assert_eq!(got.len(), 1, "only the A+t1 request matches both clauses");
        let _ = m.delete_imposter(19736).await;
    }

    #[tokio::test]
    async fn get_requests_unknown_port_404() {
        let m = Arc::new(ImposterManager::new());
        let resp = handle_get_requests(19737, None, m).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_requests_malformed_match_400() {
        let m = manager_with(19738, None, &[rec("A", None)]).await;
        let resp = handle_get_requests(19738, Some("match=path=/foo"), m.clone()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let _ = m.delete_imposter(19738).await;
    }

    #[tokio::test]
    async fn delete_requests_targeted_clear_removes_only_slice() {
        let m = manager_with(
            19739,
            None,
            &[rec("A", None), rec("B", None), rec("A", None)],
        )
        .await;
        let base = "http://localhost:2525";
        handle_clear_requests(19739, Some("match=header:X-Mock-Space=A"), base, m.clone()).await;
        let remaining = requests(handle_get_requests(19739, None, m.clone()).await).await;
        assert_eq!(remaining.len(), 1, "only the B request survives");
        assert_eq!(remaining[0].headers.get("X-Mock-Space").unwrap()[0], "B");
        let _ = m.delete_imposter(19739).await;
    }

    #[tokio::test]
    async fn delete_requests_without_match_clears_all() {
        let m = manager_with(19740, None, &[rec("A", None), rec("B", None)]).await;
        let base = "http://localhost:2525";
        handle_clear_requests(19740, None, base, m.clone()).await;
        let remaining = requests(handle_get_requests(19740, None, m.clone()).await).await;
        assert!(remaining.is_empty());
        let _ = m.delete_imposter(19740).await;
    }

    #[tokio::test]
    async fn delete_requests_malformed_match_400() {
        let m = manager_with(19741, None, &[rec("A", None)]).await;
        let base = "http://localhost:2525";
        let resp = handle_clear_requests(19741, Some("match=path=/foo"), base, m.clone()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // a rejected clear must leave the buffer untouched
        let remaining = requests(handle_get_requests(19741, None, m.clone()).await).await;
        assert_eq!(
            remaining.len(),
            1,
            "malformed DELETE must not clear anything"
        );
        let _ = m.delete_imposter(19741).await;
    }
}
