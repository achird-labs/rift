//! Types for proxy recording - responses and request signatures.

use serde::{Deserialize, Serialize};

use crate::imposter::reconcile::Fnv1a;

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

/// Request signature for matching recorded responses.
///
/// Build one with [`RequestSignature::new`] and, where the body is part of the identity,
/// [`RequestSignature::with_body`]; the struct is `#[non_exhaustive]` so a new field is not a
/// breaking change.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RequestSignature {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    /// Filtered headers based on predicateGenerators
    pub headers: Vec<(String, String)>,
    /// FNV-1a 64 of the request body; `None` for a body-less request (issue #1317). Absent from
    /// the serialized form when `None`, so a body-less key serializes as it did before the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_hash: Option<u64>,
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
            body_hash: None,
        }
    }

    /// Fold the request body into the signature. An empty body leaves the signature unchanged.
    #[must_use]
    pub fn with_body(mut self, body: &[u8]) -> Self {
        if !body.is_empty() {
            let mut hasher = Fnv1a::default();
            hasher.update(body);
            self.body_hash = Some(hasher.finish());
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig() -> RequestSignature {
        RequestSignature::new("post", "/orders", Some("a=1"), &[])
    }

    #[test]
    fn an_empty_body_leaves_the_signature_unchanged() {
        assert_eq!(sig().with_body(b""), sig());
        assert_eq!(sig().with_body(b"").body_hash, None);
    }

    #[test]
    fn equal_bodies_give_equal_signatures() {
        assert_eq!(sig().with_body(b"abc"), sig().with_body(b"abc"));
    }

    #[test]
    fn different_bodies_give_different_signatures() {
        assert_ne!(sig().with_body(b"abc"), sig().with_body(b"abd"));
        assert_ne!(sig().with_body(b"abc"), sig());
    }

    #[test]
    fn a_body_less_signature_serializes_without_the_field() {
        let json = serde_json::to_value(sig()).expect("serialize");
        assert!(json.get("body_hash").is_none(), "{json}");
        let old: RequestSignature = serde_json::from_value(json).expect("old shape decodes");
        assert_eq!(old, sig());
    }

    #[test]
    fn the_body_hash_round_trips() {
        let with = sig().with_body(b"abc");
        let json = serde_json::to_value(&with).expect("serialize");
        assert!(json["body_hash"].is_u64(), "{json}");
        let back: RequestSignature = serde_json::from_value(json).expect("decode");
        assert_eq!(back, with);
    }
}
