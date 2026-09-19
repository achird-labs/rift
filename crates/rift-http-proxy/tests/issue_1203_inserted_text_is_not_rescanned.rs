//! Issue #1203: text the engine substitutes into a response is never scanned for tokens again. A
//! token is expanded only if at least part of it was written by the author.
//!
//! Every substitution pass used to search the output of the pass before it, so a client that sent
//! a complete token had it expanded: `${R}[secret]` served a CSV column the config never exposed,
//! and `${request.headers.<name>}` reflected a header an ingress added.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

struct Admin {
    client: reqwest::Client,
    url: String,
    manager: Arc<ImposterManager>,
}

impl Admin {
    async fn start() -> Self {
        let manager = Arc::new(ImposterManager::new());
        let admin_port = free_port();
        let server = AdminApiServer::new(
            format!("127.0.0.1:{admin_port}").parse().expect("addr"),
            manager.clone(),
            None,
        )
        .with_allow_injection(true);
        tokio::spawn(server.run());
        let url = format!("http://127.0.0.1:{admin_port}");
        let client = reqwest::Client::new();
        for _ in 0..100 {
            if client.get(format!("{url}/imposters")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Admin {
            client,
            url,
            manager,
        }
    }

    /// Create an imposter serving `response` and return its port.
    async fn serve(&self, response: Value) -> u16 {
        let port = free_port();
        let imposter =
            json!({"port": port, "protocol": "http", "stubs": [{"responses": [response]}]});
        let created = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(&imposter)
            .send()
            .await
            .expect("POST");
        assert_eq!(created.status().as_u16(), 201, "{imposter}");
        port
    }

    async fn get(&self, port: u16, path_and_query: &str) -> reqwest::Response {
        self.client
            .get(format!("http://127.0.0.1:{port}{path_and_query}"))
            .header("x-internal", "gateway-added")
            .send()
            .await
            .expect("GET")
    }

    async fn body(&self, port: u16, path_and_query: &str) -> String {
        self.get(port, path_and_query)
            .await
            .text()
            .await
            .expect("body")
    }

    async fn stop(&self, port: u16) {
        let _ = self.manager.delete_imposter(port).await;
    }
}

/// `id,name,secret` / `1,alice,s3cr3t`, looked up by `?id=` into `${R}`.
struct Rows {
    _dir: tempfile::TempDir,
    lookup: Value,
}

fn rows(csv: &str) -> Rows {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rows.csv");
    std::fs::write(&path, csv).expect("csv");
    Rows {
        lookup: json!({
            "key": {"from": {"query": "id"}, "using": {"method": "regex", "selector": ".+"}},
            "fromDataSource": {"csv": {"path": path.to_string_lossy(), "keyColumn": "id"}},
            "into": "${R}"
        }),
        _dir: dir,
    }
}

const SECRET_ROW: &str = "id,name,secret\n1,alice,s3cr3t\n";
/// `?id=1&q=${R}[secret]`, percent-encoded.
const ASKS_FOR_SECRET: &str = "/p?id=1&q=%24%7BR%7D%5Bsecret%5D";

fn copy(from_query: &str, into: &str) -> Value {
    json!({"from": {"query": from_query}, "into": into, "using": {"method": "regex", "selector": ".+"}})
}

/// Route A: `${request.*}` reflects the client's text before behaviors run.
#[tokio::test]
async fn a_reflected_query_is_not_expanded_by_a_lookup() {
    let admin = Admin::start().await;
    let rows = rows(SECRET_ROW);
    let port = admin
        .serve(json!({
            "is": {"body": "q=${request.query.q} name=${R}[name]"},
            "_behaviors": {"lookup": rows.lookup}
        }))
        .await;
    assert_eq!(
        admin.body(port, ASKS_FOR_SECRET).await,
        "q=${R}[secret] name=alice"
    );
    admin.stop(port).await;
}

/// Route B: the `{{ }}` pass reflects it.
#[tokio::test]
async fn a_templated_query_is_not_expanded_by_a_lookup() {
    let admin = Admin::start().await;
    let rows = rows(SECRET_ROW);
    let port = admin
        .serve(json!({
            "is": {"body": "q={{request.query.q}} name=${R}[name]"},
            "_rift": {"templated": true},
            "_behaviors": {"lookup": rows.lookup}
        }))
        .await;
    assert_eq!(
        admin.body(port, ASKS_FOR_SECRET).await,
        "q=${R}[secret] name=alice"
    );
    admin.stop(port).await;
}

/// Route C: an explicit `[copy, lookup]` program. Mountebank 2.9.1 serves the secret here; Rift
/// deliberately does not (docs/mountebank/behaviors.md).
#[tokio::test]
async fn copied_text_is_not_expanded_by_a_later_lookup_step() {
    let admin = Admin::start().await;
    let rows = rows(SECRET_ROW);
    let port = admin
        .serve(json!({
            "is": {"body": "q=${Q} name=${R}[name]"},
            "behaviors": [{"copy": copy("q", "${Q}")}, {"lookup": rows.lookup}]
        }))
        .await;
    assert_eq!(
        admin.body(port, ASKS_FOR_SECRET).await,
        "q=${R}[secret] name=alice"
    );
    admin.stop(port).await;
}

/// Route D: header values carry the same provenance as the body.
#[tokio::test]
async fn a_reflected_header_value_is_not_expanded_by_a_lookup() {
    let admin = Admin::start().await;
    let rows = rows(SECRET_ROW);
    let port = admin
        .serve(json!({
            "is": {"body": "name=${R}[name]", "headers": {"X-Echo": "${request.query.q}"}},
            "_behaviors": {"lookup": rows.lookup}
        }))
        .await;
    let response = admin.get(port, ASKS_FOR_SECRET).await;
    assert_eq!(response.headers()["x-echo"], "${R}[secret]");
    assert_eq!(response.text().await.expect("body"), "name=alice");
    admin.stop(port).await;
}

/// Route F, no lookup involved: the `${request.*}` pass used to re-scan what `{{ }}` inserted, so a
/// client could name any request header — including one an ingress added — and read it back.
#[tokio::test]
async fn a_templated_query_naming_a_request_header_is_served_as_written() {
    let admin = Admin::start().await;
    let port = admin
        .serve(json!({
            "is": {"body": "q={{request.query.q}}"},
            "_rift": {"templated": true}
        }))
        .await;
    assert_eq!(
        admin
            .body(port, "/p?q=%24%7Brequest.headers.x-internal%7D")
            .await,
        "q=${request.headers.x-internal}"
    );
    admin.stop(port).await;
}

/// The author can still opt in to a client-chosen column: the match spans the authored `${R}[` and
/// `]`, so it is not wholly inside the copied text.
#[tokio::test]
async fn a_token_the_author_split_around_a_copy_is_still_expanded() {
    let admin = Admin::start().await;
    let rows = rows(SECRET_ROW);
    let port = admin
        .serve(json!({
            "is": {"body": "${R}[${COL}]"},
            "behaviors": [{"copy": copy("col", "${COL}")}, {"lookup": rows.lookup}]
        }))
        .await;
    assert_eq!(admin.body(port, "/p?id=1&col=name").await, "alice");
    admin.stop(port).await;
}

/// A script's output is the author's text: a decorate between the copy and the lookup hands the
/// copied text back as authored, so the lookup expands it. Pinned so the exception stays deliberate.
#[tokio::test]
async fn a_decorate_between_copy_and_lookup_hands_back_authored_text() {
    let admin = Admin::start().await;
    let rows = rows(SECRET_ROW);
    let port = admin
        .serve(json!({
            "is": {"body": "q=${Q}"},
            "behaviors": [
                {"copy": copy("q", "${Q}")},
                {"decorate": "function (request, response) {}"},
                {"lookup": rows.lookup}
            ]
        }))
        .await;
    assert_eq!(
        admin.body(port, "/p?id=1&q=%24%7BR%7D%5Bname%5D").await,
        "q=alice"
    );
    admin.stop(port).await;
}

/// A proxied body is the upstream's text, chosen by the author through `proxy.to`, so a proxy
/// `lookup` (#1189) still expands the tokens it returns.
#[tokio::test]
async fn a_proxy_lookup_still_expands_tokens_the_upstream_returns() {
    let admin = Admin::start().await;
    let rows = rows(SECRET_ROW);
    let upstream = admin.serve(json!({"is": {"body": "n=${R}[name]"}})).await;
    let port = admin
        .serve(json!({
            "proxy": {"to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyTransparent"},
            "_behaviors": {"lookup": rows.lookup}
        }))
        .await;
    assert_eq!(admin.body(port, "/p?id=1").await, "n=alice");
    admin.stop(port).await;
    admin.stop(upstream).await;
}

/// A cell holding another column's token is served as written. The row's columns are visited in
/// hash order, so before #1203 the result varied from request to request: repeat to catch it.
#[tokio::test]
async fn a_cell_holding_another_columns_token_is_served_as_written() {
    let admin = Admin::start().await;
    let rows = rows("id,name,secret\n1,${R}[secret],s3cr3t\n");
    let port = admin
        .serve(json!({
            "is": {"body": "n=${R}[name]"},
            "_behaviors": {"lookup": rows.lookup}
        }))
        .await;
    for _ in 0..50 {
        assert_eq!(admin.body(port, "/p?id=1").await, "n=${R}[secret]");
    }
    admin.stop(port).await;
}

/// An authored character cannot complete a token the client started: the client sends
/// `${request.headers.x-internal` and the author's own closing `}` follows it.
#[tokio::test]
async fn an_authored_closing_brace_does_not_complete_a_reflected_token() {
    let admin = Admin::start().await;
    let port = admin
        .serve(json!({
            "is": {"body": "{\"id\":{{request.query.id}}}"},
            "_rift": {"templated": true}
        }))
        .await;
    assert_eq!(
        admin
            .body(port, "/p?id=%24%7Brequest.headers.x-internal")
            .await,
        "{\"id\":${request.headers.x-internal}"
    );
    admin.stop(port).await;
}

/// Nor can an authored character open one: a literal `$` (a price) before a reflected value.
#[tokio::test]
async fn an_authored_dollar_does_not_open_a_reflected_token() {
    let admin = Admin::start().await;
    let port = admin
        .serve(json!({
            "is": {"body": "price=${{request.query.amount}}"},
            "_rift": {"templated": true}
        }))
        .await;
    assert_eq!(
        admin
            .body(port, "/p?amount=%7Brequest.headers.x-internal%7D")
            .await,
        "price=${request.headers.x-internal}"
    );
    admin.stop(port).await;
}

/// The same for a lookup: the client sends `${R}[secret` and the author's `]` follows it.
#[tokio::test]
async fn an_authored_bracket_does_not_complete_a_reflected_lookup_token() {
    let admin = Admin::start().await;
    let rows = rows(SECRET_ROW);
    let port = admin
        .serve(json!({
            "is": {"body": "ids=[${request.query.q}] name=${R}[name]"},
            "_behaviors": {"lookup": rows.lookup}
        }))
        .await;
    assert_eq!(
        admin.body(port, "/p?id=1&q=%24%7BR%7D%5Bsecret").await,
        "ids=[${R}[secret] name=alice"
    );
    admin.stop(port).await;
}
