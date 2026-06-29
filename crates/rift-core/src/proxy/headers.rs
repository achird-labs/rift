//! Safe header insertion helpers.
//!
//! This module provides compile-time safe header names and values for Rift's
//! custom headers, eliminating runtime `.parse().unwrap()` calls.
//!
//! The extension trait methods accept references and handle cloning internally,
//! keeping call sites clean while the clones themselves are cheap (just copying
//! a pointer for `from_static` headers).

use hyper::header::{HeaderName, HeaderValue};
use hyper::http::response::Parts;
use hyper::Response;

// Static header names for Rift custom headers
pub static X_RIFT_FAULT: HeaderName = HeaderName::from_static("x-rift-fault");
pub static X_RIFT_RULE_ID: HeaderName = HeaderName::from_static("x-rift-rule-id");
pub static X_RIFT_SCRIPT: HeaderName = HeaderName::from_static("x-rift-script");
pub static X_RIFT_LATENCY_MS: HeaderName = HeaderName::from_static("x-rift-latency-ms");
pub static X_RIFT_TCP_FAULT: HeaderName = HeaderName::from_static("x-rift-tcp-fault");
pub static X_RIFT_PROXIED: HeaderName = HeaderName::from_static("x-rift-proxied");
pub static X_RIFT_RECORDED: HeaderName = HeaderName::from_static("x-rift-recorded");
pub static X_RIFT_REPLAYED: HeaderName = HeaderName::from_static("x-rift-replayed");
pub static X_RIFT_BEHAVIOR_WAIT: HeaderName = HeaderName::from_static("x-rift-behavior-wait");
pub static X_RIFT_BEHAVIOR_COPY: HeaderName = HeaderName::from_static("x-rift-behavior-copy");
pub static X_RIFT_BEHAVIOR_LOOKUP: HeaderName = HeaderName::from_static("x-rift-behavior-lookup");
pub static X_RIFT_BEHAVIOR_SHELL: HeaderName = HeaderName::from_static("x-rift-behavior-shell");
pub static X_RIFT_BEHAVIOR_DECORATE: HeaderName =
    HeaderName::from_static("x-rift-behavior-decorate");

// Static header values for common Rift values
pub static VALUE_TRUE: HeaderValue = HeaderValue::from_static("true");
pub static VALUE_ERROR: HeaderValue = HeaderValue::from_static("error");
pub static VALUE_LATENCY: HeaderValue = HeaderValue::from_static("latency");
pub static VALUE_TCP: HeaderValue = HeaderValue::from_static("tcp");

/// Extension trait for inserting Rift headers into responses.
pub trait RiftHeadersExt {
    /// Insert a header with a static name and value.
    /// Accepts references; cloning is handled internally (cheap for `from_static` headers).
    fn set_header(&mut self, name: &HeaderName, value: &HeaderValue);

    /// Insert a header with a static name and dynamic string value.
    /// Returns false if the value couldn't be converted to a valid header value.
    fn set_header_value(&mut self, name: &HeaderName, value: &str) -> bool;
}

impl<B> RiftHeadersExt for Response<B> {
    fn set_header(&mut self, name: &HeaderName, value: &HeaderValue) {
        self.headers_mut().insert(name.clone(), value.clone());
    }

    fn set_header_value(&mut self, name: &HeaderName, value: &str) -> bool {
        match HeaderValue::from_str(value) {
            Ok(header_value) => {
                self.headers_mut().insert(name.clone(), header_value);
                true
            }
            Err(_) => false,
        }
    }
}

impl RiftHeadersExt for Parts {
    fn set_header(&mut self, name: &HeaderName, value: &HeaderValue) {
        self.headers.insert(name.clone(), value.clone());
    }

    fn set_header_value(&mut self, name: &HeaderName, value: &str) -> bool {
        match HeaderValue::from_str(value) {
            Ok(header_value) => {
                self.headers.insert(name.clone(), header_value);
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;
    use hyper::body::Bytes;

    #[test]
    fn test_static_header_names() {
        assert_eq!(X_RIFT_FAULT.as_str(), "x-rift-fault");
        assert_eq!(X_RIFT_RULE_ID.as_str(), "x-rift-rule-id");
        assert_eq!(X_RIFT_PROXIED.as_str(), "x-rift-proxied");
    }

    #[test]
    fn test_static_header_values() {
        assert_eq!(VALUE_TRUE.to_str().unwrap(), "true");
        assert_eq!(VALUE_ERROR.to_str().unwrap(), "error");
    }

    #[test]
    fn test_set_header_static() {
        let mut response = Response::new(Full::new(Bytes::new()));
        response.set_header(&X_RIFT_FAULT, &VALUE_ERROR);
        assert_eq!(response.headers().get(&X_RIFT_FAULT).unwrap(), "error");
    }

    #[test]
    fn test_set_header_value_valid() {
        let mut response = Response::new(Full::new(Bytes::new()));
        assert!(response.set_header_value(&X_RIFT_RULE_ID, "test-rule-123"));
        assert_eq!(
            response.headers().get(&X_RIFT_RULE_ID).unwrap(),
            "test-rule-123"
        );
    }

    #[test]
    fn test_set_header_value_numeric() {
        let mut response = Response::new(Full::new(Bytes::new()));
        let latency = 500u64;
        assert!(response.set_header_value(&X_RIFT_LATENCY_MS, &latency.to_string()));
        assert_eq!(response.headers().get(&X_RIFT_LATENCY_MS).unwrap(), "500");
    }

    #[test]
    fn test_set_header_value_invalid() {
        let mut response = Response::new(Full::new(Bytes::new()));
        // Header values can't contain certain characters like newlines
        assert!(!response.set_header_value(&X_RIFT_RULE_ID, "invalid\nvalue"));
    }
}
