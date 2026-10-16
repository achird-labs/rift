//! Issue #1478: with `caseSensitive` false (the default), Mountebank lowercases the body and the
//! XPath selector before it selects, so `//b` finds `<B>` and `//a/@id` finds `Id="7"`. Rift
//! selected on the raw body with the raw selector. Rift now folds element names, attribute names
//! and prefixes, values and text the same way for every operator but `matches`, keeping only the
//! CDATA and DOCTYPE markers as written so those bodies stay readable.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

/// Status and body served for each request body by an imposter whose only stub is `stub`; the
/// default response (no stub matched) is a 200 with no body, so the stubs answer 202.
async fn served(stub: serde_json::Value, bodies: &[&str]) -> Vec<(u16, String)> {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "port": 0, "protocol": "http", "stubs": [stub] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(
        created.status(),
        201,
        "{}",
        created.text().await.unwrap_or_default()
    );
    let port = created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");
    let mut out = Vec::new();
    for body in bodies {
        let response = client
            .post(format!("http://127.0.0.1:{port}/"))
            .body(body.to_string())
            .send()
            .await
            .expect("request to the imposter");
        let status = response.status().as_u16();
        out.push((status, response.text().await.expect("body")));
    }
    running.shutdown().await;
    out
}

async fn statuses(predicate: serde_json::Value, bodies: &[&str]) -> Vec<u16> {
    let stub = json!({ "predicates": [predicate], "responses": [{ "is": { "statusCode": 202 } }] });
    served(stub, bodies)
        .await
        .into_iter()
        .map(|(status, _)| status)
        .collect()
}

const UPPER: &str = "<A><B>1</B></A>";
const CDATA: &str = "<a><![CDATA[xy]]></a>";
const DOCTYPE: &str = "<!DOCTYPE a><a><b>1</b></a>";

#[tokio::test]
async fn a_lowercase_selector_finds_uppercase_elements_by_default() {
    let mut got = Vec::new();
    for predicate in [
        json!({ "equals": { "body": "1" }, "xpath": { "selector": "//b" } }),
        json!({ "exists": { "body": true }, "xpath": { "selector": "//b" } }),
        json!({ "contains": { "body": "1" }, "xpath": { "selector": "//b" } }),
        json!({ "deepEquals": { "body": "1" }, "xpath": { "selector": "//b" } }),
        json!({ "startsWith": { "body": "1" }, "xpath": { "selector": "//b" } }),
        json!({ "endsWith": { "body": "1" }, "xpath": { "selector": "//b" } }),
        json!({ "equals": { "body": "1" }, "xpath": { "selector": "//B" } }),
    ] {
        got.extend(statuses(predicate, &[UPPER]).await);
    }
    assert_eq!(got, vec![202; 7]);
}

#[tokio::test]
async fn a_lowercase_selector_finds_an_uppercase_attribute_name() {
    let got = statuses(
        json!({ "equals": { "body": "7" }, "xpath": { "selector": "//a/@id" } }),
        &["<a Id=\"7\"/>"],
    )
    .await;
    assert_eq!(got, vec![202]);
}

#[tokio::test]
async fn matches_stays_case_sensitive_on_names() {
    let got = statuses(
        json!({ "matches": { "body": "1" }, "xpath": { "selector": "//b" } }),
        &[UPPER],
    )
    .await;
    assert_eq!(got, vec![200]);
}

#[tokio::test]
async fn case_sensitive_keeps_names_exact() {
    let lower = statuses(
        json!({ "exists": { "body": true }, "caseSensitive": true, "xpath": { "selector": "//b" } }),
        &[UPPER],
    )
    .await;
    let upper = statuses(
        json!({ "exists": { "body": true }, "caseSensitive": true, "xpath": { "selector": "//B" } }),
        &[UPPER],
    )
    .await;
    assert_eq!((lower, upper), (vec![200], vec![202]));
}

#[tokio::test]
async fn a_prefixed_selector_finds_its_namespace_by_default() {
    let got = statuses(
        json!({ "equals": { "body": "1" },
                "xpath": { "selector": "//x:B", "ns": { "x": "urn:X" } } }),
        &["<x:A xmlns:x=\"urn:X\"><x:B>1</x:B></x:A>"],
    )
    .await;
    assert_eq!(got, vec![202]);
}

/// The documented residue: Mountebank's lowercased `<![cdata[` and `<!doctype` make xmldom refuse
/// the body, so it never matches; Rift keeps both readable.
#[tokio::test]
async fn cdata_and_doctype_bodies_still_match_by_default() {
    let got = statuses(
        json!({ "exists": { "body": true }, "xpath": { "selector": "//a" } }),
        &[CDATA, DOCTYPE],
    )
    .await;
    let equals = statuses(
        json!({ "equals": { "body": "xy" }, "xpath": { "selector": "//a" } }),
        &[CDATA],
    )
    .await;
    assert_eq!((got, equals), (vec![202, 202], vec![202]));
}

#[tokio::test]
async fn a_copy_selects_on_the_raw_body() {
    let stub = json!({
        "responses": [{
            "is": { "statusCode": 202, "body": "got ${b}" },
            "_behaviors": { "copy": [{
                "from": "body", "into": "${b}",
                "using": { "method": "xpath", "selector": "//b" }
            }] }
        }]
    });
    let got = served(stub, &[UPPER]).await;
    assert_eq!(got, vec![(202, "got ${b}".to_string())]);
}

const SOAP: &str = "<soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                    <soap:Body><GetPrice>1</GetPrice></soap:Body></soap:Envelope>";

/// Name functions compare against literals, so the literals fold with the names, as in
/// Mountebank: the SOAP no-namespace idiom keeps matching by default.
#[tokio::test]
async fn name_functions_match_their_literals_by_default() {
    let mut got = Vec::new();
    for selector in [
        "//*[local-name()='Envelope']/*[local-name()='Body']",
        "//*[name()='soap:Body']",
        "//*[starts-with(local-name(),'Get')]",
    ] {
        got.extend(
            statuses(
                json!({ "exists": { "body": true }, "xpath": { "selector": selector } }),
                &[SOAP],
            )
            .await,
        );
    }
    assert_eq!(got, vec![202; 3]);
}

#[tokio::test]
async fn namespace_uri_matches_a_capitalised_declaration_by_default() {
    let got = statuses(
        json!({ "exists": { "body": true },
                "xpath": { "selector": "//*[namespace-uri()='urn:Example']" } }),
        &["<x:a xmlns:x=\"urn:EXAMPLE\">1</x:a>"],
    )
    .await;
    assert_eq!(got, vec![202]);
}

#[tokio::test]
async fn a_text_literal_matches_regardless_of_case_by_default() {
    let got = statuses(
        json!({ "exists": { "body": true }, "xpath": { "selector": "//b[text()='x']" } }),
        &["<a><b>X</b></a>"],
    )
    .await;
    assert_eq!(got, vec![202]);
}

/// Rift lowercases the `ns` prefixes as well as the URIs (Mountebank only the URIs), so a stub
/// binding an upper-case prefix keeps matching once its selector is folded.
#[tokio::test]
async fn an_upper_case_ns_prefix_still_binds_by_default() {
    let got = statuses(
        json!({ "exists": { "body": true },
                "xpath": { "selector": "//SOAP:Body",
                           "ns": { "SOAP": "http://schemas.xmlsoap.org/soap/envelope/" } } }),
        &[SOAP],
    )
    .await;
    assert_eq!(got, vec![202]);
}
