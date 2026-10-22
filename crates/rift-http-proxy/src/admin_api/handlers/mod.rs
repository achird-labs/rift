//! Request handlers for the Admin API.

pub mod events;
pub mod imposters;
pub mod intercept;
pub mod scenarios;
pub mod stubs;
pub mod system;

/// Decode one admin request body that carries imposter or stub config, under one
/// [`AdmissionBudget`](rift_mock_core::proxy::client_identity::AdmissionBudget) (issue #1609): the
/// `proxy` client keys in a single request share one bound on key derivation, and a key repeated
/// across its stubs is derived once. The decode runs on the async worker; the scope is
/// thread-local, so it is entered and dropped here, around the synchronous decode only.
pub(crate) fn decode_config_body<T: serde::de::DeserializeOwned>(
    body: &[u8],
) -> serde_json::Result<T> {
    let _budget = rift_mock_core::proxy::client_identity::AdmissionBudget::enter();
    serde_json::from_slice(body)
}
