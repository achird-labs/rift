//! Issue #1696: the replayable export (`?replayable=true`, and so `rift save` / `mb save`), the
//! `DELETE /imposters` answer and the space stub door kept a stub's header order only until they
//! passed through a `serde_json::Value`, whose objects are sorted (`preserve_order` is off
//! workspace-wide). Mountebank prints a stub's `is.headers` in the order the author wrote it, so a
//! save → load round trip keeps it.
//!
//! Every assertion reads the raw response text: parsing it into a `Value` would sort it again and
//! hide the bug (whitespace is stripped, as the admin API pretty-prints). Request bodies are sent as
//! text for the same reason.

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// The stub headers as written, and as every export must print them.
const WRITTEN: &str = r#""headers":{"Zeta":"1","Alpha":"2","Mid":"3"}"#;

struct Admin {
    running: RunningAdminApi,
    base: String,
    client: reqwest::Client,
}

async fn admin() -> Admin {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());
    Admin {
        running,
        base,
        client: reqwest::Client::new(),
    }
}

impl Admin {
    /// `method path` with `body` sent as text; the answer's status and raw text.
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&str>) -> (u16, String) {
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(body.to_owned());
        }
        let response = request.send().await.expect("admin call");
        let status = response.status().as_u16();
        (status, compact(&response.text().await.expect("body")))
    }

    /// Create an imposter from `document` (text) on an engine-assigned port; return the port.
    async fn create(&self, document: &str) -> u16 {
        let (status, text) = self
            .call(reqwest::Method::POST, "/imposters", Some(document))
            .await;
        assert_eq!(status, 201, "{text}");
        let port = serde_json::from_str::<serde_json::Value>(&text).expect("json")["port"]
            .as_u64()
            .expect("port");
        u16::try_from(port).expect("a port")
    }
}

/// `text` with the whitespace between JSON tokens removed (the admin API pretty-prints), key order
/// untouched.
fn compact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut in_string, mut escaped) = (false, false);
    for c in text.chars() {
        if in_string {
            out.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

/// `GET /` over a raw socket; the response head's header names, in wire order.
async fn served_header_names(port: u16) -> Vec<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .expect("response arrives")
        .expect("read");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, _) = text.split_once("\r\n\r\n").expect("a response head");
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.to_ascii_lowercase())
        .filter(|name| ["zeta", "alpha", "mid"].contains(&name.as_str()))
        .collect()
}

const ORDERED_IMPOSTER: &str = r#"{"protocol": "http", "port": 0, "stubs": [
    {"responses": [{"is": {"headers": {"Zeta": "1", "Alpha": "2", "Mid": "3"}, "body": "ok"}}]}
]}"#;

#[tokio::test]
async fn replayable_export_keeps_stub_header_order() {
    let admin = admin().await;
    let port = admin.create(ORDERED_IMPOSTER).await;

    for path in [
        format!("/imposters/{port}?replayable=true"),
        "/imposters?replayable=true".to_owned(),
        "/imposters?replayable=true&removeProxies=true".to_owned(),
        // The non-replayable views print the same stub, and must not sort it either.
        format!("/imposters/{port}"),
        "/imposters?removeProxies=true".to_owned(),
    ] {
        let (status, text) = admin.call(reqwest::Method::GET, &path, None).await;
        assert_eq!(status, 200, "GET {path}: {text}");
        assert!(text.contains(WRITTEN), "GET {path}: {text}");
    }

    // `DELETE /imposters` answers with the replayable export of what it deleted.
    let (status, text) = admin
        .call(reqwest::Method::DELETE, "/imposters", None)
        .await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains(WRITTEN), "DELETE /imposters: {text}");
    admin.running.shutdown().await;
}

#[tokio::test]
async fn delete_one_imposter_prints_the_written_order() {
    let admin = admin().await;
    for query in ["", "?replayable=true"] {
        let port = admin.create(ORDERED_IMPOSTER).await;
        let path = format!("/imposters/{port}{query}");
        let (status, text) = admin.call(reqwest::Method::DELETE, &path, None).await;
        assert_eq!(status, 200, "DELETE {path}: {text}");
        assert!(text.contains(WRITTEN), "DELETE {path}: {text}");
    }
    admin.create(ORDERED_IMPOSTER).await;
    let (_, text) = admin
        .call(reqwest::Method::DELETE, "/imposters?replayable=false", None)
        .await;
    assert!(text.contains(WRITTEN), "DELETE /imposters (detail): {text}");
    admin.running.shutdown().await;
}

#[tokio::test]
async fn save_then_load_serves_headers_in_written_order() {
    let admin = admin().await;
    let port = admin.create(ORDERED_IMPOSTER).await;
    assert_eq!(served_header_names(port).await, ["zeta", "alpha", "mid"]);

    // `rift save` writes this body verbatim; loading it is `PUT /imposters` with the same text.
    let (status, saved) = admin
        .call(reqwest::Method::GET, "/imposters?replayable=true", None)
        .await;
    assert_eq!(status, 200, "{saved}");
    // Load into an empty server, as `rift start --configfile` would: a `PUT` over the live
    // imposter would keep it (its config hashes the same), and so its order, whatever the save says.
    let (status, text) = admin
        .call(reqwest::Method::DELETE, "/imposters", None)
        .await;
    assert_eq!(status, 200, "{text}");
    let (status, text) = admin
        .call(reqwest::Method::PUT, "/imposters", Some(&saved))
        .await;
    assert_eq!(status, 200, "{text}");

    assert_eq!(served_header_names(port).await, ["zeta", "alpha", "mid"]);
    let (_, again) = admin
        .call(reqwest::Method::GET, "/imposters?replayable=true", None)
        .await;
    assert_eq!(
        again, saved,
        "a save of the loaded save is the same document"
    );
    admin.running.shutdown().await;
}

#[tokio::test]
async fn https_replayable_export_still_carries_tls_keys_once() {
    rift_http_proxy::install_default_crypto_provider();
    let admin = admin().await;
    let c = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("certificate");
    let document = serde_json::json!({
        "protocol": "https",
        "port": 0,
        "cert": c.cert.pem(),
        "key": c.signing_key.serialize_pem(),
        "ciphers": "ecdhe-rsa-aes128-gcm-sha256",
        "stubs": [],
    })
    .to_string();
    let port = admin.create(&document).await;

    for path in [
        format!("/imposters/{port}?replayable=true"),
        "/imposters?replayable=true".to_owned(),
    ] {
        let (status, text) = admin.call(reqwest::Method::GET, &path, None).await;
        assert_eq!(status, 200, "GET {path}: {text}");
        for key in [
            "cert",
            "key",
            "mutualAuth",
            "rejectUnauthorized",
            "ca",
            "ciphers",
        ] {
            assert_eq!(
                text.matches(&format!("\"{key}\":")).count(),
                1,
                "GET {path}: `{key}` printed exactly once: {text}"
            );
        }
        assert!(text.contains(r#""ca":null"#), "GET {path}: {text}");
        assert!(text.contains(r#""mutualAuth":false"#), "GET {path}: {text}");
        assert!(
            text.contains(r#""rejectUnauthorized":false"#),
            "GET {path}: {text}"
        );
        // Printed upper-cased, as Mountebank prints it — the TLS header's value wins.
        assert!(
            text.contains(r#""ciphers":"ECDHE-RSA-AES128-GCM-SHA256""#),
            "GET {path}: {text}"
        );
    }
    admin.running.shutdown().await;
}

#[tokio::test]
async fn scenario_stub_door_keeps_header_order() {
    let admin = admin().await;
    let port = admin
        .create(r#"{"protocol": "http", "port": 0, "stubs": []}"#)
        .await;
    // The imposter's default flow is its port, so a request without a flow header lands in it.
    let path = format!("/imposters/{port}/spaces/{port}/stubs");
    let (status, text) = admin
        .call(
            reqwest::Method::POST,
            &path,
            Some(r#"{"responses": [{"is": {"headers": {"Zeta": "1", "Alpha": "2", "Mid": "3"}, "body": "ok"}}]}"#),
        )
        .await;
    assert_eq!(status, 201, "{text}");
    assert!(text.contains(WRITTEN), "POST {path}: {text}");

    let (status, text) = admin.call(reqwest::Method::GET, &path, None).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains(WRITTEN), "GET {path}: {text}");
    let (status, text) = admin
        .call(
            reqwest::Method::GET,
            &format!("/imposters/{port}/spaces/{port}"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains(WRITTEN), "GET space: {text}");

    assert_eq!(served_header_names(port).await, ["zeta", "alpha", "mid"]);
    admin.running.shutdown().await;
}
