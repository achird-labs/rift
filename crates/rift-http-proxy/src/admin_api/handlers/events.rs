//! Admin SSE stream handler (issue #461): `GET /events` and the `GET
//! /imposters/{port}/savedRequests/stream` alias. Subscribes to the manager's `AdminEventBus` and
//! forwards recorded-request + imposter-lifecycle events as Server-Sent Events, filtered by
//! `types`/`port`/`match`, with a heartbeat and lossy-but-loud backpressure (`lagged` events). The
//! stream is a push upgrade of `GET /savedRequests` polling; a gap/`lagged` means "reconcile via
//! polling" — v1 does not replay.

use bytes::Bytes;
use http_body_util::{
    BodyExt, Full,
    channel::{Channel, Sender},
    combinators::BoxBody,
};
use hyper::{Response, StatusCode};
use rift_mock_core::imposter::{AdminEvent, AdminEventKind, ImposterManager};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use crate::admin_api::request_filter::{MatchClause, parse_match_clauses};

/// The boxed body type the admin service unifies over (a `Full` error response or a streaming
/// `Channel`), so `route_request`'s `Response<Full<Bytes>>` and this streaming response share a type.
pub(crate) type AdminBody = BoxBody<Bytes, hyper::Error>;

const HEARTBEAT: Duration = Duration::from_secs(15);
/// Write-side buffer for the SSE channel; a client this far behind on the socket causes the
/// forwarder to block on `send_data`, which in turn lets the broadcast bus lag (→ `lagged`).
const CHANNEL_BUFFER: usize = 16;

/// Parsed `/events` query parameters.
struct StreamParams {
    requests: bool,
    lifecycle: bool,
    port: Option<u16>,
    clauses: Vec<MatchClause>,
}

/// Classify an admin path as a stream target: `Some(None)` for `/events`, `Some(Some(port))` for the
/// `/imposters/{port}/savedRequests/stream` alias, `None` otherwise.
pub(crate) fn stream_target(path: &str) -> Option<Option<u16>> {
    if path == "/events" {
        return Some(None);
    }
    let rest = path.strip_prefix("/imposters/")?;
    let segments: Vec<&str> = rest.split('/').collect();
    match segments.as_slice() {
        [port, "savedRequests", "stream"] => port.parse::<u16>().ok().map(Some),
        _ => None,
    }
}

/// Handle a stream request. `forced_port` is `Some` for the alias (which pre-binds the port and
/// streams only request events). Returns a 400/404 error body on invalid params/unknown port,
/// otherwise a `text/event-stream` whose forwarder task runs until the client disconnects or
/// `cancel` fires (server shutdown).
pub(crate) fn handle_stream(
    manager: &Arc<ImposterManager>,
    query: Option<&str>,
    forced_port: Option<u16>,
    cancel: CancellationToken,
) -> Response<AdminBody> {
    let params = match parse_params(query, forced_port) {
        Ok(p) => p,
        Err((code, msg)) => return error_response(code, &msg),
    };
    if let Some(port) = params.port
        && manager.get_imposter(port).is_err()
    {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("no imposter on port {port}"),
        );
    }

    let bus = Arc::clone(manager.event_bus());
    let mut rx = bus.subscribe();
    let start_seq = bus.seq();
    let (mut tx, body) = Channel::<Bytes, hyper::Error>::new(CHANNEL_BUFFER);

    tokio::spawn(async move {
        let hello = serde_json::json!({
            "engineVersion": env!("CARGO_PKG_VERSION"),
            "seq": start_seq,
            "types": type_list(&params),
            "port": params.port,
        });
        if !send_frame(&mut tx, &cancel, sse_frame("hello", None, &hello)).await {
            return;
        }
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.tick().await; // the first tick is immediate — consume it so ping doesn't fire now
        loop {
            // Pick the next frame to send. A `continue` skips a filtered-out event; a `break` ends
            // the stream (shutdown or the bus closing).
            let frame = tokio::select! {
                () = cancel.cancelled() => break,
                _ = heartbeat.tick() => Bytes::from_static(b": ping\n\n"),
                recv = rx.recv() => match recv {
                    Ok(event) => match render(&event, &params) {
                        Some(frame) => frame,
                        None => continue,
                    },
                    Err(RecvError::Lagged(n)) => {
                        sse_frame("lagged", None, &serde_json::json!({ "missed": n }))
                    }
                    Err(RecvError::Closed) => break,
                },
            };
            if !send_frame(&mut tx, &cancel, frame).await {
                break;
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        // Defeat proxy/response buffering so events reach the client immediately.
        .header("X-Accel-Buffering", "no")
        .body(body.boxed())
        .expect("SSE response builds from a static status + headers")
}

/// Send one SSE frame, abandoning it if the server is shutting down. `send_data` blocks when a
/// stalled client fills the channel buffer, so racing it against `cancel` keeps a slow or silent
/// reader from pinning the forwarder task (and its connection) past graceful shutdown. Returns
/// `false` (stop the stream) on cancellation or a client disconnect.
async fn send_frame(
    tx: &mut Sender<Bytes, hyper::Error>,
    cancel: &CancellationToken,
    frame: Bytes,
) -> bool {
    tokio::select! {
        () = cancel.cancelled() => false,
        result = tx.send_data(frame) => result.is_ok(),
    }
}

/// Parse `types` (comma list of `requests`/`lifecycle`, default both), `port`, and `match=` clauses.
/// For the alias (`forced_port` set), the stream is request-only and `types` is ignored.
fn parse_params(
    query: Option<&str>,
    forced_port: Option<u16>,
) -> Result<StreamParams, (StatusCode, String)> {
    let clauses =
        parse_match_clauses(query).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let mut requests = true;
    let mut lifecycle = true;
    let mut port = forced_port;
    let alias = forced_port.is_some();

    for pair in query.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "types" if !alias => {
                requests = false;
                lifecycle = false;
                for t in value.split(',').filter(|s| !s.is_empty()) {
                    match t {
                        "requests" => requests = true,
                        "lifecycle" => lifecycle = true,
                        other => {
                            return Err((
                                StatusCode::BAD_REQUEST,
                                format!(
                                    "unknown types value '{other}' (expected requests|lifecycle)"
                                ),
                            ));
                        }
                    }
                }
            }
            "port" if !alias => {
                port =
                    Some(value.parse().map_err(|_| {
                        (StatusCode::BAD_REQUEST, format!("invalid port: {value}"))
                    })?);
            }
            _ => {}
        }
    }
    if alias {
        requests = true;
        lifecycle = false;
    }
    Ok(StreamParams {
        requests,
        lifecycle,
        port,
        clauses,
    })
}

fn type_list(params: &StreamParams) -> Vec<&'static str> {
    let mut out = Vec::new();
    if params.requests {
        out.push("requests");
    }
    if params.lifecycle {
        out.push("lifecycle");
    }
    out
}

/// Render an event to an SSE frame, applying the type/port/match filters. `None` = filtered out.
fn render(event: &AdminEvent, params: &StreamParams) -> Option<Bytes> {
    match &event.kind {
        AdminEventKind::Lifecycle { action, port } => {
            if !params.lifecycle {
                return None;
            }
            // A concrete-port lifecycle event is dropped when it isn't the filtered port; a global
            // event (AllDeleted, port None) always passes.
            if let (Some(filter), Some(p)) = (params.port, port)
                && *p != filter
            {
                return None;
            }
            let mut data = serde_json::json!({ "action": action.as_str() });
            if let Some(p) = port {
                data["port"] = serde_json::json!(p);
            }
            Some(sse_frame("imposter", Some(event.seq), &data))
        }
        AdminEventKind::Request {
            port,
            flow_id,
            index,
            request,
        } => {
            if !params.requests {
                return None;
            }
            if let Some(filter) = params.port
                && *port != filter
            {
                return None;
            }
            if !event_matches(&params.clauses, flow_id, request) {
                return None;
            }
            let mut data =
                serde_json::json!({ "port": port, "flowId": flow_id, "request": request });
            // Omitted entirely when the journal backend has no stable indices (issue #603), so
            // its absence is the same capability probe the polling side uses.
            if let Some(index) = index {
                data["index"] = serde_json::json!(index);
            }
            Some(sse_frame("request", Some(event.seq), &data))
        }
    }
}

/// Apply `match=` clauses to a request event. `flow_id=` compares the event's flow id — resolved at
/// record time from the imposter's `flow_id_source` (a `header:` source falls back to the port when
/// the header is absent, matching `rift-verify`'s record-time semantics) — and `header:` checks the
/// recorded request's (multi-value) headers. This mirrors `GET /savedRequests?match=` except for the
/// header-absent edge, where `savedRequests` resolves the flow to "no match" at query time.
fn event_matches(
    clauses: &[MatchClause],
    flow_id: &str,
    request: &rift_mock_core::imposter::RecordedRequest,
) -> bool {
    clauses.iter().all(|clause| match clause {
        MatchClause::FlowId(value) => flow_id == value,
        MatchClause::Header { name, value } => request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .is_some_and(|(_, values)| values.iter().any(|v| v == value)),
        MatchClause::Method(value) => request.method == *value,
        MatchClause::Path(value) => request.path == *value,
    })
}

fn sse_frame(event: &str, id: Option<u64>, data: &serde_json::Value) -> Bytes {
    let mut frame = format!("event: {event}\n");
    if let Some(id) = id {
        frame.push_str(&format!("id: {id}\n"));
    }
    frame.push_str(&format!("data: {data}\n\n"));
    Bytes::from(frame)
}

fn error_response(code: StatusCode, message: &str) -> Response<AdminBody> {
    let body = serde_json::json!({ "error": message }).to_string();
    Response::builder()
        .status(code)
        .header("Content-Type", "application/json")
        .body(
            Full::new(Bytes::from(body))
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("error response builds from a static status + headers")
}
