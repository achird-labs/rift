//! Request handlers for the Admin API.

pub mod events;
pub mod imposters;
pub mod intercept;
pub mod scenarios;
pub mod stubs;
pub mod system;

/// Why [`decode_config_body`] produced no value.
#[derive(Debug)]
pub(crate) enum DecodeError {
    /// The body is not valid config: the client's fault, a 400.
    Invalid(serde_json::Error),
    /// The decode task panicked or was cancelled: the server's fault, never a 400.
    Internal(tokio::task::JoinError),
}

impl DecodeError {
    /// The admin response for this failure. `context` leads a 400's message, as each door words it
    /// (`Invalid stub JSON`).
    pub(crate) fn into_response(
        self,
        context: &str,
    ) -> hyper::Response<http_body_util::Full<hyper::body::Bytes>> {
        use crate::admin_api::types::error_response;
        use hyper::StatusCode;
        match self {
            Self::Invalid(e) => error_response(StatusCode::BAD_REQUEST, &format!("{context}: {e}")),
            // Cancelled: the runtime is shutting down, so the request can be retried elsewhere.
            Self::Internal(e) if e.is_cancelled() => error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "the server is shutting down; the request body was not decoded",
            ),
            Self::Internal(e) => {
                // Logged, not returned: a panic message may quote what was being decoded.
                tracing::error!(error = %e, "the admin request body decode panicked");
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the request body could not be decoded (internal error)",
                )
            }
        }
    }
}

/// Decode one admin request body that carries imposter or stub config, on the blocking pool and
/// under one [`AdmissionBudget`](rift_mock_core::proxy::client_identity::AdmissionBudget) (issue
/// #1609): admitting a `proxy` client key decrypts it, the keys in a single request share one
/// bound on that work, and a key repeated across its stubs is derived once. The scope is
/// thread-local, so it is entered inside the blocking task, around the decode only.
///
/// Takes the body as `Bytes` so the task shares the caller's buffer (a refcount, not a copy); the
/// caller keeps its `CollectedBody`, and with it the body-budget reservation, alive across the
/// await.
pub(crate) async fn decode_config_body<T>(body: hyper::body::Bytes) -> Result<T, DecodeError>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _budget = rift_mock_core::proxy::client_identity::AdmissionBudget::enter();
        serde_json::from_slice(&body)
    })
    .await
    .map_err(DecodeError::Internal)?
    .map_err(DecodeError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_config_is_a_400_naming_the_door() {
        let err = decode_config_body::<serde_json::Value>(hyper::body::Bytes::from_static(b"{"))
            .await
            .expect_err("not JSON");
        let response = err.into_response("Invalid stub JSON");
        assert_eq!(response.status(), hyper::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_panicked_decode_is_a_500_not_a_client_error() {
        let joined = tokio::task::spawn_blocking(|| panic!("boom")).await;
        let err = DecodeError::Internal(joined.expect_err("panicked"));
        let response = err.into_response("Invalid stub JSON");
        assert_eq!(response.status(), hyper::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn a_cancelled_decode_is_a_503() {
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        let err = DecodeError::Internal(task.await.expect_err("cancelled"));
        let response = err.into_response("Invalid stub JSON");
        assert_eq!(response.status(), hyper::StatusCode::SERVICE_UNAVAILABLE);
    }
}
