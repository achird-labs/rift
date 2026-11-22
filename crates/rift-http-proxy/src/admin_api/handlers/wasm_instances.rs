//! `GET /imposters/:port/wasm/instances` (issue #1793): the `instance: "imposter"` wasm instances
//! an imposter keeps, one per component, `export` and `knobs` its stubs reference; and
//! `GET /imposters/:port/wasm/snapshot` (issue #1794), one of their `rift:mock/introspect` states.

use crate::admin_api::types::json_response;
use crate::imposter::ImposterManager;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use std::sync::Arc;

/// GET /imposters/:port/wasm/instances. In a build without the `wasm` feature the route answers
/// `404` naming the feature, so a client tells "no instances" from "no engine".
pub async fn handle_list(port: u16, manager: Arc<ImposterManager>) -> Response<Full<Bytes>> {
    let imposter = match manager.get_imposter(port) {
        Ok(imposter) => imposter,
        Err(e) => return e.into(),
    };
    if !cfg!(feature = "wasm") {
        return crate::response::error_response_typed(
            StatusCode::NOT_FOUND,
            crate::response::ErrorKind::NoSuchResource,
            "engine \"wasm\" is not available in this build",
        );
    }
    json_response(
        StatusCode::OK,
        &serde_json::json!({ "instances": imposter.wasm_instances().list() }),
    )
}

/// GET /imposters/:port/wasm/snapshot[?instance=<id>] (issue #1794): the guest's
/// `rift:mock/introspect.snapshot` text, byte for byte, with the fuel it spent in
/// `X-Rift-Fuel-Used`. `id` is one `GET /imposters/:port/wasm/instances` lists; without it the
/// imposter's only live instance answers. The route never instantiates.
pub async fn handle_snapshot(
    port: u16,
    query: Option<&str>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    use crate::response::{ErrorKind, error_response_typed};
    use rift_mock_core::scripting::WasmSnapshotError as E;

    let imposter = match manager.get_imposter(port) {
        Ok(imposter) => imposter,
        Err(e) => return e.into(),
    };
    let id = query.and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(name, _)| name == "instance")
            .map(|(_, value)| value.into_owned())
    });
    match imposter.wasm_snapshot(id.as_deref()).await {
        Ok(snapshot) => {
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json");
            if let Some(fuel) = snapshot.fuel_used {
                builder = builder.header("X-Rift-Fuel-Used", fuel);
            }
            builder
                .body(Full::new(Bytes::from(snapshot.body)))
                .unwrap_or_else(|e| {
                    error_response_typed(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ErrorKind::InternalError,
                        &format!("snapshot response: {e}"),
                    )
                })
        }
        Err(e) => {
            let (status, kind) = match &e {
                E::Ambiguous(_) => (StatusCode::BAD_REQUEST, ErrorKind::BadData),
                E::NotInstantiated | E::RequestMode => {
                    (StatusCode::CONFLICT, ErrorKind::ResourceConflict)
                }
                E::NoExport | E::Unavailable => (StatusCode::NOT_FOUND, ErrorKind::NoSuchResource),
                E::Guest(_) | E::NotJson | E::Trap(_) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, ErrorKind::ScriptError)
                }
                E::Busy => (StatusCode::GATEWAY_TIMEOUT, ErrorKind::ScriptTimeout),
            };
            let message = match &e {
                E::RequestMode => format!(
                    "{e}; read state with /admin/imposters/{port}/flow-state/{{space}}/{{key}}"
                ),
                _ => e.to_string(),
            };
            error_response_typed(status, kind, &message)
        }
    }
}
