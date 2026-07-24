//! Single-port gateway dispatch (issue #212) as a library function (issue #317), so any
//! listener — not just the admin router — can forward in-process traffic to an imposter.

use crate::imposter::{ImposterManager, handle_imposter_request};
use crate::response::error_response;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};

/// Dispatch `req` to the imposter on `port`, exactly as if it had arrived on the
/// imposter's own port. The request URI must already be imposter-relative (path + query
/// only — callers translating a prefixed form like `/__rift/:port/<path>` rewrite the URI
/// first). Returns a Mountebank-format 404 error response when no imposter is bound to
/// `port`. The imposter's recorded `request_from` is the loopback gateway address.
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
