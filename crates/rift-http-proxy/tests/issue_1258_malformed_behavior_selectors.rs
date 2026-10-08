//! Issue #1258: a `copy`/`lookup` behavior whose `using` selector does not compile was accepted at
//! load and then extracted nothing on every request, so the token was replaced with an empty string
//! (or the lookup found no row) with nothing to say why. Predicates have refused such selectors since
//! #1220 and `matches` regexes since #1221; these tests hold the behavior doors to the same rule, for
//! regex, JSONPath and XPath alike.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

const BAD_JSONPATH: &str = "$[[[bad";
const BAD_XPATH: &str = "//*[[";
const BAD_REGEX: &str = "(unclosed";

fn copy_block(method: &str, selector: &str) -> serde_json::Value {
    json!({ "copy": { "from": "body", "into": "${TOKEN}", "using": { "method": method, "selector": selector } } })
}

fn lookup_block(method: &str, selector: &str) -> serde_json::Value {
    json!({ "lookup": {
        "key": { "from": "body", "using": { "method": method, "selector": selector } },
        "fromDataSource": { "csv": { "path": "/nonexistent.csv", "keyColumn": "id" } },
        "into": "${row}"
    } })
}

/// An object block goes under `_behaviors`; the array (program) form is spelled `behaviors`.
fn is_stub(behaviors: serde_json::Value) -> serde_json::Value {
    let key = if behaviors.is_array() {
        "behaviors"
    } else {
        "_behaviors"
    };
    json!({ "responses": [{ "is": { "statusCode": 200, "body": "${TOKEN}" }, key: behaviors }] })
}

fn proxy_stub(behaviors: serde_json::Value) -> serde_json::Value {
    json!({ "responses": [{ "proxy": { "to": "http://127.0.0.1:9" }, "behaviors": behaviors }] })
}

/// Every malformed shape: (stub, behavior key and method its refusal must name, selector).
fn bad_stubs() -> Vec<(serde_json::Value, &'static str, &'static str)> {
    vec![
        (
            is_stub(copy_block("jsonpath", BAD_JSONPATH)),
            "`copy` behavior `jsonpath`",
            BAD_JSONPATH,
        ),
        (
            is_stub(copy_block("xpath", BAD_XPATH)),
            "`copy` behavior `xpath`",
            BAD_XPATH,
        ),
        (
            is_stub(copy_block("regex", BAD_REGEX)),
            "`copy` behavior `regex`",
            BAD_REGEX,
        ),
        (
            is_stub(lookup_block("jsonpath", BAD_JSONPATH)),
            "`lookup` behavior `jsonpath`",
            BAD_JSONPATH,
        ),
        (
            is_stub(lookup_block("regex", BAD_REGEX)),
            "`lookup` behavior `regex`",
            BAD_REGEX,
        ),
        (
            proxy_stub(copy_block("xpath", BAD_XPATH)),
            "`copy` behavior `xpath`",
            BAD_XPATH,
        ),
        (
            is_stub(json!([{ "wait": 1 }, copy_block("jsonpath", BAD_JSONPATH)])),
            "`copy` behavior `jsonpath`",
            BAD_JSONPATH,
        ),
    ]
}

async fn admin() -> (rift_http_proxy::admin_api::RunningAdminApi, String) {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());
    (running, base)
}

async fn assert_refused(response: reqwest::Response, names: &str, selector: &str, door: &str) {
    assert_eq!(
        response.status(),
        400,
        "{door} must refuse a behavior selector that cannot compile"
    );
    let body = response.text().await.expect("body");
    assert!(
        body.contains("bad data"),
        "{door}: bad data envelope: {body}"
    );
    assert!(
        body.contains(names),
        "{door}: the refusal names {names}: {body}"
    );
    assert!(
        body.contains(selector),
        "{door}: the refusal names `{selector}`: {body}"
    );
}

#[tokio::test]
async fn post_imposters_refuses_a_malformed_behavior_selector() {
    for (stub, names, selector) in bad_stubs() {
        let (running, base) = admin().await;
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}/imposters"))
            .json(&json!({ "protocol": "http", "stubs": [stub] }))
            .send()
            .await
            .expect("POST /imposters");
        assert_refused(response, names, selector, "POST /imposters").await;
        let listing: serde_json::Value = client
            .get(format!("{base}/imposters"))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json");
        assert_eq!(listing["imposters"].as_array().map(Vec::len), Some(0));
        running.shutdown().await;
    }
}

#[tokio::test]
async fn stub_doors_refuse_a_malformed_behavior_selector() {
    for (stub, names, selector) in bad_stubs() {
        let (running, base) = admin().await;
        let client = reqwest::Client::new();
        let created = client
            .post(format!("{base}/imposters"))
            .json(&json!({ "port": 0, "protocol": "http", "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "ORIGINAL" } }] }] }))
            .send()
            .await
            .expect("POST /imposters");
        assert_eq!(created.status(), 201);
        let port = created.json::<serde_json::Value>().await.expect("json")["port"]
            .as_u64()
            .expect("port");

        let response = client
            .post(format!("{base}/imposters/{port}/stubs"))
            .json(&json!({ "stub": stub.clone() }))
            .send()
            .await
            .expect("POST stubs");
        assert_refused(response, names, selector, "POST /imposters/:port/stubs").await;
        let response = client
            .put(format!("{base}/imposters/{port}/stubs/0"))
            .json(&stub)
            .send()
            .await
            .expect("PUT stub");
        assert_refused(response, names, selector, "PUT /imposters/:port/stubs/0").await;
        let response = client
            .put(format!("{base}/imposters"))
            .json(&json!({ "imposters": [{ "protocol": "http", "stubs": [stub] }] }))
            .send()
            .await
            .expect("PUT /imposters");
        assert_refused(response, names, selector, "PUT /imposters").await;

        // Every refusal left the running imposter as it was.
        let imposter: serde_json::Value = client
            .get(format!("{base}/imposters/{port}"))
            .send()
            .await
            .expect("GET imposter")
            .json()
            .await
            .expect("json");
        let stubs = imposter["stubs"].as_array().expect("stubs");
        assert_eq!(stubs.len(), 1, "{imposter}");
        assert_eq!(stubs[0]["responses"][0]["is"]["body"], "ORIGINAL");
        running.shutdown().await;
    }
}

#[test]
fn a_configfile_with_a_malformed_behavior_selector_fails_to_load() {
    for (stub, _, selector) in bad_stubs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        let config = json!({ "imposters": [{ "port": 0, "protocol": "http", "stubs": [stub] }] });
        std::fs::write(&path, config.to_string()).expect("write");
        let err = load_configs(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .expect_err("a malformed behavior selector must fail the load");
        assert!(format!("{err:#}").contains(selector), "{err:#}");
    }
}

/// A step a later `null` removes configures nothing, so its selector is not checked (#1162 rule).
#[test]
fn a_removed_copy_step_is_not_checked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let config = json!({ "imposters": [{ "port": 0, "protocol": "http", "stubs": [is_stub(json!([
        copy_block("jsonpath", BAD_JSONPATH),
        { "copy": null }
    ]))] }] });
    std::fs::write(&path, config.to_string()).expect("write");
    load_configs(&ConfigSource::File {
        path,
        no_parse: false,
    })
    .expect("a removed step is not part of the program the engine runs");
}

#[tokio::test]
async fn well_formed_behavior_selectors_still_load() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let ignore_case = json!({ "copy": { "from": "path", "into": "${ID}",
        "using": { "method": "regex", "selector": "/catalog/(\\d+)", "options": { "ignoreCase": true } } } });
    let response = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "protocol": "http", "stubs": [
            is_stub(copy_block("jsonpath", "$.user.name")),
            is_stub(copy_block("jsonpath", "user.name")),
            is_stub(copy_block("xpath", "//user/name/text()")),
            is_stub(ignore_case),
            is_stub(lookup_block("regex", "/(.*)")),
        ] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(
        response.status(),
        201,
        "{}",
        response.text().await.unwrap_or_default()
    );
    running.shutdown().await;
}

/// Issue #1326: an XPath `using` may carry Mountebank's `ns` map, on `copy` and on `lookup`.
#[tokio::test]
async fn xpath_selectors_with_an_ns_map_load() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let ns = json!({ "mb": "http://example.com/mb" });
    let mut copy = copy_block("xpath", "//mb:name");
    copy["copy"]["using"]["ns"] = ns.clone();
    let mut lookup = lookup_block("xpath", "//mb:id");
    lookup["lookup"]["key"]["using"]["ns"] = ns;
    let response = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "protocol": "http", "stubs": [is_stub(copy), is_stub(lookup)] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(
        response.status(),
        201,
        "{}",
        response.text().await.unwrap_or_default()
    );
    running.shutdown().await;
}

/// Issue #1326: an `ns` that is not a prefix→URI object, or one on a method that has no
/// namespaces, is refused at the door rather than dropped.
#[tokio::test]
async fn a_malformed_or_misplaced_ns_map_is_refused() {
    let mut string_ns = copy_block("xpath", "//mb:name");
    string_ns["copy"]["using"]["ns"] = json!("http://example.com/mb");
    let mut number_uri = lookup_block("xpath", "//mb:id");
    number_uri["lookup"]["key"]["using"]["ns"] = json!({ "mb": 1 });
    let mut on_regex = copy_block("regex", "(.*)");
    on_regex["copy"]["using"]["ns"] = json!({ "mb": "http://example.com/mb" });
    let cases = [
        (
            is_stub(string_ns),
            "`copy` behavior `xpath` `using.ns` must be an object mapping each prefix to a namespace URI string",
        ),
        (
            is_stub(number_uri),
            "`lookup` behavior `xpath` `using.ns` must be an object mapping each prefix to a namespace URI string",
        ),
        (
            proxy_stub(on_regex),
            "`copy` behavior `regex` has `using.ns`, which only applies to `method: xpath`",
        ),
    ];
    for (stub, refusal) in cases {
        let (running, base) = admin().await;
        let response = reqwest::Client::new()
            .post(format!("{base}/imposters"))
            .json(&json!({ "protocol": "http", "stubs": [stub] }))
            .send()
            .await
            .expect("POST /imposters");
        assert_eq!(response.status(), 400, "{refusal}");
        let body = response.text().await.expect("body");
        assert!(body.contains(refusal), "{refusal}: {body}");
        running.shutdown().await;
    }
}
