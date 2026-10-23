//! Issue #1639: two Mountebank shapes around `shellTransform`.
//!
//! - A `behaviors` array element whose `copy`, `lookup` or `shellTransform` is a list is refused at
//!   a config door with Mountebank's first error, as Mountebank 2.9.5's `behaviorsValidator.js`
//!   validates the value as written. The `_behaviors` object form is upcast one element per item
//!   first, so it still takes a list, and a stored config holding one still loads.
//! - A shellTransform command reads an `is` body written as JSON (a number, an object) as that value
//!   in `MB_RESPONSE.body`, as Mountebank hands it the response object.

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

struct Admin {
    client: reqwest::Client,
    base: String,
    manager: Arc<ImposterManager>,
    _running: rift_http_proxy::admin_api::RunningAdminApi,
}

impl Admin {
    async fn start() -> Self {
        let manager = Arc::new(ImposterManager::new());
        let running =
            AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
                .with_allow_injection(true)
                .bind()
                .await
                .expect("admin API binds");
        Self {
            client: reqwest::Client::new(),
            base: format!("http://{}", running.local_addr()),
            manager,
            _running: running,
        }
    }

    /// `POST /imposters` with one stub holding `response`: the status and the body text.
    async fn post(&self, response: &Value) -> (u16, String) {
        let reply = self
            .client
            .post(format!("{}/imposters", self.base))
            .json(&json!({"protocol": "http", "stubs": [{"responses": [response]}]}))
            .send()
            .await
            .expect("POST /imposters");
        let status = reply.status().as_u16();
        (status, reply.text().await.expect("body"))
    }

    /// Admits `response` and returns the port, which the caller stops.
    async fn admits(&self, response: &Value) -> u16 {
        let (status, body) = self.post(response).await;
        assert_eq!(status, 201, "{response} must be admitted: {body}");
        let created: Value = serde_json::from_str(&body).expect("201 body is JSON");
        created["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .expect("assigned port")
    }

    async fn get(&self, port: u16) -> String {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .expect("client");
        client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect("GET")
            .text()
            .await
            .expect("body")
    }

    async fn stop(&self, port: u16) {
        let _ = self.manager.delete_imposter(port).await;
    }
}

const SHELL: &str = "shellTransform behavior \"shellTransform\" field must be a string, \
                     representing the path to a command line application";

fn copy() -> Value {
    json!({"from": "path", "into": "${P}", "using": {"method": "regex", "selector": ".+"}})
}

fn lookup() -> Value {
    json!({"key": {"from": "path", "using": {"method": "regex", "selector": ".+"}},
           "fromDataSource": {"csv": {"path": "rows.csv", "keyColumn": "id"}},
           "into": "${R}"})
}

/// Each list-valued element Mountebank refuses, with its first error.
fn list_valued() -> Vec<(Value, &'static str)> {
    vec![
        (json!({"shellTransform": ["echo a", "echo b"]}), SHELL),
        (json!({"shellTransform": []}), SHELL),
        (
            json!({"copy": [copy()]}),
            "copy behavior \"from\" field required",
        ),
        (
            json!({"lookup": [lookup()]}),
            "lookup behavior \"key\" field required",
        ),
        (
            json!({"lookup": []}),
            "lookup behavior \"key\" field required",
        ),
    ]
}

#[tokio::test]
async fn a_list_valued_behavior_in_an_array_element_is_a_400() {
    let admin = Admin::start().await;
    for (element, message) in list_valued() {
        let response = json!({"is": {"body": "a"}, "behaviors": [element]});
        let (status, body) = admin.post(&response).await;
        assert_eq!(status, 400, "{response} must be refused: {body}");
        let envelope: Value = serde_json::from_str(&body).expect("400 body is JSON");
        let said = envelope["errors"][0]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(said.contains(message), "{response}: {body}");
    }
}

#[tokio::test]
async fn the_object_form_and_one_command_per_element_are_admitted() {
    let admin = Admin::start().await;
    for response in [
        json!({"is": {"body": "a"}, "_behaviors": {"shellTransform": ["echo a", "echo b"]}}),
        json!({"is": {"body": "${P}"}, "_behaviors": {"copy": [copy()]}}),
        json!({"is": {"body": "a"}, "behaviors": [
            {"shellTransform": "echo a"}, {"shellTransform": "echo b"}
        ]}),
    ] {
        let port = admin.admits(&response).await;
        admin.stop(port).await;
    }
}

#[test]
fn a_configfile_holding_one_fails_to_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let file = json!({"imposters": [{"port": 4612, "protocol": "http", "stubs": [{"responses": [
        {"is": {}, "behaviors": [{"shellTransform": ["echo a", "echo b"]}]}
    ]}]}]});
    std::fs::write(&path, file.to_string()).expect("write configfile");
    let err = load_configs(&ConfigSource::File {
        path,
        no_parse: true,
    })
    .expect_err("a --configfile is a door");
    assert!(format!("{err:#}").contains(SHELL), "{err:#}");
}

/// Engines up to v0.23.0 admitted every one of these, and wrote copy lists inside one element
/// before #1199, so a `--datadir` reload still loads them: an upgrade never drops a stored imposter.
#[test]
fn a_datadir_file_holding_one_still_loads() {
    for (element, _) in list_valued() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = json!({"port": 4612, "protocol": "http",
                          "stubs": [{"responses": [{"is": {}, "behaviors": [element]}]}]});
        std::fs::write(dir.path().join("4612.json"), file.to_string()).expect("write datadir file");
        let loaded = load_configs(&ConfigSource::Dir(dir.path().to_path_buf()))
            .unwrap_or_else(|e| panic!("{file} must load from a datadir: {e:#}"));
        assert_eq!(loaded.len(), 1, "{file}");
    }
}

/// What the one command saw as `MB_RESPONSE.body`, for an `is` response written with `body`.
async fn mb_response_body(admin: &Admin, body: Value) -> Value {
    let port = admin
        .admits(&json!({"is": {"body": body}, "_behaviors": {
            "shellTransform": r#"printf '{"body":%s}' "$MB_RESPONSE" #"#
        }}))
        .await;
    let served = admin.get(port).await;
    admin.stop(port).await;
    let response: Value = serde_json::from_str(&served).expect("the command printed JSON");
    response["body"].clone()
}

#[tokio::test]
async fn mb_response_body_keeps_the_is_bodys_json_type() {
    let admin = Admin::start().await;
    for body in [
        json!(3),
        json!(true),
        json!({"a": 1}),
        json!([1, "x"]),
        json!("text"),
    ] {
        assert_eq!(mb_response_body(&admin, body.clone()).await, body);
    }
}
