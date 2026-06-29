use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::http::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Response, StatusCode};
use std::convert::Infallible;
use std::str::FromStr;

pub struct ErrorResponseBuilder {
    status: StatusCode,
    body: Option<String>,
    headers: HeaderMap,
}

impl ErrorResponseBuilder {
    pub fn new(status_code: StatusCode) -> Self {
        ErrorResponseBuilder {
            status: status_code,
            body: None,
            headers: Default::default(),
        }
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        match (HeaderName::from_str(name), HeaderValue::from_str(value)) {
            (Ok(name), Ok(value)) => {
                self.headers.insert(name, value);
                self
            }
            _ => self,
        }
    }

    pub fn merge_headers<H, K, V>(mut self, headers: H) -> Self
    where
        H: IntoIterator<Item = (K, V)>,
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
    {
        for (key, value) in headers {
            if let (Ok(name), Ok(value)) = (HeaderName::try_from(key), HeaderValue::try_from(value))
            {
                self.headers.insert(name, value);
            }
        }
        self
    }

    pub fn build_full(self) -> Response<Full<Bytes>> {
        let payload = self.body.map(Bytes::from).unwrap_or_default();
        let body = Full::new(payload);

        let mut response = Response::builder().status(self.status).body(body).unwrap();

        response.headers_mut().extend(self.headers);
        response
    }

    pub fn build_boxed(self) -> Response<BoxBody<Bytes, hyper::Error>> {
        let payload = self.body.map(Bytes::from).unwrap_or_default();
        let body = Full::new(payload)
            .map_err(|never: Infallible| match never {})
            .boxed();

        let mut response = Response::builder().status(self.status).body(body).unwrap();

        response.headers_mut().extend(self.headers);
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::{HeaderValue, CONTENT_TYPE};
    use hyper::StatusCode;

    #[test]
    fn test_builder_with_status() {
        let response = ErrorResponseBuilder::new(StatusCode::OK).build_boxed();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_builder_with_headers() {
        let response = ErrorResponseBuilder::new(StatusCode::OK)
            .header("X-Custom-Header", "test-value")
            .header("Content-Type", "application/json")
            .build_boxed();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("X-Custom-Header"),
            Some(&HeaderValue::from_static("test-value"))
        );
        assert_eq!(
            response.headers().get(CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/json"))
        );
    }

    #[test]
    fn test_merge_headers() {
        let name = HeaderName::from_str("key_A").unwrap();
        let value = HeaderValue::from_str("value_A").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(name, value);

        let response = ErrorResponseBuilder::new(StatusCode::OK)
            .header("key_B", "value_B")
            .merge_headers(&headers)
            .build_boxed();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("key_A"),
            Some(&HeaderValue::from_static("value_A"))
        );
        assert_eq!(
            response.headers().get("key_B"),
            Some(&HeaderValue::from_static("value_B"))
        );
    }

    #[test]
    fn test_merge_multiple_headers() {
        let name = HeaderName::from_str("key_A").unwrap();
        let name1 = HeaderName::from_str("key_C").unwrap();
        let value = HeaderValue::from_str("value_A").unwrap();
        let value1 = HeaderValue::from_str("value_C").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(name, value);

        let mut headers1 = HeaderMap::new();
        headers1.insert(name1, value1);

        let response = ErrorResponseBuilder::new(StatusCode::OK)
            .header("key_B", "value_B")
            .merge_headers(&headers)
            .merge_headers(&headers1)
            .build_boxed();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("key_A"),
            Some(&HeaderValue::from_static("value_A"))
        );
        assert_eq!(
            response.headers().get("key_B"),
            Some(&HeaderValue::from_static("value_B"))
        );
        assert_eq!(
            response.headers().get("key_C"),
            Some(&HeaderValue::from_static("value_C"))
        );
    }
}
