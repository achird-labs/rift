//! Types for proxy recording - responses and request signatures.

use serde::{Deserialize, Serialize};

/// Recorded response from proxy.
///
/// Headers are stored as `Vec<(String, String)>` to preserve multi-valued
/// headers (e.g., multiple `Set-Cookie` headers) that would be lost with a HashMap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub latency_ms: Option<u64>,
    /// Unix timestamp in seconds
    pub timestamp_secs: u64,
}

/// Request signature for matching recorded responses
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestSignature {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    /// Filtered headers based on predicateGenerators
    pub headers: Vec<(String, String)>,
}

impl RequestSignature {
    /// Create signature from request components
    pub fn new(
        method: &str,
        path: &str,
        query: Option<&str>,
        headers: &[(String, String)],
    ) -> Self {
        Self {
            method: method.to_uppercase(),
            path: path.to_string(),
            query: query.map(|s| s.to_string()),
            headers: headers.to_vec(),
        }
    }
}
