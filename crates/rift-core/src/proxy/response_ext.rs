//! Response extension traits for common transformations.
//!
//! This module provides extension traits for working with `Response` types,
//! particularly for converting between different body types.

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::Response;
use std::convert::Infallible;

/// Extension trait for `Response<Full<Bytes>>` providing common transformations.
pub trait ResponseExt {
    /// Convert the response body into a boxed body type.
    ///
    /// This method wraps a `Response<Full<Bytes>>` into a
    /// `Response<BoxBody<Bytes, hyper::Error>>`, which is commonly needed
    /// when returning responses from handlers.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use crate::proxy::response_ext::ResponseExt;
    ///
    /// let response = Response::new(Full::new(Bytes::from("hello")));
    /// let boxed = response.into_boxed();
    /// ```
    fn into_boxed(self) -> Response<BoxBody<Bytes, hyper::Error>>;
}

impl ResponseExt for Response<Full<Bytes>> {
    fn into_boxed(self) -> Response<BoxBody<Bytes, hyper::Error>> {
        self.map(|b| BoxBody::new(b.map_err(|never: Infallible| match never {})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_into_boxed_preserves_status() {
        let response = Response::builder()
            .status(404)
            .body(Full::new(Bytes::from("not found")))
            .unwrap();

        let boxed = response.into_boxed();
        assert_eq!(boxed.status(), 404);
    }

    #[test]
    fn test_into_boxed_preserves_headers() {
        let response = Response::builder()
            .header("X-Custom", "value")
            .body(Full::new(Bytes::from("test")))
            .unwrap();

        let boxed = response.into_boxed();
        assert_eq!(
            boxed.headers().get("X-Custom").map(|v| v.to_str().unwrap()),
            Some("value")
        );
    }

    #[test]
    fn test_into_boxed_empty_body() {
        let response = Response::new(Full::new(Bytes::new()));
        let boxed = response.into_boxed();
        assert_eq!(boxed.status(), 200);
    }
}
