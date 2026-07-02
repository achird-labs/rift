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
