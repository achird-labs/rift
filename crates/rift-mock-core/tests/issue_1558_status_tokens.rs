//! Issue #1558: an imposter's `statusCode` may be any string. Mountebank stores it as written, lets
//! `copy` and `lookup` substitute tokens into it (`behaviors.js` `globalObjectReplace`), and reads it
//! only when it writes the response, with Node's `writeHead` rule: `Number(value) | 0`, and a value
//! outside 100–999 answers `500` `ERR_HTTP_INVALID_STATUS_CODE` (`baseHttpServer.js:143-155`). A
//! falsy value (`""`, `0`) falls back to `defaultResponse.statusCode`, then `200` (`:25`).

use rift_mock_core::imposter::ImposterManager;
use serde_json::{Value, json};
use std::time::Duration;

/// Creates the imposter on an auto-assigned port (issue #1438) and returns it.
async fn create(manager: &ImposterManager, cfg: Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("config decodes");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

async fn get(port: u16, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("send")
}

/// The status and, for a 500, the envelope's `code` and `message`.
async fn answer(port: u16, path: &str) -> (u16, String) {
    let resp = get(port, path).await;
    let status = resp.status().as_u16();
    (status, resp.text().await.expect("body"))
}

/// Asserts the `500` Mountebank answers for a status Node refuses, naming the value as resolved.
async fn assert_invalid_status(port: u16, path: &str, value: &str) {
    let resp = get(port, path).await;
    assert_eq!(resp.status(), 500, "{path}: an invalid status answers 500");
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "{path}: the error envelope is JSON"
    );
    let body: Value = resp.json().await.expect("json body");
    let error = &body["errors"][0];
    assert_eq!(
        error["code"], "ERR_HTTP_INVALID_STATUS_CODE",
        "{path}: {body}"
    );
    assert_eq!(error["name"], "RangeError", "{path}: {body}");
    assert_eq!(
        error["message"],
        format!("Invalid status code: {value}"),
        "{path}: {body}"
    );
}

fn copy_code_from_path() -> Value {
    json!([{ "copy": {
        "from": "path",
        "into": "${code}",
        "using": { "method": "regex", "selector": "\\d+" }
    } }])
}

#[tokio::test]
async fn copy_substitutes_a_token_in_is_status_code() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [ { "responses": [ {
            "is": { "statusCode": "${code}", "body": "copied" },
            "behaviors": copy_code_from_path()
        } ] } ] }),
    )
    .await;

    assert_eq!(answer(port, "/404").await, (404, "copied".to_string()));
    assert_eq!(answer(port, "/201").await, (201, "copied".to_string()));
    // Nothing to copy: the token stays as written, and Node cannot read it.
    assert_invalid_status(port, "/abc", "${code}").await;
    // Copied, but outside 100–999.
    assert_invalid_status(port, "/99", "99").await;
    assert_invalid_status(port, "/1000", "1000").await;

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn copy_substitutes_part_of_a_status_code() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [ { "responses": [ {
            "is": { "statusCode": "4${code}" },
            "behaviors": copy_code_from_path()
        } ] } ] }),
    )
    .await;

    assert_eq!(answer(port, "/04").await.0, 404);

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn lookup_substitutes_a_csv_cell_in_is_status_code() {
    let dir = std::env::temp_dir().join(format!("rift_1558_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let csv = dir.join("codes.csv");
    std::fs::write(&csv, "name,code\nmountebank,400\nbroken,4x0\n").expect("write csv");

    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [ { "responses": [ {
            "is": { "statusCode": "${row}[code]", "body": "looked up" },
            "behaviors": [ { "lookup": {
                "key": { "from": "path", "using": { "method": "regex", "selector": "\\w+" } },
                "fromDataSource": { "csv": { "path": csv.to_str().expect("utf-8 path"), "keyColumn": "name" } },
                "into": "${row}"
            } } ]
        } ] } ] }),
    )
    .await;

    assert_eq!(
        answer(port, "/mountebank").await,
        (400, "looked up".to_string())
    );
    assert_invalid_status(port, "/broken", "4x0").await;
    // No row: the token stays as written.
    assert_invalid_status(port, "/nobody", "${row}[code]").await;

    let _ = manager.delete_imposter(port).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without behaviors the string is read as Node reads it: whitespace, a fraction and a hex literal
/// are all a number; anything else is not.
#[tokio::test]
async fn a_string_status_is_read_as_node_reads_it() {
    let manager = ImposterManager::new();
    let stub = |path: &str, status: Value| {
        json!({
            "predicates": [ { "equals": { "path": path } } ],
            "responses": [ { "is": { "statusCode": status } } ]
        })
    };
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [
            stub("/padded", json!(" 404 ")),
            stub("/fraction", json!("404.7")),
            stub("/hex", json!("0x194")),
            stub("/exponent", json!("2.01e2")),
            stub("/word", json!("abc")),
            stub("/token", json!("${code}")),
            stub("/number99", json!(99)),
        ] }),
    )
    .await;

    assert_eq!(answer(port, "/padded").await.0, 404);
    assert_eq!(answer(port, "/fraction").await.0, 404);
    assert_eq!(answer(port, "/hex").await.0, 404);
    assert_eq!(answer(port, "/exponent").await.0, 201);
    assert_invalid_status(port, "/word", "abc").await;
    assert_invalid_status(port, "/token", "${code}").await;
    // A number outside 100–999 takes the same door (it used to be "Response build error").
    assert_invalid_status(port, "/number99", "99").await;

    let _ = manager.delete_imposter(port).await;
}

/// An empty string is falsy: `defaultResponse.statusCode`, else `200`.
#[tokio::test]
async fn an_empty_status_falls_back_to_the_default_then_200() {
    let manager = ImposterManager::new();
    let bare = create(
        &manager,
        json!({ "protocol": "http", "stubs": [ { "responses": [ { "is": { "statusCode": "" } } ] } ] }),
    )
    .await;
    assert_eq!(answer(bare, "/").await.0, 200);

    let defaulted = create(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": { "statusCode": 418 },
            "stubs": [ { "responses": [ { "is": { "statusCode": "" } } ] } ]
        }),
    )
    .await;
    assert_eq!(answer(defaulted, "/").await.0, 418);

    let _ = manager.delete_imposter(bare).await;
    let _ = manager.delete_imposter(defaulted).await;
}

/// Mountebank merges `defaultResponse` after behaviors, so a token in it is never substituted, even
/// when the stub's `copy` names it.
#[tokio::test]
async fn a_default_response_status_token_is_never_substituted() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": { "statusCode": "${code}" },
            "stubs": [ {
                "predicates": [ { "startsWith": { "path": "/stub" } } ],
                "responses": [ { "is": { "body": "omits status" }, "behaviors": copy_code_from_path() } ]
            } ]
        }),
    )
    .await;

    // Unmatched: the default itself.
    assert_invalid_status(port, "/", "${code}").await;
    // Matched: the stub omits its status, so the default's completes it, unsubstituted.
    assert_invalid_status(port, "/stub/404", "${code}").await;

    let _ = manager.delete_imposter(port).await;
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn decorate_sees_the_status_as_a_string() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [ { "responses": [ {
            "is": { "statusCode": "${code}" },
            "behaviors": [
                { "copy": {
                    "from": "path",
                    "into": "${code}",
                    "using": { "method": "regex", "selector": "\\d+" }
                } },
                { "decorate": "function (request, response) { response.body = typeof response.statusCode + ':' + response.statusCode; }" }
            ]
        } ] } ] }),
    )
    .await;

    assert_eq!(answer(port, "/404").await, (404, "string:404".to_string()));

    let _ = manager.delete_imposter(port).await;
}
