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
    /// The stable symbolic error type (issue #797). Unlike [`code`](Self::code) — which is a status
    /// string on most doors and a Mountebank slug on a few, frozen that way for compatibility —
    /// this is *always* a slug, on every door. New consumers should read this field.
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

/// The symbolic type of an error, independent of its HTTP status (issue #797).
///
/// An enum rather than free strings so a typo is a compile error and the whole slug set stays
/// enumerable for the docs and the pinning tests. The first six variants are Mountebank's own
/// error types, copied verbatim from its `src/util/errors.js` (2.9.1) — a client that already maps
/// Mountebank error types keeps working. The rest name doors Mountebank does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    // --- Mountebank-canonical (src/util/errors.js) ---
    BadData,
    InvalidInjection,
    ResourceConflict,
    InsufficientAccess,
    NoSuchResource,
    Unauthorized,
    // --- Rift doors Mountebank has no equivalent for ---
    InvalidPredicateInjection,
    InjectionTimeout,
    PredicateInjectionTimeout,
    ScriptError,
    ScriptTimeout,
    BehaviorError,
    ImposterDisabled,
    RequestTooLarge,
    UpstreamFailure,
    Unavailable,
    Timeout,
    InternalError,
    ClientError,
    ServerError,
}

impl ErrorKind {
    /// Every variant as of writing — keep in sync by hand when adding one. The exhaustive,
    /// wildcard-free match in [`slug`](Self::slug) is the half of the guard the compiler enforces;
    /// this array is what lets the shape test iterate them.
    pub const ALL: [ErrorKind; 20] = [
        ErrorKind::BadData,
        ErrorKind::InvalidInjection,
        ErrorKind::ResourceConflict,
        ErrorKind::InsufficientAccess,
        ErrorKind::NoSuchResource,
        ErrorKind::Unauthorized,
        ErrorKind::InvalidPredicateInjection,
        ErrorKind::InjectionTimeout,
        ErrorKind::PredicateInjectionTimeout,
        ErrorKind::ScriptError,
        ErrorKind::ScriptTimeout,
        ErrorKind::BehaviorError,
        ErrorKind::ImposterDisabled,
        ErrorKind::RequestTooLarge,
        ErrorKind::UpstreamFailure,
        ErrorKind::Unavailable,
        ErrorKind::Timeout,
        ErrorKind::InternalError,
        ErrorKind::ClientError,
        ErrorKind::ServerError,
    ];

    /// The wire slug. Lowercase, space-separated; pinned by tests.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            ErrorKind::BadData => "bad data",
            ErrorKind::InvalidInjection => "invalid injection",
            ErrorKind::ResourceConflict => "resource conflict",
            ErrorKind::InsufficientAccess => "insufficient access",
            ErrorKind::NoSuchResource => "no such resource",
            ErrorKind::Unauthorized => "unauthorized",
            ErrorKind::InvalidPredicateInjection => "invalid predicate injection",
            ErrorKind::InjectionTimeout => "injection timeout",
            ErrorKind::PredicateInjectionTimeout => "predicate injection timeout",
            ErrorKind::ScriptError => "script error",
            ErrorKind::ScriptTimeout => "script timeout",
            ErrorKind::BehaviorError => "behavior error",
            ErrorKind::ImposterDisabled => "imposter disabled",
            ErrorKind::RequestTooLarge => "request too large",
            ErrorKind::UpstreamFailure => "upstream failure",
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::Timeout => "timeout",
            ErrorKind::InternalError => "internal error",
            ErrorKind::ClientError => "client error",
            ErrorKind::ServerError => "server error",
        }
    }

    /// The kind a door gets when it does not name one — the ~85 call sites that reach
    /// [`error_body`] with only a status. The unlisted-status fallback is the status *class*, never
    /// the status string: `type` must never be something a client could mistake for `code`.
    #[must_use]
    pub fn for_status(status: StatusCode) -> Self {
        match status {
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => ErrorKind::BadData,
            StatusCode::UNAUTHORIZED => ErrorKind::Unauthorized,
            StatusCode::FORBIDDEN => ErrorKind::InsufficientAccess,
            StatusCode::NOT_FOUND => ErrorKind::NoSuchResource,
            StatusCode::CONFLICT => ErrorKind::ResourceConflict,
            StatusCode::PAYLOAD_TOO_LARGE => ErrorKind::RequestTooLarge,
            StatusCode::INTERNAL_SERVER_ERROR => ErrorKind::InternalError,
            StatusCode::BAD_GATEWAY => ErrorKind::UpstreamFailure,
            StatusCode::SERVICE_UNAVAILABLE => ErrorKind::Unavailable,
            StatusCode::GATEWAY_TIMEOUT => ErrorKind::Timeout,
            s if s.is_client_error() => ErrorKind::ClientError,
            _ => ErrorKind::ServerError,
        }
    }
}

/// The serialized body of a Mountebank-format error, for callers that need to attach their own
/// headers and so cannot use [`error_response`]'s finished response (issue #679).
///
/// The `unwrap_or_else` is a terminal last resort, not a swallow: `ErrorResponse` is plain `String`
/// fields, so serde has no data-dependent way to fail here.
pub(crate) fn error_body(status: StatusCode, message: &str) -> String {
    error_body_typed(status, ErrorKind::for_status(status), message)
}

/// [`error_body`] for a door that names its own [`ErrorKind`], because the status alone is too
/// coarse to identify it — a 500 from a failing script and a 500 from a broken response builder
/// are the same status but different doors (issue #797).
///
/// `code` is *not* affected by the kind: it stays the status string, so naming a kind can never
/// change what an existing client reads.
pub(crate) fn error_body_typed(status: StatusCode, kind: ErrorKind, message: &str) -> String {
    let error = ErrorResponse {
        errors: vec![ErrorDetail {
            code: status.as_str().to_string(),
            error_type: kind.slug().to_string(),
            message: message.to_string(),
        }],
    };
    serde_json::to_string_pretty(&error).unwrap_or_else(|_| "{}".to_string())
}

/// Create a Mountebank-format error response.
pub fn error_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    error_response_typed(status, ErrorKind::for_status(status), message)
}

/// [`error_response`] for a door that names its own [`ErrorKind`] (issue #797).
pub fn error_response_typed(
    status: StatusCode,
    kind: ErrorKind,
    message: &str,
) -> Response<Full<Bytes>> {
    ErrorResponseBuilder::new(status)
        .body(error_body_typed(status, kind, message))
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
            ImposterError::BindError(p, e) => {
                tracing::error!(port = p, error = %format_args!("{e:#}"), "failed to bind imposter port");
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Failed to bind port {p}: {e:#}"),
                )
            }
            ImposterError::InvalidProtocol(p) => {
                error_response(StatusCode::BAD_REQUEST, &format!("Invalid protocol: {p}"))
            }
            ImposterError::StubIndexOutOfBounds(i) => {
                error_response(StatusCode::NOT_FOUND, &format!("Stub index {i} not found"))
            }
            ImposterError::StubNotFound(id) => {
                error_response(StatusCode::NOT_FOUND, &format!("No stub with id '{id}'"))
            }
            ImposterError::StubIdConflict(id) => error_response(
                StatusCode::CONFLICT,
                &format!("A stub with id '{id}' already exists"),
            ),
            ImposterError::PersistError(msg) => {
                tracing::error!(error = %format_args!("{msg:#}"), "failed to persist imposter state");
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &format!("Persistence error: {msg:#}"),
                )
            }
            ImposterError::Tls(msg) => error_response(
                StatusCode::BAD_REQUEST,
                &format!("TLS configuration error: {msg}"),
            ),
            // A misconfigured/unavailable explicitly-requested flow-store backend is an operator
            // config error (issue #325), mirroring the TLS case → 400, not a 500.
            ImposterError::FlowStoreConfig(msg) => error_response(
                StatusCode::BAD_REQUEST,
                &format!("Flow store configuration error: {msg}"),
            ),
            ImposterError::Backend(e) => crate::extensions::decorate::backend_error_response(&e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_of(status: StatusCode) -> serde_json::Value {
        serde_json::from_str(&error_body(status, "msg")).expect("valid json")
    }

    // AC2: the status → default-slug table IS the specification for every door that does not name
    // its own kind (~85 call sites reach `error_body` without one). Pinning it here is what stops a
    // door's `type` changing as a side effect of an unrelated edit.
    #[test]
    fn status_to_default_slug_table_is_pinned() {
        let expected = [
            (StatusCode::BAD_REQUEST, "bad data"),
            (StatusCode::UNPROCESSABLE_ENTITY, "bad data"),
            (StatusCode::UNAUTHORIZED, "unauthorized"),
            (StatusCode::FORBIDDEN, "insufficient access"),
            (StatusCode::NOT_FOUND, "no such resource"),
            (StatusCode::CONFLICT, "resource conflict"),
            (StatusCode::PAYLOAD_TOO_LARGE, "request too large"),
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
            (StatusCode::BAD_GATEWAY, "upstream failure"),
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            (StatusCode::GATEWAY_TIMEOUT, "timeout"),
            // Unlisted statuses fall back to the class, never to a status string.
            (StatusCode::IM_A_TEAPOT, "client error"),
            (StatusCode::NOT_IMPLEMENTED, "server error"),
        ];
        for (status, slug) in expected {
            assert_eq!(
                ErrorKind::for_status(status).slug(),
                slug,
                "default slug for {status}"
            );
        }
    }

    // The six slugs Rift shares with Mountebank are copied from its `src/util/errors.js` (2.9.1),
    // not invented. Drift here is a silent compatibility break for any client that maps Mountebank
    // error types, so the wording is pinned verbatim.
    #[test]
    fn mountebank_canonical_slugs_are_verbatim() {
        assert_eq!(ErrorKind::BadData.slug(), "bad data");
        assert_eq!(ErrorKind::InvalidInjection.slug(), "invalid injection");
        assert_eq!(ErrorKind::ResourceConflict.slug(), "resource conflict");
        assert_eq!(ErrorKind::InsufficientAccess.slug(), "insufficient access");
        assert_eq!(ErrorKind::NoSuchResource.slug(), "no such resource");
        assert_eq!(ErrorKind::Unauthorized.slug(), "unauthorized");
    }

    // Every slug is lowercase, space-separated, non-empty — the shape clients pattern-match on.
    #[test]
    fn every_slug_is_a_lowercase_space_separated_token() {
        for kind in ErrorKind::ALL {
            let s = kind.slug();
            assert!(!s.is_empty(), "{kind:?} has an empty slug");
            assert!(
                s.chars().all(|c| c.is_ascii_lowercase() || c == ' '),
                "{kind:?} slug {s:?} must be lowercase words separated by single spaces"
            );
            assert!(
                !s.starts_with(' ') && !s.ends_with(' ') && !s.contains("  "),
                "{kind:?} slug {s:?} has stray whitespace"
            );
        }
    }

    // AC1/invariant 2, the non-breakage proof: `code` stays the status string on every
    // status-derived door, byte for byte, while `type` is added alongside it.
    #[test]
    fn error_body_adds_type_and_leaves_code_as_the_status_string() {
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::PAYLOAD_TOO_LARGE,
            StatusCode::BAD_GATEWAY,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            let body = body_of(status);
            assert_eq!(
                body["errors"][0]["code"],
                status.as_str(),
                "code must remain the status string for {status}"
            );
            assert_eq!(
                body["errors"][0]["type"],
                ErrorKind::for_status(status).slug(),
                "type must be the default slug for {status}"
            );
            assert_eq!(body["errors"][0]["message"], "msg");
        }
    }

    // A door that names its own kind overrides `type` only — `code` is still the status string, so
    // naming a kind can never change what a 0.14.0 client reads.
    #[test]
    fn typed_body_overrides_type_but_never_code() {
        let raw = error_body_typed(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::ScriptError,
            "boom",
        );
        let body: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        assert_eq!(body["errors"][0]["code"], "500");
        assert_eq!(body["errors"][0]["type"], "script error");
        assert_ne!(
            body["errors"][0]["type"],
            ErrorKind::for_status(StatusCode::INTERNAL_SERVER_ERROR).slug(),
            "this test is only meaningful if the named kind differs from the status default"
        );
    }

    // The field is serialized as `type`, not the Rust identifier.
    #[test]
    fn the_field_is_named_type_on_the_wire() {
        let raw = error_body(StatusCode::NOT_FOUND, "nope");
        assert!(raw.contains("\"type\""), "wire field must be `type`: {raw}");
        assert!(
            !raw.contains("error_type"),
            "the Rust field name must not leak: {raw}"
        );
    }
}
