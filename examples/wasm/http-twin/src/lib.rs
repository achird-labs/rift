//! A `wasi:http/proxy` component (issue #1758) that also exports `rift:mock/handler`, so the same
//! component answers under `_rift.script.export: "http"` and `"rift"`.
//!
//! Under `export: "http"` the path picks the behaviour:
//!
//! | path | answer |
//! |---|---|
//! | `/echo…` (and any other path) | `200`, `x-echo-method`, two `set-cookie`s, `x-knobs` (the `RIFT_KNOBS` variable, `absent` when unset), body `"<METHOD> <path?query>\n"` then the request body's bytes |
//! | `/count` | increments `hits` in the imposter's flow store (`rift:mock/state`): `{"count":N}` |
//! | `/memory` | increments a counter in guest memory: `{"count":N}` |
//! | `/stream` | sets the response, then writes `one-`, `two-`, `three` as three writes |
//! | `/big?bytes=N` | `N` bytes of `x` |
//! | `/bytes` | the three bytes `ff 00 fe`, which are not UTF-8 |
//! | `/length-ok` | `content-length: 3` and `abc` |
//! | `/length-cut` | `content-length: 10`, writes `abc`, and returns still holding the body |
//! | `/spin` | sets the response, writes `partial`, then spins until the deadline |
//! | `/none` | drops the response outparam without setting it |
//! | `/error` | sets the outparam to an `internal-error` |
//! | `/trap` | traps |
//!
//! Under `export: "rift"`, `respond` answers `/echo…` as the http export does (status,
//! `x-echo-method`, body), with `x-knobs` from `init`. Built with `--no-default-features`, the
//! component exports `wasi:http/incoming-handler` only.
//!
//! Build with `cargo build --release --target wasm32-wasip2`.

use std::io::{Read as _, Write as _};
#[cfg(feature = "rift-export")]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

mod mock {
    wit_bindgen::generate!({
        path: "../../../crates/rift-mock-core/wit",
        world: "mock",
        pub_export_macro: true,
    });
}

use wasip2::http::types::{
    ErrorCode, Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};

/// Guest memory: lives as long as the instance does.
static MEMORY_COUNT: AtomicU64 = AtomicU64::new(0);
/// `knobs`, as `init` saw them (the rift export).
#[cfg(feature = "rift-export")]
static KNOBS: OnceLock<String> = OnceLock::new();

struct HttpTwin;

fn echo_body(method: &str, path_with_query: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!("{method} {path_with_query}\n").into_bytes();
    out.extend_from_slice(body);
    out
}

fn method_name(method: &Method) -> String {
    match method {
        Method::Get => "GET".into(),
        Method::Head => "HEAD".into(),
        Method::Post => "POST".into(),
        Method::Put => "PUT".into(),
        Method::Delete => "DELETE".into(),
        Method::Connect => "CONNECT".into(),
        Method::Options => "OPTIONS".into(),
        Method::Trace => "TRACE".into(),
        Method::Patch => "PATCH".into(),
        Method::Other(other) => other.clone(),
    }
}

fn read_body(request: &IncomingRequest) -> Vec<u8> {
    let Ok(body) = request.consume() else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if let Ok(mut stream) = body.stream() {
        // A closed stream ends the read; an error ends it too, keeping what was read.
        let _ = stream.read_to_end(&mut bytes);
    }
    bytes
}

/// Set `out` to a response with `headers`, then write `chunks` as separate writes.
fn answer(out: ResponseOutparam, status: u16, headers: &[(&str, &[u8])], chunks: &[&[u8]]) {
    let fields = Fields::new();
    for (name, value) in headers {
        fields
            .append(name, value)
            .expect("a header the twin sets is allowed");
    }
    let response = OutgoingResponse::new(fields);
    response.set_status_code(status).expect("a valid status");
    let body = response.body().expect("the body, once");
    ResponseOutparam::set(out, Ok(response));
    let mut stream = body.write().expect("the body stream, once");
    for chunk in chunks {
        stream.write_all(chunk).expect("write");
        stream.flush().expect("flush");
    }
    drop(stream);
    OutgoingBody::finish(body, None).expect("finish");
}

fn query_number(path_with_query: &str, key: &str) -> Option<usize> {
    let (_, query) = path_with_query.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key { v.parse().ok() } else { None }
    })
}

impl wasip2::exports::http::incoming_handler::Guest for HttpTwin {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let method = method_name(&request.method());
        let path = request.path_with_query().unwrap_or_default();
        let route = path.split('?').next().unwrap_or_default().to_owned();
        let body = read_body(&request);
        match route.as_str() {
            "/count" => {
                let count = mock::rift::mock::state::incr_by("hits", 1, None).expect("state");
                answer(out, 200, &[], &[format!(r#"{{"count":{count}}}"#).as_bytes()]);
            }
            "/memory" => {
                let count = MEMORY_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                answer(out, 200, &[], &[format!(r#"{{"count":{count}}}"#).as_bytes()]);
            }
            "/stream" => answer(out, 200, &[], &[b"one-", b"two-", b"three"]),
            "/big" => {
                let n = query_number(&path, "bytes").unwrap_or(0);
                let chunk = vec![b'x'; 64 * 1024];
                let mut chunks: Vec<&[u8]> = Vec::new();
                let mut left = n;
                while left > 0 {
                    let take = left.min(chunk.len());
                    chunks.push(&chunk[..take]);
                    left -= take;
                }
                answer(out, 200, &[], &chunks);
            }
            "/bytes" => answer(out, 200, &[], &[b"\xff\x00\xfe"]),
            "/length-ok" => answer(out, 200, &[("content-length", b"3")], &[b"abc"]),
            "/length-cut" => {
                let fields = Fields::new();
                fields.append("content-length", b"10").expect("allowed");
                let response = OutgoingResponse::new(fields);
                let body = response.body().expect("the body, once");
                ResponseOutparam::set(out, Ok(response));
                let mut stream = body.write().expect("the body stream, once");
                stream.write_all(b"abc").expect("write");
                stream.flush().expect("flush");
                // Neither finished nor dropped: the host is left holding an open body.
                std::mem::forget(stream);
                std::mem::forget(body);
            }
            "/spin" => {
                let response = OutgoingResponse::new(Fields::new());
                let body = response.body().expect("the body, once");
                ResponseOutparam::set(out, Ok(response));
                let mut stream = body.write().expect("the body stream, once");
                stream.write_all(b"partial").expect("write");
                stream.flush().expect("flush");
                loop {
                    std::hint::spin_loop();
                }
            }
            "/none" => drop(out),
            "/error" => ResponseOutparam::set(
                out,
                Err(ErrorCode::InternalError(Some("twin failed".into()))),
            ),
            "/trap" => unreachable!("the twin traps on /trap"),
            _ => {
                let knobs = std::env::var("RIFT_KNOBS").unwrap_or_else(|_| "absent".into());
                answer(
                    out,
                    200,
                    &[
                        ("x-echo-method", method.as_bytes()),
                        ("set-cookie", b"a=1"),
                        ("set-cookie", b"b=2"),
                        ("x-knobs", knobs.as_bytes()),
                    ],
                    &[&echo_body(&method, &path, &body)],
                );
            }
        }
    }
}

wasip2::http::proxy::export!(HttpTwin);

#[cfg(feature = "rift-export")]
use mock::exports::rift::mock::handler::Guest as RiftGuest;
#[cfg(feature = "rift-export")]
use mock::rift::mock::types::{Decision, Header, Request, Response};

#[cfg(feature = "rift-export")]
impl RiftGuest for HttpTwin {
    fn init(knobs: String) -> Result<(), String> {
        KNOBS
            .set(knobs)
            .map_err(|_| "init ran twice on one instance".to_string())
    }

    fn respond(req: Request) -> Result<Decision, String> {
        let mut path = req.path.clone();
        if !req.query.is_empty() {
            let query: Vec<String> = req.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
            path = format!("{path}?{}", query.join("&"));
        }
        Ok(Decision::Respond(Response {
            status: 200,
            headers: vec![
                Header {
                    name: "x-echo-method".into(),
                    value: req.method.clone(),
                },
                Header {
                    name: "x-knobs".into(),
                    value: KNOBS.get().cloned().unwrap_or_default(),
                },
            ],
            body: echo_body(&req.method, &path, &req.body),
        }))
    }
}

#[cfg(feature = "rift-export")]
mock::export!(HttpTwin with_types_in mock);
