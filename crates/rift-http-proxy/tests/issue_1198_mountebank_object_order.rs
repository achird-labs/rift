//! Issue #1198, part B: Mountebank runs the object form (`_behaviors`) in the order its
//! compatibility layer upcasts it — wait, lookup, copy, shellTransform, decorate
//! (`compatibility.js`). Rift documented that as Mountebank's order and ran wait, copy, lookup,
//! decorate, shellTransform, which differs twice.

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

    async fn serve(&self, body: &str, behaviors: Value) -> u16 {
        let port = free_port();
        let imposter = json!({"port": port, "protocol": "http", "stubs": [{"responses": [
            {"is": {"body": body}, "_behaviors": behaviors}
        ]}]});
        let response = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(&imposter)
            .send()
            .await
            .expect("POST");
        assert_eq!(response.status().as_u16(), 201, "{imposter}");
        port
    }

    async fn body(&self, port: u16, path_and_query: &str) -> String {
        self.client
            .get(format!("http://127.0.0.1:{port}{path_and_query}"))
            .send()
            .await
            .expect("GET")
            .text()
            .await
            .expect("body")
    }

    async fn saved_behaviors(&self, port: u16) -> Value {
        let imposter: Value = self
            .client
            .get(format!("{}/imposters/{port}", self.url))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json");
        imposter["stubs"][0]["responses"][0]["behaviors"].clone()
    }

    async fn stop(&self, port: u16) {
        let _ = self.manager.delete_imposter(port).await;
    }
}

const APPEND_D: &str = "function (request, response) { response.body = response.body + 'D'; }";

/// The finding's last row: Mountebank serves `xSD` — shellTransform, then decorate. The
/// shellTransform here ignores its input and prints `S`, so the order is visible in the body.
#[tokio::test]
async fn shell_transform_runs_before_decorate_in_the_object_form() {
    let admin = Admin::start().await;
    let port = admin
        .serve(
            "x",
            json!({"decorate": APPEND_D, "shellTransform": "printf S"}),
        )
        .await;
    assert_eq!(admin.body(port, "/p").await, "SD");
    admin.stop(port).await;
}

/// lookup runs before copy, so request text a copy inserts is never re-scanned for lookup tokens.
/// In the old order a client could put `${R}[secret]` in a copied field and read any column of the
/// matched row.
#[tokio::test]
async fn copied_request_text_is_not_expanded_by_a_lookup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let csv = dir.path().join("rows.csv");
    std::fs::write(&csv, "id,name,secret\n1,alice,s3cr3t\n").expect("csv");
    let admin = Admin::start().await;
    let regex = json!({"method": "regex", "selector": ".+"});
    let port = admin
        .serve(
            "name=${R}[name] q=${Q}",
            json!({
                "copy": {"from": {"query": "q"}, "into": "${Q}", "using": regex},
                "lookup": {
                    "key": {"from": {"query": "id"}, "using": regex},
                    "fromDataSource": {"csv": {"path": csv.to_string_lossy(), "keyColumn": "id"}},
                    "into": "${R}"
                }
            }),
        )
        .await;
    assert_eq!(
        admin.body(port, "/p?id=1&q=%24%7BR%7D%5Bsecret%5D").await,
        "name=alice q=${R}[secret]"
    );
    admin.stop(port).await;
}

/// What is written back is what runs.
#[tokio::test]
async fn an_object_form_block_is_written_back_in_mountebank_order() {
    let admin = Admin::start().await;
    let copy =
        json!({"from": "path", "into": "${P}", "using": {"method": "regex", "selector": ".+"}});
    let port = admin
        .serve(
            "x",
            json!({"decorate": APPEND_D, "shellTransform": "cat", "copy": copy.clone(), "wait": 1}),
        )
        .await;
    assert_eq!(
        admin.saved_behaviors(port).await,
        json!([{"wait": 1}, {"copy": copy}, {"shellTransform": "cat"}, {"decorate": APPEND_D}])
    );
    admin.stop(port).await;
}
