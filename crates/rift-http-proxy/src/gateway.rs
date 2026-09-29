//! Single-port gateway dispatch (issue #212) as a library function (issue #317), so any
//! listener — not just the admin router — can forward in-process traffic to an imposter.
//!
//! [`dispatch_to_port`] returns the imposter's response, including a TCP-fault *carrier*, as is.
//! A listener that owns the client connection turns a carrier into the real fault with
//! [`apply_tcp_fault`] (issue #1234); the admin API's `/__rift/` route and the front door both do.

use crate::imposter::{ImposterManager, handle_imposter_request};
use crate::response::error_response;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode, Version};
use rift_mock_core::{FaultCell, InjectedFault, TcpFaultKind};

/// Apply a TCP-fault carrier on the connection it is about to be written to (issue #1234).
///
/// For an ordinary response this returns it unchanged. For a carrier (see
/// [`tcp_fault_carrier`](crate::tcp_fault_carrier)):
///
/// - **HTTP/1**: arms `cell` and hands the carrier back. The listener must have wrapped the
///   connection in [`FaultIo::new`](crate::FaultIo::new) with this `cell`; `FaultIo` then performs
///   the fault on hyper's write of the carrier, exactly as on the imposter's own port.
/// - **HTTP/2**: returns `Err`, and the service returns it, so hyper resets that one stream
///   (`RST_STREAM`, `INTERNAL_ERROR`) for every fault kind. The cell is never armed: the next write
///   may belong to another stream, and a socket abort would kill every stream on the connection.
///
/// `version` is the request's, read before the request is consumed. Never return the `Err` on
/// HTTP/1 instead: hyper would close without writing, `FaultIo` would never trip, and every kind
/// would degrade to an empty close.
pub fn apply_tcp_fault<B>(
    response: Response<B>,
    version: Version,
    cell: &FaultCell,
) -> Result<Response<B>, InjectedFault> {
    let Some(kind) = response.extensions().get::<TcpFaultKind>().copied() else {
        return Ok(response);
    };
    if matches!(
        version,
        Version::HTTP_09 | Version::HTTP_10 | Version::HTTP_11
    ) {
        cell.arm(kind);
        Ok(response)
    } else {
        Err(InjectedFault(kind))
    }
}

/// Dispatch `req` to the imposter on `port`, exactly as if it had arrived on the
/// imposter's own port. The request URI must already be imposter-relative (path + query
/// only — callers translating a prefixed form like `/__rift/:port/<path>` rewrite the URI
/// first). Returns a Mountebank-format 404 error response when no imposter is bound to
/// `port`. The imposter's recorded `request_from` is the loopback gateway address.
///
/// A TCP-fault stub yields its carrier response untouched: this function has no connection to
/// abort. The calling listener applies it with [`apply_tcp_fault`], or classifies it with
/// [`tcp_fault_carrier`](crate::tcp_fault_carrier) when it answers in-process.
pub async fn dispatch_to_port(
    manager: &ImposterManager,
    port: u16,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let Ok(imposter) = manager.get_imposter(port) else {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("no imposter on port {port}"),
        );
    };

    // The gateway is the imposter's client; recorded `request_from` reflects the loopback gateway.
    let gateway_addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    match handle_imposter_request(req, imposter, gateway_addr).await {
        Ok(resp) => resp,
        Err(e) => match e {}, // handle_imposter_request is Infallible
    }
}

/// Parse and dispatch a `/__rift/:port/<path>` gateway request (issue #212): `rest` is everything
/// after the `/__rift/` prefix. Rewrites the URI to the imposter-relative `/<path>` (+ query) so
/// the imposter's predicates and recorded requests see it exactly as if it had arrived on its own
/// port, then calls [`dispatch_to_port`].
///
/// Shared by the admin API's `/__rift/` route and the front door's fallback addressing (issue
/// #19 / U-11) so the two listeners cannot drift on what counts as a valid gateway target.
pub async fn dispatch_gateway_path(
    rest: &str,
    query: Option<&str>,
    req: Request<Incoming>,
    manager: &ImposterManager,
) -> Response<Full<Bytes>> {
    let (port_str, sub_path) = match rest.split_once('/') {
        Some((port, sub)) => (port, format!("/{sub}")),
        None => (rest, "/".to_string()),
    };
    let Ok(port) = port_str.parse::<u16>() else {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("invalid gateway target '{port_str}' (expected /__rift/<port>/<path>)"),
        );
    };
    // Check existence before the URI rewrite so a missing imposter stays a 404 even if the
    // rewritten URI would be rejected — the pre-#317 response precedence. dispatch_to_port
    // re-checks as its own defensive 404 for other callers.
    if manager.get_imposter(port).is_err() {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("no imposter on port {port}"),
        );
    }

    let target = match query {
        Some(q) => format!("{sub_path}?{q}"),
        None => sub_path,
    };
    let (mut parts, body) = req.into_parts();
    parts.uri = match target.parse() {
        Ok(uri) => uri,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid gateway path"),
    };

    dispatch_to_port(manager, port, Request::from_parts(parts, body)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn carrier(kind: TcpFaultKind) -> Response<()> {
        let mut response = Response::new(());
        response.extensions_mut().insert(kind);
        response
    }

    #[test]
    fn h1_carrier_arms_the_cell_and_is_handed_back() {
        for version in [Version::HTTP_10, Version::HTTP_11] {
            let cell = FaultCell::new();
            let out = apply_tcp_fault(carrier(TcpFaultKind::Reset), version, &cell)
                .expect("h1 hands the carrier back for FaultIo to trip on");
            assert!(crate::tcp_fault_carrier(&out).is_some());
            // Armed: the FaultIo sharing this cell trips on the carrier's write. Observed through
            // the Debug form, since taking the slot is FaultIo's alone.
            assert!(format!("{cell:?}").contains("Reset"), "{cell:?}");
        }
    }

    #[test]
    fn h2_carrier_is_an_error_and_never_arms_the_cell() {
        let cell = FaultCell::new();
        let err = apply_tcp_fault(
            carrier(TcpFaultKind::MalformedChunk),
            Version::HTTP_2,
            &cell,
        )
        .expect_err("h2 resets the stream");
        assert_eq!(err, InjectedFault(TcpFaultKind::MalformedChunk));
        assert!(crate::is_injected_fault(&err));
        assert!(format!("{cell:?}").contains("None"), "{cell:?}");
    }

    #[test]
    fn an_ordinary_response_passes_through_on_any_version() {
        for version in [Version::HTTP_11, Version::HTTP_2] {
            let cell = FaultCell::new();
            let out = apply_tcp_fault(Response::new("body"), version, &cell).expect("untouched");
            assert_eq!(*out.body(), "body");
            assert!(format!("{cell:?}").contains("None"), "{cell:?}");
        }
    }
}
