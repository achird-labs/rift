//! Issue #1585: an XPath selector that compiles but cannot be evaluated — a prefix its `ns` map
//! does not bind, an unknown function or variable, a function given the wrong argument — fails the
//! request with Mountebank's `500 malformed xpath predicate selector`. Rift read the failure as
//! "nothing selected", so `equals {body: ""}`, `exists {body: false}` and `not` behind such a
//! selector matched every request (the open half of #181).
//!
//! Every expectation here was observed against Mountebank 2.9.5.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use std::sync::Arc;

const PLAIN: &str = "<a><b>1</b></a>";

/// The response Mountebank and Rift give a request that reaches a stub whose XPath fails.
struct Served {
    status: u16,
    body: String,
}

/// Creates an imposter with `stubs` and a catch-all `fallback` stub after them, sends `body` to
/// `path`, and returns what the imposter answered.
async fn serve(stubs: Vec<Value>, path: &str, body: &str) -> Served {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    // The lookup's CSV is a temp file; an admin-API lookup reads only under `--scripts-dir`.
    .with_scripts_dir(std::env::temp_dir())
    .bind()
    .await
    .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());
    let mut all = stubs;
    all.push(json!({ "responses": [{ "is": { "body": "fallback" } }] }));
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "port": 0, "protocol": "http", "stubs": all }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(
        created.status(),
        201,
        "{}",
        created.text().await.unwrap_or_default()
    );
    let port = created.json::<Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");
    let response = client
        .post(format!("http://127.0.0.1:{port}{path}"))
        .header("content-type", "application/xml")
        .body(body.to_string())
        .send()
        .await
        .expect("request to the imposter");
    let served = Served {
        status: response.status().as_u16(),
        body: response.text().await.expect("body"),
    };
    running.shutdown().await;
    served
}

fn hit(predicate: Value) -> Value {
    json!({ "predicates": [predicate], "responses": [{ "is": { "body": "hit" } }] })
}

/// Mountebank's envelope, without the `source` (the selector), `inner`, `name` and `stack` Rift
/// never sends (`docs/mountebank/differences.md`, "Error envelopes").
fn assert_malformed_xpath(served: &Served, selector: &str) {
    assert_eq!(served.status, 500, "{selector}: {}", served.body);
    let body: Value = serde_json::from_str(&served.body).expect("a JSON error body");
    assert_eq!(
        body,
        json!({ "errors": [{
            "code": "bad data",
            "type": "bad data",
            "message": "malformed xpath predicate selector",
        }] }),
        "{selector}"
    );
}

const FAILING: [&str; 5] = [
    // A prefix the `ns` map does not bind, even one the document declares (Mountebank resolves
    // prefixes from its `ns` map only).
    "//x:a",
    "nosuch(//a)",
    "$v",
    "count('a')",
    "p:f(//a)",
];

#[tokio::test]
async fn a_selector_that_cannot_be_evaluated_fails_every_predicate_operator() {
    for selector in FAILING {
        let xpath = json!({ "selector": selector });
        for predicate in [
            json!({ "equals": { "body": "1" }, "xpath": xpath }),
            json!({ "equals": { "body": "" }, "xpath": xpath }),
            json!({ "deepEquals": { "body": "" }, "xpath": xpath }),
            json!({ "exists": { "body": false }, "xpath": xpath }),
            json!({ "matches": { "body": "^$" }, "xpath": xpath }),
            json!({ "not": { "equals": { "body": "1" }, "xpath": xpath } }),
            // The selector runs whenever the body is XML, whatever field the predicate reads.
            json!({ "equals": { "path": "/x" }, "xpath": xpath }),
            json!({ "equals": { "body": "1" }, "caseSensitive": false, "xpath": xpath }),
        ] {
            let served = serve(vec![hit(predicate.clone())], "/x", PLAIN).await;
            assert_malformed_xpath(&served, selector);
        }
    }
}

#[tokio::test]
async fn a_prefix_the_document_declares_still_needs_the_ns_map() {
    let doc = r#"<r xmlns:x="urn:x"><x:a>1</x:a></r>"#;
    let unbound = json!({ "equals": { "body": "1" }, "xpath": { "selector": "//x:a" } });
    assert_malformed_xpath(&serve(vec![hit(unbound)], "/x", doc).await, "//x:a");

    let bound = json!({ "equals": { "body": "1" },
                        "xpath": { "selector": "//x:a", "ns": { "x": "urn:x" } } });
    assert_eq!(serve(vec![hit(bound)], "/x", doc).await.body, "hit");
}

#[tokio::test]
async fn a_body_that_is_not_xml_never_runs_the_selector() {
    for body in ["not xml", ""] {
        let predicate = json!({ "equals": { "body": "" }, "xpath": { "selector": "//x:a" } });
        let served = serve(vec![hit(predicate)], "/x", body).await;
        assert_eq!((served.status, served.body.as_str()), (200, "hit"));
    }
}

#[tokio::test]
async fn a_prefix_inside_a_string_literal_is_not_a_prefix() {
    let predicate = json!({ "equals": { "body": "1" }, "xpath": { "selector": "//b[.!='p:x']" } });
    let served = serve(vec![hit(predicate)], "/x", PLAIN).await;
    assert_eq!((served.status, served.body.as_str()), (200, "hit"));
}

#[tokio::test]
async fn an_earlier_stub_that_matches_is_served() {
    let first = json!({ "predicates": [{ "equals": { "path": "/y" } }],
                        "responses": [{ "is": { "body": "y" } }] });
    let failing = hit(json!({ "equals": { "body": "1" }, "xpath": { "selector": "//x:a" } }));
    let served = serve(vec![first.clone(), failing.clone()], "/y", PLAIN).await;
    assert_eq!((served.status, served.body.as_str()), (200, "y"));
    assert_malformed_xpath(&serve(vec![first, failing], "/z", PLAIN).await, "//x:a");
}

#[tokio::test]
async fn an_or_that_already_matched_does_not_run_the_selector() {
    let predicate = json!({ "or": [
        { "equals": { "path": "/x" } },
        { "equals": { "body": "1" }, "xpath": { "selector": "//x:a" } },
    ] });
    let served = serve(vec![hit(predicate)], "/x", PLAIN).await;
    assert_eq!((served.status, served.body.as_str()), (200, "hit"));
}

fn copy(selector: &str, into: &str) -> Value {
    json!({ "responses": [{
        "is": { "body": "v=${V}" },
        "behaviors": [{ "copy": { "from": "body", "into": into,
                                  "using": { "method": "xpath", "selector": selector } } }]
    }] })
}

#[tokio::test]
async fn a_copy_whose_selector_cannot_be_evaluated_fails_the_request() {
    for selector in ["//x:a", "nosuch(//a)"] {
        assert_malformed_xpath(
            &serve(vec![copy(selector, "${V}")], "/x", PLAIN).await,
            selector,
        );
        // Mountebank runs the selector whether or not the response spells the token.
        assert_malformed_xpath(
            &serve(vec![copy(selector, "${Z}")], "/x", PLAIN).await,
            selector,
        );
    }
    let served = serve(vec![copy("//x:a", "${V}")], "/x", "not xml").await;
    assert_eq!((served.status, served.body.as_str()), (200, "v=${V}"));
    let served = serve(vec![copy("//b", "${V}")], "/x", PLAIN).await;
    assert_eq!((served.status, served.body.as_str()), (200, "v=1"));
}

/// Mountebank's `lookup` catches the failure and logs it, leaving the token as authored. The CSV
/// has a row keyed by the empty string, which a failure read as `""` would serve.
#[tokio::test]
async fn a_lookup_whose_key_selector_cannot_be_evaluated_leaves_the_token() {
    let csv = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile_in(std::env::temp_dir())
        .expect("temp csv");
    std::fs::write(csv.path(), "id,name\n,empty\n1,alice\n").expect("write csv");
    let lookup = |selector: &str| {
        json!({ "responses": [{
            "is": { "body": "n=${R}[name]" },
            "behaviors": [{ "lookup": {
                "key": { "from": "body", "using": { "method": "xpath", "selector": selector } },
                "fromDataSource": { "csv": { "path": csv.path(), "keyColumn": "id" } },
                "into": "${R}"
            } }]
        }] })
    };
    let served = serve(vec![lookup("//x:a")], "/x", PLAIN).await;
    assert_eq!((served.status, served.body.as_str()), (200, "n=${R}[name]"));
    let served = serve(vec![lookup("//b")], "/x", PLAIN).await;
    assert_eq!((served.status, served.body.as_str()), (200, "n=alice"));
}
