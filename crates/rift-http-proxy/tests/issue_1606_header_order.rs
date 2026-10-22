//! Issue #1606: header order is kept, as Mountebank keeps it.
//!
//! - A request's headers reach scripts and the journal in the order the client sent them
//!   (Mountebank's `headersMap.ofRaw`), a repeated name at its first position.
//! - A response sends its headers in the order the stub, a decorate or an inject wrote them
//!   (Mountebank's `writeHead` walks the object's keys in property order).
//! - A stub prints its headers back in document order, and a proxy records the origin's order.
//!
//! Both used to be arbitrary: the request side changed on every request (the map's hasher is seeded
//! per map), so every test that can loops. Requests go out over a raw socket, so the test controls
//! the order on the wire, and responses are read the same way, which survives #1597's Title-Case.
//!
//! Every imposter takes an engine-assigned port.

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Create from the document's text, as `POST /imposters` reads a body: a `serde_json::Value` would
/// already have sorted every object's keys.
async fn create(manager: &ImposterManager, document: &str) -> u16 {
    let config = serde_json::from_str(document).expect("imposter config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

/// Send `GET target` with `headers` in this order over a raw HTTP/1.1 socket; return the response
/// head (status line and header lines) and the body.
async fn raw_get(port: u16, target: &str, headers: &[(&str, &str)]) -> (String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let mut request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .expect("response arrives")
        .expect("read");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("a response head");
    (head.to_owned(), body.to_owned())
}

/// The header names of a response head, in wire order, lower-cased.
fn names(head: &str) -> Vec<String> {
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect()
}

/// The names of `head` among `wanted`, in wire order.
fn order_of(head: &str, wanted: &[&str]) -> Vec<String> {
    names(head)
        .into_iter()
        .filter(|n| wanted.contains(&n.as_str()))
        .collect()
}

const SENT: [(&str, &str); 5] = [
    ("Zulu", "1"),
    ("Alpha", "2"),
    ("Mike", "3"),
    ("Bravo", "4"),
    ("Yankee", "5"),
];

#[tokio::test]
async fn inject_sees_request_headers_in_the_order_sent() {
    let manager = ImposterManager::new();
    // The non-JSON prefix keeps the body a plain string, compared byte for byte.
    let port = create(
        &manager,
        r#"{"protocol": "http", "stubs": [{"responses": [{"inject":
            "function (config) { return { statusCode: 200, body: 'H ' + JSON.stringify(Object.keys(config.request.headers)) }; }"
        }]}]}"#,
    )
    .await;
    for i in 0..50 {
        let (_, body) = raw_get(port, "/inject", &SENT).await;
        assert_eq!(
            body, r#"H ["Host","Zulu","Alpha","Mike","Bravo","Yankee","Connection"]"#,
            "request {i}"
        );
    }
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn the_journal_records_request_headers_in_the_order_sent() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        r#"{"protocol": "http", "recordRequests": true, "stubs": []}"#,
    )
    .await;
    for _ in 0..20 {
        raw_get(port, "/j", &SENT).await;
    }
    // A repeated name keeps its first position and every value, as `headersMap.ofRaw` does.
    raw_get(port, "/r", &[("X-A", "1"), ("X-B", "2"), ("X-A", "3")]).await;

    let recorded = manager
        .get_imposter(port)
        .expect("imposter")
        .get_recorded_requests();
    assert_eq!(recorded.len(), 21);
    for (i, request) in recorded[..20].iter().enumerate() {
        let printed = serde_json::to_string(&request).expect("serializes");
        let headers = printed
            .split_once(r#""headers":"#)
            .expect("headers printed")
            .1;
        assert!(
            headers.starts_with(
                r#"{"Host":"localhost","Zulu":"1","Alpha":"2","Mike":"3","Bravo":"4","Yankee":"5","Connection":"close"}"#
            ),
            "request {i}: {headers}"
        );
    }
    let printed = serde_json::to_string(&recorded[20]).expect("serializes");
    assert!(
        printed.contains(
            r#""headers":{"Host":"localhost","X-A":["1","3"],"X-B":"2","Connection":"close"}"#
        ),
        "{printed}"
    );
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn responses_send_headers_in_the_order_written() {
    // A decorate's added header goes after the stub's, as a JS object appends a new key.
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        r#"{"protocol": "http", "stubs": [
            {"predicates": [{"equals": {"path": "/is"}}],
             "responses": [{"is": {"headers": {"Zeta": "1", "Alpha": "2", "Mid": "3", "Beta": "4"}, "body": "ok"}}]},
            {"predicates": [{"equals": {"path": "/decorate"}}],
             "responses": [{"is": {"headers": {"Zeta": "1", "Alpha": "2", "Mid": "3", "Beta": "4"}, "body": "ok"},
                            "behaviors": [{"decorate": "function (request, response) { response.headers['Mid2'] = '5'; }"}]}]},
            {"predicates": [{"equals": {"path": "/inject"}}],
             "responses": [{"inject":
                "function () { return { statusCode: 200, headers: { Zeta: '1', Alpha: '2', Mid: '3', Beta: '4' }, body: 'ok' }; }"}]}
        ]}"#,
    )
    .await;
    let wanted = ["zeta", "alpha", "mid", "beta", "mid2"];
    for i in 0..20 {
        let (head, _) = raw_get(port, "/is", &[]).await;
        assert_eq!(
            order_of(&head, &wanted),
            ["zeta", "alpha", "mid", "beta"],
            "is {i}: {head}"
        );
        let (head, _) = raw_get(port, "/decorate", &[]).await;
        assert_eq!(
            order_of(&head, &wanted),
            ["zeta", "alpha", "mid", "beta", "mid2"],
            "decorate {i}: {head}"
        );
        let (head, _) = raw_get(port, "/inject", &[]).await;
        assert_eq!(
            order_of(&head, &wanted),
            ["zeta", "alpha", "mid", "beta"],
            "inject {i}: {head}"
        );
    }
    let _ = manager.delete_imposter(port).await;
}

/// The keys of the first stub's first `is.headers`, as the stub prints.
fn printed_header_keys(manager: &ImposterManager, port: u16) -> String {
    let stubs = manager.get_imposter(port).expect("imposter").get_stubs();
    let printed = serde_json::to_string(&stubs[0]).expect("serializes");
    printed
        .split_once(r#""headers":"#)
        .expect("headers printed")
        .1
        .split_once('}')
        .expect("an object")
        .0
        .to_owned()
}

#[tokio::test]
async fn a_stub_prints_its_headers_in_document_order_and_round_trips() {
    let manager = ImposterManager::new();
    // Streamed from text, as `POST /imposters` reads a body. A case-variant name folds into the
    // first spelling at its position.
    let document = r#"{"protocol": "http", "stubs": [{"responses": [{"is": {"headers":
        {"Zeta": "1", "x-a": "1", "B": "2", "X-A": "3", "Alpha": "4"}}}]}]}"#;
    let config = serde_json::from_str(document).expect("imposter config");
    let port = manager.create_imposter(config).await.expect("create");
    let keys = printed_header_keys(&manager, port);
    assert_eq!(
        keys, r#"{"Zeta":"1","x-a":["1","3"],"B":"2","Alpha":"4""#,
        "printed in document order"
    );

    // Posted back, the order holds.
    let stubs = manager.get_imposter(port).expect("imposter").get_stubs();
    let _ = manager.delete_imposter(port).await;
    let text = format!(
        r#"{{"protocol": "http", "stubs": [{}]}}"#,
        serde_json::to_string(&stubs[0]).expect("serializes")
    );
    let port = manager
        .create_imposter(serde_json::from_str(&text).expect("imposter config"))
        .await
        .expect("create again");
    assert_eq!(printed_header_keys(&manager, port), keys, "round trip");
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn a_proxy_records_the_origins_header_order() {
    let manager = ImposterManager::new();
    let origin = create(
        &manager,
        r#"{"protocol": "http", "stubs": [{"responses": [{"is":
            {"headers": {"X-Z": "1", "X-A": "2", "X-M": "3"}, "body": "o"}}]}]}"#,
    )
    .await;
    let proxy = create(
        &manager,
        &format!(
            r#"{{"protocol": "http", "stubs": [{{"responses": [{{"proxy":
                {{"to": "http://127.0.0.1:{origin}", "mode": "proxyOnce",
                  "predicateGenerators": [{{"matches": {{"path": true}}}}]}}}}]}}]}}"#
        ),
    )
    .await;
    raw_get(proxy, "/p", &[]).await;

    let stubs = manager.get_imposter(proxy).expect("imposter").get_stubs();
    // Read the names off the printed text: a `serde_json::Value` would sort them.
    let printed: Vec<String> = stubs
        .iter()
        .map(|s| serde_json::to_string(s).expect("serializes"))
        .collect();
    let printed = printed
        .iter()
        .find(|s| s.contains("\"X-Z\""))
        .unwrap_or_else(|| panic!("a recorded stub in {printed:?}"));
    let positions: Vec<usize> = ["\"X-Z\"", "\"X-A\"", "\"X-M\""]
        .iter()
        .map(|name| printed.find(name).expect("recorded"))
        .collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "recorded in the origin's order: {printed}"
    );
    let _ = manager.delete_imposter(proxy).await;
    let _ = manager.delete_imposter(origin).await;
}
