//! Per-imposter clock admin handlers (issue #1754).
//!
//! `PUT /imposters/:port/clock` freezes the clock at an instant (`{"nowMs": n}`) or steps it
//! forward (`{"advanceMs": n}`); `GET` reads it; `DELETE` returns it to the wall clock.

use crate::admin_api::types::{collect_body, json_response};
use crate::imposter::ImposterManager;
use crate::imposter::clock::{ClockMode, ImposterClock};
use crate::response::{ErrorKind, error_response_typed};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use std::sync::Arc;

/// What a `PUT` asks of the clock.
enum ClockRequest {
    Freeze(i64),
    Advance(i64),
}

fn bad_data(message: &str) -> Response<Full<Bytes>> {
    error_response_typed(StatusCode::BAD_REQUEST, ErrorKind::BadData, message)
}

/// Exactly one of `nowMs` / `advanceMs`, each an integer.
fn parse_request(body: &[u8]) -> Result<ClockRequest, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("Invalid JSON: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "the body must be a JSON object".to_string())?;
    if let Some(unknown) = object
        .keys()
        .find(|k| !matches!(k.as_str(), "nowMs" | "advanceMs"))
    {
        return Err(format!("unknown key '{unknown}'"));
    }
    let integer = |key: &str| -> Result<Option<i64>, String> {
        object.get(key).map_or(Ok(None), |v| {
            v.as_i64()
                .map(Some)
                .ok_or_else(|| format!("{key} must be an integer number of milliseconds"))
        })
    };
    match (integer("nowMs")?, integer("advanceMs")?) {
        (Some(now), None) => Ok(ClockRequest::Freeze(now)),
        (None, Some(by)) => Ok(ClockRequest::Advance(by)),
        (Some(_), Some(_)) => Err("give nowMs or advanceMs, not both".to_string()),
        (None, None) => Err("one of nowMs or advanceMs is required".to_string()),
    }
}

fn clock_json(clock: &ImposterClock) -> Response<Full<Bytes>> {
    // Read the mode first: `now_ms` of a wall clock moves, and the pair must describe one state.
    let mode = clock.mode();
    let (name, now_ms) = match mode {
        ClockMode::Wall => ("wall", clock.now_ms()),
        ClockMode::Frozen { now_ms } => ("frozen", now_ms),
    };
    json_response(
        StatusCode::OK,
        &serde_json::json!({ "mode": name, "nowMs": now_ms }),
    )
}

/// GET /imposters/:port/clock
pub async fn handle_get(port: u16, manager: Arc<ImposterManager>) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => clock_json(imposter.clock()),
        Err(e) => e.into(),
    }
}

/// PUT /imposters/:port/clock
pub async fn handle_put(
    port: u16,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req, &manager.body_guard()).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let imposter = match manager.get_imposter(port) {
        Ok(i) => i,
        Err(e) => return e.into(),
    };
    let request = match parse_request(&body) {
        Ok(r) => r,
        Err(message) => return bad_data(&message),
    };
    let clock = imposter.clock();
    match request {
        ClockRequest::Freeze(now_ms) => {
            if let Err(e) = clock.freeze(now_ms) {
                return bad_data(&e.to_string());
            }
        }
        ClockRequest::Advance(by) => {
            if let Err(e) = clock.advance(by) {
                return bad_data(&e.to_string());
            }
        }
    }
    clock_json(clock)
}

/// DELETE /imposters/:port/clock
pub async fn handle_delete(port: u16, manager: Arc<ImposterManager>) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            imposter.clock().reset();
            clock_json(imposter.clock())
        }
        Err(e) => e.into(),
    }
}
