//! Issue #461: the admin SSE stream (`GET /events`, `GET /imposters/{port}/savedRequests/stream`)
//! must push recorded-request and imposter-lifecycle events to subscribers, filtered by
//! `types`/`port`/`match`, gated by the admin API key, lossy-but-loud under backpressure, and
//! closed cleanly on shutdown. These black-box tests drive a real in-process admin server over a
//! raw TCP socket (reqwest here has no `stream` feature) so the SSE framing is exercised end to end.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A parsed SSE frame: `event:`/`data:`/`id:` lines, or a `:` comment (heartbeat).
#[derive(Debug, Default, Clone)]
struct Frame {
    event: Option<String>,
    data: String,
    id: Option<String>,
    comment: Option<String>,
}

/// A live SSE connection over a raw socket, reading frames incrementally.
struct Sse {
    stream: TcpStream,
    buf: String,
}

impl Sse {
    /// Open `GET <path>` against the admin server, optionally with an `Authorization` header, and
    /// consume the response head. Returns the connection plus the HTTP status line.
    async fn connect(addr: SocketAddr, path: &str, auth: Option<&str>) -> (Self, String) {
        let mut stream = TcpStream::connect(addr).await.expect("connect admin");
        let auth_line = auth
            .map(|k| format!("Authorization: {k}\r\n"))
            .unwrap_or_default();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth_line}Accept: text/event-stream\r\nConnection: keep-alive\r\n\r\n"
        );
        stream
            .write_all(req.as_bytes())
            .await
            .expect("write request");

        // Read until the end of the response head (\r\n\r\n).
        let mut raw = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
                .await
                .expect("head read timeout")
                .expect("head read");
            if n == 0 {
                break;
            }
            raw.push(byte[0]);
            if raw.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&raw).into_owned();
        let status = head.lines().next().unwrap_or_default().to_string();
        (
            Self {
                stream,
                buf: String::new(),
            },
            head_with_status(head, status.clone()),
        )
    }

    /// Read the next SSE frame (blocks separated by a blank line), or `None` on timeout/EOF.
    async fn next_frame(&mut self, timeout: Duration) -> Option<Frame> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(idx) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..idx + 2).collect();
                return Some(parse_frame(&block));
            }
            let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
            let mut chunk = [0u8; 4096];
            let n = tokio::time::timeout(remaining, self.stream.read(&mut chunk))
                .await
                .ok()?
                .ok()?;
            if n == 0 {
                return None; // EOF
            }
            self.buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    }

    /// Read frames until one with `event == name` arrives (skipping heartbeats/others), or timeout.
    async fn wait_for(&mut self, name: &str, timeout: Duration) -> Option<Frame> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
            let f = self.next_frame(remaining).await?;
            if f.event.as_deref() == Some(name) {
                return Some(f);
            }
        }
    }
}

fn head_with_status(_head: String, status: String) -> String {
    status
}

fn parse_frame(block: &str) -> Frame {
    let mut f = Frame::default();
    let mut data_lines = Vec::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix(':') {
            f.comment = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("event:") {
            f.event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("id:") {
            f.id = Some(rest.trim().to_string());
        }
    }
    f.data = data_lines.join("\n");
    f
}

fn json(f: &Frame) -> serde_json::Value {
    serde_json::from_str(&f.data).unwrap_or(serde_json::Value::Null)
}

async fn start_server(
    api_key: Option<&str>,
) -> (
    SocketAddr,
    Arc<ImposterManager>,
    rift_http_proxy::admin_api::RunningAdminApi,
) {
    let manager = Arc::new(ImposterManager::new());
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = AdminApiServer::new(addr, manager.clone(), api_key.map(str::to_string));
    let running = server.bind().await.expect("bind admin");
    (running.local_addr(), manager, running)
}

async fn create(manager: &ImposterManager, config: serde_json::Value) -> u16 {
    let cfg = serde_json::from_value(config).expect("valid config");
    manager.create_imposter(cfg).await.expect("create imposter")
}

#[tokio::test]
async fn hello_event_on_connect() {
    let (addr, _mgr, running) = start_server(None).await;
    let (mut sse, status) = Sse::connect(addr, "/events", None).await;
    assert!(status.contains("200"), "expected 200, got {status:?}");
    let hello = sse
        .wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello frame");
    let v = json(&hello);
    assert!(
        v["engineVersion"].is_string(),
        "hello carries engineVersion: {v}"
    );
    assert!(v["seq"].is_number(), "hello carries seq: {v}");
    running.shutdown().await;
}

#[tokio::test]
async fn lifecycle_events_created_and_deleted() {
    let (addr, mgr, running) = start_server(None).await;
    let (mut sse, _s) = Sse::connect(addr, "/events?types=lifecycle", None).await;
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");

    let port = create(
        &mgr,
        serde_json::json!({"port":18801,"protocol":"http","stubs":[]}),
    )
    .await;
    let created = sse
        .wait_for("imposter", Duration::from_secs(5))
        .await
        .expect("created event");
    assert_eq!(json(&created)["action"], "created");
    assert_eq!(json(&created)["port"], port);

    mgr.delete_imposter(port).await.expect("delete");
    let deleted = sse
        .wait_for("imposter", Duration::from_secs(5))
        .await
        .expect("deleted event");
    assert_eq!(json(&deleted)["action"], "deleted");
    assert_eq!(json(&deleted)["port"], port);
    running.shutdown().await;
}

#[tokio::test]
async fn request_event_matches_saved_requests() {
    let (addr, mgr, running) = start_server(None).await;
    let iport = create(
        &mgr,
        serde_json::json!({"port":18802,"protocol":"http","recordRequests":true,
            "stubs":[{"responses":[{"is":{"statusCode":200,"body":"ok"}}]}]}),
    )
    .await;
    let (mut sse, _s) = Sse::connect(addr, "/events?types=requests", None).await;
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");

    reqwest::get(format!("http://127.0.0.1:{iport}/hello?q=1"))
        .await
        .expect("drive request");
    let ev = sse
        .wait_for("request", Duration::from_secs(5))
        .await
        .expect("request event");
    let v = json(&ev);
    assert_eq!(v["port"], iport);
    assert_eq!(v["request"]["path"], "/hello");
    assert_eq!(v["request"]["method"], "GET");

    // The embedded request JSON must equal the savedRequests projection.
    let saved: serde_json::Value =
        reqwest::get(format!("http://{addr}/imposters/{iport}/savedRequests"))
            .await
            .expect("saved")
            .json()
            .await
            .expect("saved json");
    assert_eq!(
        v["request"], saved[0],
        "SSE request JSON must equal the savedRequests entry"
    );
    running.shutdown().await;
}

// Issue #603 + #461: the `index` on a request event is the same journal index the polling side
// hands out, so a client that lags or reconnects can reconcile with `?since=<index>` instead of
// re-polling the whole journal. This is the join between the push and pull halves of the tail.
#[tokio::test]
async fn request_event_index_matches_savedrequests_cursor() {
    let (addr, mgr, running) = start_server(None).await;
    let iport = create(
        &mgr,
        serde_json::json!({"port":18814,"protocol":"http","recordRequests":true,
            "stubs":[{"responses":[{"is":{"statusCode":200,"body":"ok"}}]}]}),
    )
    .await;
    let (mut sse, _s) = Sse::connect(addr, "/events?types=requests", None).await;
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");

    let mut indices = Vec::new();
    for n in 1..=3 {
        reqwest::get(format!("http://127.0.0.1:{iport}/r{n}"))
            .await
            .expect("drive request");
        let ev = sse
            .wait_for("request", Duration::from_secs(5))
            .await
            .expect("request event");
        indices.push(json(&ev)["index"].as_u64().expect("index on the event"));
    }
    assert_eq!(indices, vec![1, 2, 3], "1-based, sequential, per port");

    // The cursor the polling side reports must agree with the last index the stream delivered.
    let resp = reqwest::get(format!("http://{addr}/imposters/{iport}/savedRequests"))
        .await
        .expect("saved");
    let next = resp
        .headers()
        .get("x-rift-next-index")
        .expect("cursor header")
        .to_str()
        .expect("header utf8")
        .to_string();
    assert_eq!(
        next, "3",
        "the poll cursor agrees with the streamed indices"
    );

    // Reconciling from the second event's index returns exactly what came after it — the
    // canonical lag/reconnect recovery, without a full re-poll.
    let after: serde_json::Value = reqwest::get(format!(
        "http://{addr}/imposters/{iport}/savedRequests?since={}",
        indices[1]
    ))
    .await
    .expect("since")
    .json()
    .await
    .expect("since json");
    assert_eq!(after.as_array().expect("array").len(), 1);
    assert_eq!(after[0]["path"], "/r3");

    running.shutdown().await;
}

#[tokio::test]
async fn types_lifecycle_suppresses_request_events() {
    let (addr, mgr, running) = start_server(None).await;
    let iport = create(
        &mgr,
        serde_json::json!({"port":18803,"protocol":"http","recordRequests":true,
            "stubs":[{"responses":[{"is":{"statusCode":200}}]}]}),
    )
    .await;
    let (mut sse, _s) = Sse::connect(addr, "/events?types=lifecycle", None).await;
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");

    reqwest::get(format!("http://127.0.0.1:{iport}/x"))
        .await
        .expect("drive request");
    // No request event should arrive on a lifecycle-only stream; a short read must not yield one.
    let f = sse.wait_for("request", Duration::from_secs(1)).await;
    assert!(f.is_none(), "types=lifecycle must suppress request events");
    running.shutdown().await;
}

#[tokio::test]
async fn port_filter_scopes_request_events() {
    let (addr, mgr, running) = start_server(None).await;
    let a = create(&mgr, serde_json::json!({"port":18804,"protocol":"http","recordRequests":true,"stubs":[{"responses":[{"is":{"statusCode":200}}]}]})).await;
    let b = create(&mgr, serde_json::json!({"port":18805,"protocol":"http","recordRequests":true,"stubs":[{"responses":[{"is":{"statusCode":200}}]}]})).await;
    let (mut sse, _s) = Sse::connect(addr, &format!("/events?types=requests&port={a}"), None).await;
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");

    // A request to the OTHER imposter must not appear; a request to the scoped one must.
    reqwest::get(format!("http://127.0.0.1:{b}/other"))
        .await
        .expect("req b");
    reqwest::get(format!("http://127.0.0.1:{a}/mine"))
        .await
        .expect("req a");
    let ev = sse
        .wait_for("request", Duration::from_secs(5))
        .await
        .expect("request event");
    assert_eq!(json(&ev)["port"], a);
    assert_eq!(
        json(&ev)["request"]["path"],
        "/mine",
        "must skip the other imposter's request"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn match_flow_id_filter_scopes_request_events() {
    let (addr, mgr, running) = start_server(None).await;
    // Flow id derives from a header, so two requests with different X-Space values land in
    // different flows — the same scoping `GET /savedRequests?match=flow_id=` applies.
    let iport = create(
        &mgr,
        serde_json::json!({"port":18806,"protocol":"http","recordRequests":true,
            "_rift":{"flowState":{"flowIdSource":"header:X-Space"}},
            "stubs":[{"responses":[{"is":{"statusCode":200}}]}]}),
    )
    .await;
    let (mut sse, _s) =
        Sse::connect(addr, "/events?types=requests&match=flow_id%3Dblue", None).await;
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");

    let client = reqwest::Client::new();
    client
        .get(format!("http://127.0.0.1:{iport}/green"))
        .header("X-Space", "green")
        .send()
        .await
        .expect("green");
    client
        .get(format!("http://127.0.0.1:{iport}/blue"))
        .header("X-Space", "blue")
        .send()
        .await
        .expect("blue");
    let ev = sse
        .wait_for("request", Duration::from_secs(5))
        .await
        .expect("request event");
    assert_eq!(json(&ev)["flowId"], "blue");
    assert_eq!(
        json(&ev)["request"]["path"],
        "/blue",
        "must skip the green-flow request"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn match_method_and_path_filter_scopes_request_events() {
    // #700: `method=`/`path=` filter the SSE request stream, AND-ed like every other clause.
    let (addr, mgr, running) = start_server(None).await;
    let iport = create(
        &mgr,
        serde_json::json!({"port":18815,"protocol":"http","recordRequests":true,
            "stubs":[{"responses":[{"is":{"statusCode":200}}]}]}),
    )
    .await;
    let (mut sse, _s) = Sse::connect(
        addr,
        "/events?types=requests&match=method%3DPOST&match=path%3D%2Forders",
        None,
    )
    .await;
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");

    let client = reqwest::Client::new();
    // Rejected: right path, wrong method.
    client
        .get(format!("http://127.0.0.1:{iport}/orders"))
        .send()
        .await
        .expect("get /orders");
    // Accepted: POST /orders.
    client
        .post(format!("http://127.0.0.1:{iport}/orders"))
        .send()
        .await
        .expect("post /orders");
    let ev = sse
        .wait_for("request", Duration::from_secs(5))
        .await
        .expect("request event");
    assert_eq!(json(&ev)["request"]["method"], "POST");
    assert_eq!(
        json(&ev)["request"]["path"],
        "/orders",
        "must skip the GET /orders request"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn unauthorized_without_api_key() {
    let (addr, _mgr, running) = start_server(Some("s3cret")).await;
    let (_sse, status) = Sse::connect(addr, "/events", None).await;
    assert!(
        status.contains("401"),
        "expected 401 without key, got {status:?}"
    );
    // With the key it connects.
    let (mut ok, status_ok) = Sse::connect(addr, "/events", Some("s3cret")).await;
    assert!(
        status_ok.contains("200"),
        "expected 200 with key, got {status_ok:?}"
    );
    ok.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello with key");
    running.shutdown().await;
}

#[tokio::test]
async fn lagged_event_on_backpressure() {
    let (addr, mgr, running) = start_server(None).await;
    let (mut sse, _s) = Sse::connect(addr, "/events?types=lifecycle", None).await;
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");

    // Stall the reader and flood the bus past its capacity so the slow subscriber lags.
    tokio::time::sleep(Duration::from_millis(100)).await;
    for _ in 0..2000u32 {
        mgr.event_bus().publish_lifecycle(
            rift_mock_core::imposter::ImposterAction::StubsChanged,
            Some(19999),
        );
    }
    // Now drain: a `lagged` frame must appear, and the stream keeps going (not closed).
    let lagged = sse
        .wait_for("lagged", Duration::from_secs(5))
        .await
        .expect("lagged frame");
    assert!(
        json(&lagged)["missed"].as_u64().unwrap_or(0) >= 1,
        "lagged reports missed>=1"
    );
    // The stream must CONTINUE after lagging (not silently die): a fresh event still arrives.
    mgr.event_bus().publish_lifecycle(
        rift_mock_core::imposter::ImposterAction::Created,
        Some(17777),
    );
    let after = sse
        .wait_for("imposter", Duration::from_secs(5))
        .await
        .expect("an event must still arrive after a lagged frame");
    assert!(
        json(&after)["action"].is_string(),
        "stream stays alive after lagged: {}",
        json(&after)
    );
    running.shutdown().await;
}

#[tokio::test]
async fn alias_endpoint_streams_scoped_requests() {
    let (addr, mgr, running) = start_server(None).await;
    let a = create(&mgr, serde_json::json!({"port":18810,"protocol":"http","recordRequests":true,"stubs":[{"responses":[{"is":{"statusCode":200}}]}]})).await;
    let b = create(&mgr, serde_json::json!({"port":18811,"protocol":"http","recordRequests":true,"stubs":[{"responses":[{"is":{"statusCode":200}}]}]})).await;
    // The alias auto-scopes to its port and streams request events without any query params.
    let (mut sse, status) =
        Sse::connect(addr, &format!("/imposters/{a}/savedRequests/stream"), None).await;
    assert!(status.contains("200"), "alias returns 200, got {status:?}");
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");

    reqwest::get(format!("http://127.0.0.1:{b}/other"))
        .await
        .expect("req b");
    reqwest::get(format!("http://127.0.0.1:{a}/mine"))
        .await
        .expect("req a");
    let ev = sse
        .wait_for("request", Duration::from_secs(5))
        .await
        .expect("request event");
    assert_eq!(json(&ev)["port"], a, "alias auto-scopes to its own port");
    assert_eq!(json(&ev)["request"]["path"], "/mine");
    running.shutdown().await;
}

#[tokio::test]
async fn no_types_param_streams_both_families_with_monotonic_ids() {
    let (addr, mgr, running) = start_server(None).await;
    let (mut sse, _s) = Sse::connect(addr, "/events", None).await;
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");

    // Default (no types=) streams BOTH families: a lifecycle event and a request event.
    let iport = create(&mgr, serde_json::json!({"port":18812,"protocol":"http","recordRequests":true,"stubs":[{"responses":[{"is":{"statusCode":200}}]}]})).await;
    let created = sse
        .wait_for("imposter", Duration::from_secs(5))
        .await
        .expect("lifecycle event");
    assert_eq!(json(&created)["action"], "created");
    reqwest::get(format!("http://127.0.0.1:{iport}/x"))
        .await
        .expect("drive");
    let req = sse
        .wait_for("request", Duration::from_secs(5))
        .await
        .expect("request event");
    // `id:` is present and strictly increasing across events (the reconnect contract).
    let id_created: u64 = created.id.as_deref().unwrap().parse().unwrap();
    let id_req: u64 = req.id.as_deref().unwrap().parse().unwrap();
    assert!(
        id_req > id_created,
        "ids strictly increase: {id_created} -> {id_req}"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn error_branches_return_400_and_404() {
    let (addr, _mgr, running) = start_server(None).await;
    // Unknown port → 404.
    let (_a, s404) = Sse::connect(addr, "/events?port=59999", None).await;
    assert!(s404.contains("404"), "unknown port → 404, got {s404:?}");
    // Bad types value → 400.
    let (_b, s400) = Sse::connect(addr, "/events?types=bogus", None).await;
    assert!(s400.contains("400"), "bad types → 400, got {s400:?}");
    // Alias with a nonexistent port → 404.
    let (_c, s404b) = Sse::connect(addr, "/imposters/59998/savedRequests/stream", None).await;
    assert!(
        s404b.contains("404"),
        "alias unknown port → 404, got {s404b:?}"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn no_request_event_when_record_requests_disabled() {
    let (addr, mgr, running) = start_server(None).await;
    // recordRequests omitted (defaults false): the request is neither journaled nor streamed.
    let iport = create(&mgr, serde_json::json!({"port":18813,"protocol":"http","stubs":[{"responses":[{"is":{"statusCode":200}}]}]})).await;
    let (mut sse, _s) = Sse::connect(addr, "/events?types=requests", None).await;
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");
    reqwest::get(format!("http://127.0.0.1:{iport}/x"))
        .await
        .expect("drive");
    let f = sse.wait_for("request", Duration::from_secs(1)).await;
    assert!(f.is_none(), "no request event when recordRequests is off");
    running.shutdown().await;
}

#[tokio::test]
async fn shutdown_closes_stream_cleanly() {
    let (addr, _mgr, running) = start_server(None).await;
    let (mut sse, _s) = Sse::connect(addr, "/events", None).await;
    sse.wait_for("hello", Duration::from_secs(5))
        .await
        .expect("hello");
    running.shutdown().await;
    // After shutdown the socket must reach EOF promptly (no hang): next_frame returns None.
    let closed = tokio::time::timeout(
        Duration::from_secs(5),
        sse.next_frame(Duration::from_secs(5)),
    )
    .await;
    assert!(
        matches!(closed, Ok(None)),
        "stream must close (EOF) on shutdown, got {closed:?}"
    );
}
