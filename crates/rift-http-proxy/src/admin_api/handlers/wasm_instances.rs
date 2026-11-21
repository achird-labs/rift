//! `GET /imposters/:port/wasm/instances` (issue #1793): the `instance: "imposter"` wasm instances
//! an imposter keeps, one per component, `export` and `knobs` its stubs reference.

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
