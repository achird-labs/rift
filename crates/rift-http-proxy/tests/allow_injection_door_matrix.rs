//! Issue #1492 (ported from rift-conformance's AllowInjectionMatrix116 spec): the `--allowInjection`
//! door × payload matrix. The invariant under test is the one from the #612 triage: **the same
//! document gets the same security answer through every door.** The gate has been re-opened at a
//! new door four times (#610, #612, #616, #657); the per-door tests elsewhere each send one payload,
//! so this loops every payload kind through every admin door that accepts a stub.
//!
//! Every refusal is paired with the same request on a gate-on server being admitted, so a refusal
//! is proven to come from the gate rather than from a schema rejection that would refuse the
//! request either way.

use clap::Parser;
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use serde_json::{Value, json};

const INJECT: &str = "function (config) { return { statusCode: 200, body: 'x' }; }";
const INJECT_PRED: &str = "function (config) { return true; }";

/// Every scripted payload kind, as a full stub body.
fn scripted_stubs() -> Vec<(&'static str, Value)> {
    vec![
        (
            "inject response",
            json!({ "responses": [{ "inject": INJECT }] }),
        ),
        (
            "inject predicate",
            json!({ "predicates": [{ "inject": INJECT_PRED }],
                    "responses": [{ "is": { "statusCode": 200 } }] }),
        ),
        // #657: the classifier must see through `not`/`or` nesting.
        (
            "nested not/or inject predicate",
            json!({ "predicates": [{ "or": [{ "equals": { "path": "/a" } },
                                            { "not": { "inject": INJECT_PRED } }] }],
                    "responses": [{ "is": { "statusCode": 200 } }] }),
        ),
        (
            "_behaviors.decorate",
            json!({ "responses": [{ "is": { "statusCode": 200 },
                    "_behaviors": { "decorate": "function (r, s) { s.body = 'x'; }" } }] }),
        ),
        (
            "_behaviors.shellTransform",
            json!({ "responses": [{ "is": { "statusCode": 200 },
                    "_behaviors": { "shellTransform": "/bin/cat" } }] }),
        ),
        (
            "function wait",
            json!({ "responses": [{ "is": { "statusCode": 200 },
                    "_behaviors": { "wait": "function () { return 1; }" } }] }),
        ),
        (
            "_rift.script",
            json!({ "responses": [{ "_rift": { "script": {
                    "engine": "rhai", "code": "fn respond(ctx) { http(200, #{}) }" } } }] }),
        ),
        (
            "predicateGenerators.inject",
            json!({ "responses": [{ "proxy": { "to": "http://127.0.0.1:1", "mode": "proxyOnce",
                    "predicateGenerators": [{ "inject": INJECT_PRED }] } }] }),
        ),
    ]
}

/// The clean stub every target imposter starts with.
fn clean_stub() -> Value {
    json!({ "id": "clean", "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] })
}

fn cli(gate_on: bool, extra: &[&str]) -> Cli {
    let mut argv = vec!["rift", "--host", "127.0.0.1", "--port", "0"];
    if gate_on {
        argv.push("--allow-injection");
    }
    argv.extend_from_slice(extra);
    Cli::try_parse_from(argv).expect("cli parse")
}

async fn start(gate_on: bool, extra: &[&str]) -> (RunningServer, String) {
    let server = ServerBuilder::from_cli(cli(gate_on, extra))
        .start()
        .await
        .expect("server starts");
    let admin = format!("http://{}", server.admin_addr());
    (server, admin)
}

async fn send(method: reqwest::Method, url: &str, body: &Value) -> (u16, Value) {
    let mut req = reqwest::Client::new().request(method.clone(), url);
    if method != reqwest::Method::GET {
        req = req.json(body);
    }
    let resp = req.send().await.expect("admin request");
    let status = resp.status().as_u16();
    let text = resp.text().await.expect("body");
    let parsed = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, parsed)
}

/// A fresh port-less imposter holding only [`clean_stub`], with a flow-state store so the space
/// door is reachable; returns its port.
async fn clean_imposter(admin: &str) -> u16 {
    let (status, body) = send(
        reqwest::Method::POST,
        &format!("{admin}/imposters"),
        &json!({ "protocol": "http", "_rift": { "flowState": {} }, "stubs": [clean_stub()] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    u16::try_from(body["port"].as_u64().expect("port")).expect("u16 port")
}

async fn stub_ids(admin: &str, port: u16) -> Vec<String> {
    let (status, body) = send(
        reqwest::Method::GET,
        &format!("{admin}/imposters/{port}"),
        &json!({}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body["stubs"]
        .as_array()
        .expect("stubs")
        .iter()
        .map(|s| s["id"].as_str().unwrap_or("<no id>").to_owned())
        .collect()
}

/// Every admin door that takes a stub, as (label, method, path under the admin URL, body).
/// `{port}` is substituted with a fresh clean imposter's port.
fn doors(stub: &Value) -> Vec<(&'static str, reqwest::Method, &'static str, Value)> {
    use reqwest::Method;
    vec![
        (
            "POST /imposters",
            Method::POST,
            "/imposters",
            json!({ "protocol": "http", "stubs": [stub] }),
        ),
        (
            "PUT /imposters",
            Method::PUT,
            "/imposters",
            json!({ "imposters": [{ "protocol": "http", "stubs": [stub] }] }),
        ),
        (
            "POST /imposters/{port}/stubs",
            Method::POST,
            "/imposters/{port}/stubs",
            json!({ "stub": stub }),
        ),
        (
            "PUT /imposters/{port}/stubs",
            Method::PUT,
            "/imposters/{port}/stubs",
            json!({ "stubs": [stub] }),
        ),
        (
            "PUT /imposters/{port}/stubs/0",
            Method::PUT,
            "/imposters/{port}/stubs/0",
            stub.clone(),
        ),
        (
            "PUT /imposters/{port}/stubs/by-id/clean",
            Method::PUT,
            "/imposters/{port}/stubs/by-id/clean",
            stub.clone(),
        ),
        (
            "POST /imposters/{port}/spaces/s1/stubs",
            Method::POST,
            "/imposters/{port}/spaces/s1/stubs",
            stub.clone(),
        ),
    ]
}

/// The admin error envelope's `type`: `invalid injection` for the gate, `bad data` for a schema
/// refusal (whose `code` is the status).
fn error_type(body: &Value) -> &str {
    body["errors"][0]["type"]
        .as_str()
        .unwrap_or("<no error type>")
}

/// Gate off: every payload through every door is a `400 invalid injection`, and the target
/// imposter's stubs are untouched.
#[tokio::test]
async fn every_scripted_payload_is_refused_at_every_admin_door_without_the_flag() {
    let (server, admin) = start(false, &[]).await;
    let mut wrong = Vec::new();
    let mut checked = 0;
    for (payload, stub) in scripted_stubs() {
        for (door, method, path, body) in doors(&stub) {
            let port = clean_imposter(&admin).await;
            let url = format!("{admin}{}", path.replace("{port}", &port.to_string()));
            let (status, resp) = send(method, &url, &body).await;
            if status != 400 || error_type(&resp) != "invalid injection" {
                wrong.push(format!("{payload} via {door}: {status} {resp}"));
            }
            let ids = stub_ids(&admin, port).await;
            if ids != ["clean"] {
                wrong.push(format!(
                    "{payload} via {door} mutated the imposter: {ids:?}"
                ));
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 8 * 7);
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    server.shutdown().await;
}

/// Gate on: the same requests are admitted, so the refusals above are the gate's.
#[tokio::test]
async fn every_scripted_payload_is_admitted_at_every_admin_door_with_the_flag() {
    let (server, admin) = start(true, &[]).await;
    let mut wrong = Vec::new();
    for (payload, stub) in scripted_stubs() {
        for (door, method, path, body) in doors(&stub) {
            let port = clean_imposter(&admin).await;
            let url = format!("{admin}{}", path.replace("{port}", &port.to_string()));
            let (status, resp) = send(method, &url, &body).await;
            if !(200..300).contains(&status) {
                wrong.push(format!("{payload} via {door}: {status} {resp}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    server.shutdown().await;
}

/// #610: an unparseable `_behaviors` must never be read as "no script surface here". Since 0.18.0
/// it is refused at the schema layer before the gate runs, so the answer is the same `bad data` at
/// every door whether the gate is on or off — fail-closed either way.
#[tokio::test]
async fn an_unparseable_behaviors_block_is_refused_at_every_door_either_way() {
    let stub = json!({ "responses": [{ "is": { "statusCode": 200 },
                       "_behaviors": { "decorate": 12345 } }] });
    for gate_on in [false, true] {
        let (server, admin) = start(gate_on, &[]).await;
        let mut wrong = Vec::new();
        for (door, method, path, body) in doors(&stub) {
            let port = clean_imposter(&admin).await;
            let url = format!("{admin}{}", path.replace("{port}", &port.to_string()));
            let (status, resp) = send(method, &url, &body).await;
            if status != 400 || error_type(&resp) != "bad data" {
                wrong.push(format!("gate on={gate_on} via {door}: {status} {resp}"));
            }
            let ids = stub_ids(&admin, port).await;
            if ids != ["clean"] {
                wrong.push(format!("gate on={gate_on} via {door} mutated: {ids:?}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
        server.shutdown().await;
    }
}

/// `POST /imposters/{port}/verify` takes predicates only: both inject-predicate shapes are refused
/// without the flag and admitted with it.
#[tokio::test]
async fn inject_verify_predicates_are_gated_like_stub_predicates() {
    for gate_on in [false, true] {
        let (server, admin) = start(gate_on, &[]).await;
        for (payload, stub) in scripted_stubs() {
            let Some(predicates) = stub.get("predicates") else {
                continue;
            };
            let port = clean_imposter(&admin).await;
            let (status, resp) = send(
                reqwest::Method::POST,
                &format!("{admin}/imposters/{port}/verify"),
                &json!({ "predicates": predicates }),
            )
            .await;
            if gate_on {
                assert_eq!(status, 200, "{payload} gate on: {resp}");
            } else {
                assert_eq!(
                    (status, error_type(&resp)),
                    (400, "invalid injection"),
                    "{payload} gate off: {resp}"
                );
            }
        }
        server.shutdown().await;
    }
}

/// `POST /admin/reload` of a config file edited to carry each payload: refused without the flag,
/// leaving the running imposter serving, and applied with it.
#[tokio::test]
async fn every_scripted_payload_is_gated_at_the_reload_door() {
    for gate_on in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        let clean = json!({ "imposters": [{ "protocol": "http", "stubs": [clean_stub()] }] });
        std::fs::write(&path, clean.to_string()).expect("write");
        let (server, admin) = start(gate_on, &["--configfile", path.to_str().unwrap()]).await;
        for (payload, stub) in scripted_stubs() {
            std::fs::write(&path, clean.to_string()).expect("write");
            let (status, resp) = send(
                reqwest::Method::POST,
                &format!("{admin}/admin/reload"),
                &json!({}),
            )
            .await;
            assert_eq!(status, 200, "clean reload before {payload}: {resp}");

            let poisoned = json!({ "imposters": [{ "protocol": "http", "stubs": [stub] }] });
            std::fs::write(&path, poisoned.to_string()).expect("write");
            let (status, resp) = send(
                reqwest::Method::POST,
                &format!("{admin}/admin/reload"),
                &json!({}),
            )
            .await;
            if gate_on {
                assert_eq!(status, 200, "{payload} gate on: {resp}");
            } else {
                assert_eq!(
                    (status, error_type(&resp)),
                    (400, "invalid injection"),
                    "{payload} gate off: {resp}"
                );
                let (_, listed) = send(
                    reqwest::Method::GET,
                    &format!("{admin}/imposters"),
                    &json!({}),
                )
                .await;
                let port = listed["imposters"][0]["port"].as_u64().expect("port");
                let u16_port = u16::try_from(port).expect("u16 port");
                assert_eq!(
                    stub_ids(&admin, u16_port).await,
                    ["clean"],
                    "{payload}: a refused reload must change nothing"
                );
            }
        }
        server.shutdown().await;
    }
}
