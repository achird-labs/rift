//! Issue #1439: body limits are Mountebank's (no refusal below its ~512 MiB breaking point), and a
//! shared in-flight budget turns overload into `503` + `Retry-After` instead of exhausting memory.
//!
//! The ceiling and budget tests use small configured limits, so the same logic runs without
//! allocating hundreds of MiB; the parity tests use the defaults with bodies past the old caps.
//! Refusals are driven over a raw socket with `Expect: 100-continue`: a refused body is never sent,
//! so the status always arrives instead of a reset.

use rift_http_proxy::imposter::ImposterManager;
use rift_http_proxy::imposter::body_limits::BodyLimits;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MIB: usize = 1024 * 1024;

fn limits(max: usize, budget: Option<usize>) -> BodyLimits {
    BodyLimits::new(max, budget).expect("valid limits")
}

async fn create(manager: &ImposterManager, imposter: Value) -> u16 {
    let config = serde_json::from_value(imposter).expect("imposter config");
    manager.create_imposter(config).await.expect("create")
}

/// An imposter answering 200 after holding the request `wait_ms`, recording requests.
async fn slow_imposter(manager: &ImposterManager, wait_ms: u64) -> u16 {
    create(
        manager,
        json!({"port": 0, "protocol": "http", "recordRequests": true, "stubs": [{"responses": [{
            "is": {"statusCode": 200, "body": "ok"},
            "_behaviors": {"wait": wait_ms}
        }]}]}),
    )
    .await
}

/// The status line and headers of the next response on `stream`, lower-cased.
async fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.expect("read") == 0 {
            break;
        }
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head).to_lowercase()
}

fn status_of(head: &str) -> u16 {
    head.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// POST `len` bytes with `Expect: 100-continue`, sending the body only if the server asks for it.
/// Returns the final response head.
async fn post_expecting_continue(port: u16, len: usize) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let head = format!(
        "POST /upload HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {len}\r\n\
         Expect: 100-continue\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("head");
    let first = read_head(&mut stream).await;
    if status_of(&first) != 100 {
        return first;
    }
    stream.write_all(&vec![b'x'; len]).await.expect("body");
    read_head(&mut stream).await
}

/// POST `chunks` as a chunked body (no length), then read the response head.
async fn post_chunked(port: u16, chunks: &[usize]) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    stream
        .write_all(
            b"POST /upload HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\
              Connection: close\r\n\r\n",
        )
        .await
        .expect("head");
    for &size in chunks {
        let mut chunk = format!("{size:x}\r\n").into_bytes();
        chunk.extend(std::iter::repeat_n(b'x', size));
        chunk.extend_from_slice(b"\r\n");
        if stream.write_all(&chunk).await.is_err() {
            break;
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n").await;
    read_head(&mut stream).await
}

// ---- parity: the old 10 MiB (imposter, proxy) and 64 MiB (admin) caps are gone ----

#[tokio::test]
async fn a_body_past_the_old_imposter_cap_is_served_and_recorded_in_full() {
    let manager = ImposterManager::new();
    let port = slow_imposter(&manager, 0).await;
    let body = vec![b'a'; 16 * MIB];
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/upload"))
        .body(body)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 200);
    let recorded = manager
        .get_imposter(port)
        .expect("imposter")
        .get_recorded_requests();
    assert_eq!(
        recorded[0].body.as_deref().map(str::len),
        Some(16 * MIB),
        "recorded at full length"
    );
    manager.delete_all().await;
}

#[tokio::test]
async fn an_admin_body_past_the_old_admin_cap_is_accepted() {
    let manager = Arc::new(ImposterManager::new());
    let admin = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::clone(&manager),
        None,
    )
    .bind()
    .await
    .expect("admin binds");
    // 65 MiB of JSON whitespace around a small imposter: past the old 64 MiB admin cap without
    // building a 65 MiB imposter.
    let mut body = br#"{"protocol": "http", "port": 0}"#.to_vec();
    body.extend(std::iter::repeat_n(b' ', 65 * MIB));
    let response = reqwest::Client::new()
        .post(format!("http://{}/imposters", admin.local_addr()))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 201);
    manager.delete_all().await;
}

#[tokio::test]
async fn a_proxied_response_past_the_old_proxy_cap_is_forwarded_intact() {
    let manager = ImposterManager::new();
    let upstream = create(
        &manager,
        json!({"port": 0, "protocol": "http", "stubs": [{"responses": [{
            "is": {"statusCode": 200, "body": "b".repeat(16 * MIB)}
        }]}]}),
    )
    .await;
    let proxy = create(
        &manager,
        json!({"port": 0, "protocol": "http", "stubs": [{"responses": [{"proxy": {
            "to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyTransparent"
        }}]}]}),
    )
    .await;
    let response = reqwest::get(format!("http://127.0.0.1:{proxy}/big"))
        .await
        .expect("send");
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.expect("body").len(), 16 * MIB);
    manager.delete_all().await;
}

// ---- the ceiling ----

/// A known length over the ceiling is refused before the body is read: no `100 Continue`.
#[tokio::test]
async fn a_known_length_over_the_ceiling_gets_413_without_100_continue() {
    let manager = ImposterManager::new().with_body_limits(limits(MIB, None));
    let port = slow_imposter(&manager, 0).await;
    let head = post_expecting_continue(port, 2 * MIB).await;
    assert_eq!(status_of(&head), 413, "{head}");
    assert!(head.contains("connection: close"), "{head}");
    manager.delete_all().await;
}

#[tokio::test]
async fn a_chunked_body_over_the_ceiling_gets_413() {
    let manager = ImposterManager::new().with_body_limits(limits(MIB, None));
    let port = slow_imposter(&manager, 0).await;
    let head = post_chunked(port, &[512 * 1024, 512 * 1024, 1]).await;
    assert_eq!(status_of(&head), 413, "{head}");
    manager.delete_all().await;
}

// ---- the budget ----

/// Eight 1 MiB uploads against a 2 MiB budget, each held 3 s (past the 2 s reservation wait): two
/// are admitted, the rest get 503 with `Retry-After`, and nothing crashes. A later request fits.
#[tokio::test]
async fn concurrent_uploads_past_the_budget_get_503_then_the_budget_recovers() {
    let manager = Arc::new(ImposterManager::new().with_body_limits(limits(MIB, Some(2 * MIB))));
    let port = slow_imposter(&manager, 3000).await;
    let heads: Vec<String> =
        futures_join_all((0..8).map(|_| tokio::spawn(post_expecting_continue(port, MIB)))).await;
    let statuses: Vec<u16> = heads.iter().map(|h| status_of(h)).collect();
    assert!(
        statuses.iter().all(|s| *s == 200 || *s == 503),
        "{statuses:?}"
    );
    assert!(
        statuses.iter().filter(|s| **s == 503).count() >= 4,
        "{statuses:?}"
    );
    for head in heads.iter().filter(|h| status_of(h) == 503) {
        assert!(head.contains("retry-after: 1"), "{head}");
    }
    let quick = create(
        &manager,
        json!({"port": 0, "protocol": "http", "stubs": [{"responses": [{"is": {"statusCode": 200}}]}]}),
    )
    .await;
    assert_eq!(status_of(&post_expecting_continue(quick, MIB).await), 200);
    manager.delete_all().await;
}

/// A chunked body never waits for the budget: while another body holds it all, a chunked upload
/// is refused at once rather than queued.
#[tokio::test]
async fn a_chunked_body_is_refused_at_once_when_the_budget_is_held() {
    let manager = Arc::new(ImposterManager::new().with_body_limits(limits(MIB, Some(MIB))));
    let port = slow_imposter(&manager, 3000).await;
    let holder = tokio::spawn(post_expecting_continue(port, MIB));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    let head = post_chunked(port, &[64 * 1024]).await;
    assert_eq!(status_of(&head), 503, "{head}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "refused promptly, took {:?}",
        started.elapsed()
    );
    assert_eq!(status_of(&holder.await.expect("holder")), 200);
    manager.delete_all().await;
}

// ---- configuration ----

#[test]
fn a_budget_below_the_ceiling_is_refused_at_startup() {
    let err =
        rift_http_proxy::server::body_limits_from(2 * MIB, MIB).expect_err("budget below ceiling");
    assert!(
        format!("{err:#}").contains("smaller than the maximum body size"),
        "{err:#}"
    );
    assert!(
        rift_http_proxy::server::body_limits_from(2 * MIB, 0)
            .expect("0 is unbounded")
            .budget_bytes()
            .is_none()
    );
}

async fn futures_join_all(
    handles: impl Iterator<Item = tokio::task::JoinHandle<String>>,
) -> Vec<String> {
    let mut out = Vec::new();
    for handle in handles.collect::<Vec<_>>() {
        out.push(handle.await.expect("task"));
    }
    out
}
