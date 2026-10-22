//! Issue #1582: a config door validates a response's behaviors as Mountebank 2.9.5's
//! `behaviorsValidator.js` and `dryRunValidator.js` do. Each new refusal answers `400` in
//! Mountebank's words; each shape Mountebank admits is admitted; a lookup `key.index` is read as
//! Mountebank reads `keyValues[index || 0]`, so an index that is not a match position finds no row
//! instead of refusing the imposter.

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
                .with_scripts_dir(std::env::temp_dir())
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
    async fn post(&self, response: Value) -> (u16, String) {
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

    async fn refuses(&self, response: Value, message: &str) {
        let (status, body) = self.post(response.clone()).await;
        assert_eq!(status, 400, "{response} must be refused: {body}");
        let envelope: Value = serde_json::from_str(&body).expect("400 body is JSON");
        let said = envelope["errors"][0]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(said.contains(message), "{response}: {body}");
    }

    /// Admits `response` and returns the port, which the caller deletes.
    async fn admits(&self, response: Value) -> u16 {
        let (status, body) = self.post(response.clone()).await;
        assert_eq!(status, 201, "{response} must be admitted: {body}");
        let created: Value = serde_json::from_str(&body).expect("201 body is JSON");
        created["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .expect("assigned port")
    }

    async fn body(&self, port: u16, path: &str) -> String {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .expect("client");
        client
            .get(format!("http://127.0.0.1:{port}{path}"))
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

const REPEAT: &str = "\"repeat\" field must be an integer greater than 0";

fn copy() -> Value {
    json!({"from": "path", "into": "${P}", "using": {"method": "regex", "selector": ".+"}})
}

// ---- repeat ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_zero_or_null_repeat_is_refused_wherever_it_is_written() {
    let admin = Admin::start().await;
    for response in [
        json!({"is": {"body": "a"}, "repeat": 0}),
        json!({"is": {"body": "a"}, "repeat": null}),
        json!({"is": {"body": "a"}, "_behaviors": {"repeat": 0}}),
        json!({"is": {"body": "a"}, "_behaviors": {"repeat": null}}),
        json!({"is": {"body": "a"}, "_behaviors": {"wait": 1, "repeat": null}}),
        json!({"proxy": {"to": "http://127.0.0.1:1"}, "repeat": 0}),
    ] {
        admin.refuses(response, REPEAT).await;
    }
}

/// `repeat` is a response field in Mountebank, not a behavior, so an element holding it is an
/// unrecognized behavior, whatever its value (Mountebank 2.9.5 names the element for `0` too).
#[tokio::test]
async fn a_repeat_element_in_the_behaviors_array_is_refused() {
    let admin = Admin::start().await;
    admin
        .refuses(
            json!({"is": {"body": "a"}, "behaviors": [{"wait": 1}, {"repeat": 2}]}),
            "Unrecognized behavior: \"repeat\"",
        )
        .await;
    admin
        .refuses(
            json!({"is": {"body": "a"}, "behaviors": [{"wait": 1}, {"repeat": 0}]}),
            "Unrecognized behavior: \"repeat\"",
        )
        .await;
}

/// The #1162/#1188 rule still holds for the values both refuse, and a positive `repeat` written
/// where Mountebank reads it is admitted.
#[tokio::test]
async fn a_positive_repeat_on_the_response_or_in_the_object_form_is_admitted() {
    let admin = Admin::start().await;
    for response in [
        json!({"is": {"body": "a"}, "repeat": 2}),
        json!({"is": {"body": "a"}, "_behaviors": {"repeat": 2}}),
    ] {
        let port = admin.admits(response).await;
        admin.stop(port).await;
    }
    for repeat in [json!("0"), json!(true), json!(-1), json!("-1")] {
        let (status, _) = admin
            .post(json!({"is": {"body": "a"}, "repeat": repeat}))
            .await;
        assert_eq!(status, 400, "{repeat}");
    }
}

// ---- one behavior type per element ----------------------------------------------------------------

#[tokio::test]
async fn an_unknown_only_element_is_refused() {
    let admin = Admin::start().await;
    admin
        .refuses(
            json!({"is": {"body": "a"}, "behaviors": [{"nosuch": 1}]}),
            "Unrecognized behavior: \"nosuch\"",
        )
        .await;
}

#[tokio::test]
async fn an_element_with_two_behavior_types_is_refused() {
    let admin = Admin::start().await;
    admin
        .refuses(
            json!({"is": {"body": "${P}"}, "behaviors": [{"wait": 1, "copy": copy()}]}),
            "Each behavior object must have only one behavior type",
        )
        .await;
}

/// Mountebank lets a custom key sit beside a known behavior, and upcasts the `_behaviors` object
/// form key by key, so neither is refused.
#[tokio::test]
async fn an_unknown_key_beside_a_behavior_and_the_object_form_are_admitted() {
    let admin = Admin::start().await;
    for response in [
        json!({"is": {"body": "a"}, "behaviors": [{"wait": 1, "nosuch": 1}]}),
        json!({"is": {"body": "${P}"}, "_behaviors": {"wait": 1, "copy": [copy()]}}),
        json!({"is": {"body": "a"}, "_behaviors": {"nosuch": 1}}),
    ] {
        let port = admin.admits(response).await;
        admin.stop(port).await;
    }
}

/// What Rift writes back must load again: a step for a key that is not a behavior is written inside
/// the element before it, never as an element of its own, which Mountebank and Rift both refuse.
#[tokio::test]
async fn an_unknown_key_is_written_back_in_a_shape_that_loads_again() {
    let admin = Admin::start().await;
    for response in [
        json!({"is": {"body": "a"}, "behaviors": [{"wait": 1, "nosuch": 1}]}),
        json!({"is": {"body": "a"}, "_behaviors": {"wait": 1, "nosuch": 1}}),
        json!({"is": {"body": "a"}, "_behaviors": {"nosuch": 1}}),
    ] {
        let port = admin.admits(response.clone()).await;
        let saved: Value = admin
            .client
            .get(format!("{}/imposters/{port}?replayable=true", admin.base))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json");
        admin.stop(port).await;
        let written = saved["stubs"][0]["responses"][0].clone();
        let again = admin.admits(written.clone()).await;
        admin.stop(again).await;
        if let Some(elements) = written["behaviors"].as_array() {
            for element in elements {
                assert!(
                    ["wait", "lookup", "copy", "shellTransform", "decorate"]
                        .iter()
                        .any(|k| element.get(k).is_some()),
                    "{response} was written back as {written}"
                );
            }
        }
    }
}

// ---- from objects ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_from_object_without_exactly_one_key_is_refused() {
    let admin = Admin::start().await;
    let using = json!({"method": "regex", "selector": ".+"});
    for from in [json!({"query": "q", "headers": "x"}), json!({})] {
        admin
            .refuses(
                json!({"is": {"body": "${P}"}, "_behaviors": {"copy": {
                    "from": from, "into": "${P}", "using": using
                }}}),
                "copy behavior \"from\" field must have exactly one key",
            )
            .await;
        admin
            .refuses(
                json!({"is": {"body": "${R}"}, "behaviors": [{"lookup": {
                    "key": {"from": from, "using": using},
                    "fromDataSource": {"csv": {"path": "/nonexistent.csv", "keyColumn": "id"}},
                    "into": "${R}"
                }}]}),
                "lookup behavior \"key.from\" field must have exactly one key",
            )
            .await;
    }
}

// ---- lookup key.index -----------------------------------------------------------------------------

/// Mountebank reads the key as `keyValues[key.index || 0]`: a falsy index is the whole match, a
/// non-negative integer or a canonical numeric string is that match, and anything else is
/// `undefined`, so no row is found and the token is left.
#[tokio::test]
async fn a_lookup_key_index_is_read_as_mountebank_reads_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let csv = dir.path().join("rows.csv");
    std::fs::write(&csv, "id,name\n/ab-cd,whole\nab,first\ncd,second\n").expect("csv");
    let admin = Admin::start().await;
    let left = "${row}[name]";
    for (index, served) in [
        (json!(-1), left),
        (json!(1.5), left),
        (json!("x"), left),
        (json!("01"), left),
        (json!(true), left),
        (json!({}), left),
        (json!(5), left),
        (json!("1"), "first"),
        (json!(2), "second"),
        (json!(1.0), "first"),
        (json!(0), "whole"),
        (json!(""), "whole"),
        (json!(false), "whole"),
        (json!("0"), "whole"),
    ] {
        let port = admin
            .admits(json!({"is": {"body": left}, "_behaviors": {"lookup": {
                "key": {"from": "path",
                        "using": {"method": "regex", "selector": "^/(ab|cd)-(ab|cd)$"},
                        "index": index},
                "fromDataSource": {"csv": {"path": csv.to_string_lossy(), "keyColumn": "id"}},
                "into": "${row}"
            }}}))
            .await;
        assert_eq!(admin.body(port, "/ab-cd").await, served, "index {index}");
        admin.stop(port).await;
    }
}

/// The index is printed back as written, so a saved file means what the posted one did.
#[tokio::test]
async fn a_lookup_key_index_is_written_back_as_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let csv = dir.path().join("rows.csv");
    std::fs::write(&csv, "id,name\n").expect("csv");
    let admin = Admin::start().await;
    for index in [json!(-1), json!("1"), json!(1.5)] {
        let port = admin
            .admits(json!({"is": {"body": "${row}"}, "_behaviors": {"lookup": {
                "key": {"from": "path", "using": {"method": "regex", "selector": ".+"},
                        "index": index},
                "fromDataSource": {"csv": {"path": csv.to_string_lossy(), "keyColumn": "id"}},
                "into": "${row}"
            }}}))
            .await;
        let saved: Value = admin
            .client
            .get(format!("{}/imposters/{port}", admin.base))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json");
        assert_eq!(
            saved["stubs"][0]["responses"][0]["behaviors"][0]["lookup"]["key"]["index"],
            index
        );
        admin.stop(port).await;
    }
}

// ---- what older engines stored --------------------------------------------------------------------

/// Every shape this issue refuses at a door was admitted by older engines, and some were written by
/// Rift itself (a `{"repeat": n}` element before v0.18.0, an element holding only an unknown key
/// before #1582). A `--datadir` holds what an engine wrote, so a reload of one still loads it: an
/// upgrade never drops a stored imposter. Mountebank does not validate its datadir on load either.
#[test]
fn a_datadir_file_holding_a_shape_older_engines_stored_still_loads() {
    let using = json!({"method": "regex", "selector": ".+"});
    let responses = [
        json!({"is": {}, "behaviors": [{"wait": 1}, {"repeat": 2}]}),
        json!({"is": {}, "behaviors": [{"repeat": 0}]}),
        json!({"is": {}, "behaviors": [{"wait": 1}, {"nosuch": 1}]}),
        json!({"is": {}, "behaviors": [{"wait": 1, "decorate": "function () {}"}]}),
        json!({"is": {}, "repeat": null}),
        json!({"is": {}, "_behaviors": {"wait": 1, "repeat": null}}),
        json!({"is": {}, "repeat": 0}),
        json!({"is": {}, "_behaviors": {"repeat": 0}}),
        json!({"is": {}, "behaviors": [{"copy": {
            "from": {"query": "q", "headers": "x"}, "into": "${T}", "using": using}}]}),
        json!({"is": {}, "_behaviors": {"lookup": {
            "key": {"from": {}, "using": using},
            "fromDataSource": {"csv": {"path": "x.csv", "keyColumn": "id"}},
            "into": "${R}"}}}),
    ];
    for response in responses {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = json!({"port": 4610, "protocol": "http", "stubs": [{"responses": [response]}]});
        std::fs::write(dir.path().join("4610.json"), file.to_string()).expect("write datadir file");
        let loaded = load_configs(&ConfigSource::Dir(dir.path().to_path_buf()))
            .unwrap_or_else(|e| panic!("{response} must load from a datadir: {e:#}"));
        assert_eq!(loaded.len(), 1, "{response}");
        assert!(
            serde_json::from_value::<rift_http_proxy::imposter::ImposterConfig>(file).is_err(),
            "{response} is refused at a door"
        );
    }
}

/// A datadir file is still held to the checks every door has applied since the replay floor.
#[test]
fn a_datadir_file_that_fails_an_admission_check_is_still_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = json!({"port": 4611, "protocol": "http",
                      "stubs": [{"predicates": [{"matches": {"path": "(unclosed"}}],
                                 "responses": [{"is": {}}]}]});
    std::fs::write(dir.path().join("4611.json"), file.to_string()).expect("write datadir file");
    let err = load_configs(&ConfigSource::Dir(dir.path().to_path_buf()))
        .expect_err("an invalid regex in a datadir file fails the load");
    assert!(format!("{err:#}").contains("invalid regex"), "{err:#}");
}
