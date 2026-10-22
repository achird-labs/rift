//! Request handlers for the Admin API.

pub mod events;
pub mod imposters;
pub mod intercept;
pub mod scenarios;
pub mod stubs;
pub mod system;

/// Decode one admin request body that carries imposter or stub config, on the blocking pool and
/// under one [`AdmissionBudget`](rift_mock_core::proxy::client_identity::AdmissionBudget) (issue
/// #1609): admitting a `proxy` client key decrypts it, the keys in a single request share one
/// bound on that work, and a key repeated across its stubs is derived once. The scope is
/// thread-local, so it is entered inside the blocking task, around the decode only.
pub(crate) async fn decode_config_body<T>(body: &[u8]) -> serde_json::Result<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    // One copy of a body already held to the admin body limit: the task must own what it reads.
    let body = body.to_vec();
    match tokio::task::spawn_blocking(move || {
        let _budget = rift_mock_core::proxy::client_identity::AdmissionBudget::enter();
        serde_json::from_slice(&body)
    })
    .await
    {
        Ok(decoded) => decoded,
        // The decode panicked or the runtime is shutting down: answered as a body the server could
        // not read, which is what a 400 from this path says, never as a success.
        Err(e) => Err(serde::de::Error::custom(format!(
            "the request body could not be decoded: {e}"
        ))),
    }
}
