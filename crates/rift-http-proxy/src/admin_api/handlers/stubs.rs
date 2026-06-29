//! Stub management handlers.

use crate::admin_api::handlers::imposters::handle_get as handle_get_imposter;
use crate::admin_api::types::{
    collect_body, error_response, json_response, make_stub_links, AddStubRequest,
    ReplaceStubsRequest, StubWithLinks,
};
use crate::extensions::stub_analysis::{analyze_new_stub, analyze_stubs};
use crate::imposter::{ImposterManager, Stub};
use crate::scripting::{validate_stub, validate_stubs};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use std::sync::Arc;
use tracing::warn;

/// POST /imposters/:port/stubs - Add a stub
pub async fn handle_add(
    port: u16,
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
    };

    let mut add_req: AddStubRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("Invalid stub JSON: {e}"))
        }
    };

    // Issue #202: honor a caller-supplied `id`, but generate a stable one if absent so every
    // stub is addressable via the by-id endpoints.
    if add_req.stub.id.is_none() {
        add_req.stub.id = Some(uuid::Uuid::new_v4().to_string());
    }

    // Validate scripts in the stub before adding
    let insert_index = add_req.index.unwrap_or(0);
    let validation_result = validate_stub(&add_req.stub, insert_index);
    if !validation_result.is_valid() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "Script validation failed: {}",
                validation_result.into_error_message().unwrap_or_default()
            ),
        );
    }

    // Analyze the new stub against existing stubs (Rift extension)
    if let Ok(imposter) = manager.get_imposter(port) {
        let existing_stubs = imposter.get_stubs();
        let insert_index = add_req.index.unwrap_or(existing_stubs.len());

        // Reject out-of-bounds index before mutating state
        if insert_index > existing_stubs.len() {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!(
                    "Stub index {insert_index} is out of range (imposter has {} stubs)",
                    existing_stubs.len()
                ),
            );
        }

        let analysis = analyze_new_stub(&existing_stubs, &add_req.stub, insert_index);

        for warning in &analysis.warnings {
            warn!(
                port = port,
                stub_id = ?add_req.stub.id,
                warning_type = ?warning.warning_type,
                "New stub warning: {}",
                warning.message
            );
        }
    }

    match manager.add_stub(port, add_req.stub, add_req.index).await {
        Ok(()) => handle_get_imposter(port, None, base_url, manager).await,
        Err(e) => e.into(),
    }
}

/// PUT /imposters/:port/stubs - Replace all stubs
pub async fn handle_replace_all(
    port: u16,
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
    };

    let replace_req: ReplaceStubsRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("Invalid stubs JSON: {e}"))
        }
    };

    // Validate all scripts in stubs before replacing
    let validation_result = validate_stubs(&replace_req.stubs);
    if !validation_result.is_valid() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "Script validation failed: {}",
                validation_result.into_error_message().unwrap_or_default()
            ),
        );
    }

    // Analyze the new stubs (Rift extension)
    let analysis = analyze_stubs(&replace_req.stubs);
    for warning in &analysis.warnings {
        warn!(
            port = port,
            warning_type = ?warning.warning_type,
            "Stub replacement warning: {}",
            warning.message
        );
    }

    if let Err(e) = manager.replace_stubs(port, replace_req.stubs).await {
        return e.into();
    }

    handle_get_imposter(port, None, base_url, manager).await
}

/// GET /imposters/:port/stubs - Get all stubs
pub async fn handle_get_all(
    port: u16,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let stubs = imposter.get_stubs();
            let stubs_with_links: Vec<StubWithLinks> = stubs
                .into_iter()
                .enumerate()
                .map(|(index, stub)| StubWithLinks {
                    stub,
                    links: make_stub_links(base_url, port, index),
                })
                .collect();
            json_response(
                StatusCode::OK,
                &serde_json::json!({ "stubs": stubs_with_links }),
            )
        }
        Err(e) => e.into(),
    }
}

/// GET /imposters/:port/stubs/:index - Get a specific stub
pub async fn handle_get(
    port: u16,
    index: usize,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_stub(port, index) {
        Ok(stub) => {
            let stub_with_links = StubWithLinks {
                stub,
                links: make_stub_links(base_url, port, index),
            };
            json_response(StatusCode::OK, &stub_with_links)
        }
        Err(e) => e.into(),
    }
}

/// PUT /imposters/:port/stubs/:index - Replace a specific stub
pub async fn handle_replace(
    port: u16,
    index: usize,
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
    };

    let stub: Stub = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("Invalid stub JSON: {e}"))
        }
    };

    // Validate scripts in the stub before replacing
    let validation_result = validate_stub(&stub, index);
    if !validation_result.is_valid() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "Script validation failed: {}",
                validation_result.into_error_message().unwrap_or_default()
            ),
        );
    }

    match manager.replace_stub(port, index, stub).await {
        Ok(()) => handle_get_imposter(port, None, base_url, manager).await,
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port/stubs/:index - Delete a specific stub
pub async fn handle_delete(
    port: u16,
    index: usize,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.delete_stub(port, index).await {
        Ok(()) => handle_get_imposter(port, None, base_url, manager).await,
        Err(e) => e.into(),
    }
}

// ── Id-addressed stub operations (issue #202) ───────────────────────────────────

/// GET /imposters/:port/stubs/by-id/:id — fetch the stub addressed by id.
pub async fn handle_get_by_id(
    port: u16,
    id: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_stub_by_id(port, id) {
        Ok(stub) => json_response(StatusCode::OK, &stub),
        Err(e) => e.into(),
    }
}

/// PUT /imposters/:port/stubs/by-id/:id — replace the stub addressed by id, in place.
pub async fn handle_replace_by_id(
    port: u16,
    id: &str,
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
    };
    let stub: Stub = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("Invalid stub JSON: {e}"))
        }
    };
    let validation_result = validate_stub(&stub, 0);
    if !validation_result.is_valid() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "Script validation failed: {}",
                validation_result.into_error_message().unwrap_or_default()
            ),
        );
    }
    match manager.replace_stub_by_id(port, id, stub).await {
        Ok(()) => handle_get_imposter(port, None, base_url, manager).await,
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port/stubs/by-id/:id — delete the stub addressed by id.
pub async fn handle_delete_by_id(
    port: u16,
    id: &str,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.delete_stub_by_id(port, id).await {
        Ok(()) => handle_get_imposter(port, None, base_url, manager).await,
        Err(e) => e.into(),
    }
}
