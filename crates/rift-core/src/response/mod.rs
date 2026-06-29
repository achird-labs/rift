pub mod builder;

use crate::imposter::ImposterError;
use crate::response::builder::ErrorResponseBuilder;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use serde::Serialize;

/// Error response structure (Mountebank format).
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub errors: Vec<ErrorDetail>,
}

/// Individual error detail.
#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

/// Create a Mountebank-format error response.
pub fn error_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let error = ErrorResponse {
        errors: vec![ErrorDetail {
            code: status.as_str().to_string(),
            message: message.to_string(),
        }],
    };
    let json = serde_json::to_string_pretty(&error).unwrap_or_else(|_| "{}".to_string());
    ErrorResponseBuilder::new(status)
        .body(json)
        .header("Content-Type", "application/json")
        .build_full()
}

/// Convert an `ImposterError` into an HTTP response so handlers can use `e.into()`.
impl From<ImposterError> for Response<Full<Bytes>> {
    fn from(err: ImposterError) -> Self {
        match err {
            ImposterError::PortInUse(p) => error_response(
                StatusCode::BAD_REQUEST,
                &format!("Port {p} is already in use"),
            ),
            ImposterError::NotFound(p) => error_response(
                StatusCode::NOT_FOUND,
                &format!("Imposter not found on port {p}"),
            ),
            ImposterError::BindError(p, e) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Failed to bind port {p}: {e}"),
            ),
            ImposterError::InvalidProtocol(p) => {
                error_response(StatusCode::BAD_REQUEST, &format!("Invalid protocol: {p}"))
            }
            ImposterError::StubIndexOutOfBounds(i) => {
                error_response(StatusCode::NOT_FOUND, &format!("Stub index {i} not found"))
            }
            ImposterError::PersistError(msg) => error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("Persistence error: {msg}"),
            ),
        }
    }
}
