//! Issue #1073 — the `{{ }}` pass repairs only what it substituted, not the whole header value.
//!
//! PR #1072 drew that boundary for the `${request.*}` pass and the `copy`/`lookup` behaviors: text
//! the author wrote into a header is their own, so a control character there still fails the
//! response, while text substituted from request data is repaired. The `{{ }}` pass had repaired
//! every header value whole since #359 B3, so on a `_rift.templated` response the author's own
//! stray byte was silently stripped — and the warning blamed the client for it.
//!
//! Runs with `RIFT_DEBUG` unset, so a failing token substitutes an empty string rather than
//! failing the render.

use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use std::time::Duration;

fn cfg(v: serde_json::Value) -> ImposterConfig {
    serde_json::from_value(v).expect("valid imposter config")
}

async fn spawn(manager: &ImposterManager, config: serde_json::Value) {
    manager
        .create_imposter(cfg(config))
        .await
        .expect("create imposter");
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// A control character the author typed into a header of a templated response is an authoring bug
/// and must reach the response builder, exactly as it does on every other path. Before this change
/// the `{{ }}` pass stripped it and answered 200.
#[tokio::test]
async fn a_literal_control_char_in_a_templated_header_fails_loudly() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21360, "protocol": "http",
            "stubs": [{ "responses": [{
                "is": { "statusCode": 200, "headers": { "X-Bad": "line1\nline2" }, "body": "ok" },
                "_rift": { "templated": true }
            }]}]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:21360/x")
        .await
        .expect("request");

    assert_eq!(
        resp.status(),
        500,
        "a literal control character is the author's bug and must not be silently repaired"
    );
}

/// The sharper half: a token elsewhere in the value must not launder the author's literal byte.
#[tokio::test]
async fn a_literal_control_char_beside_a_template_token_fails_loudly() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21361, "protocol": "http",
            "stubs": [{ "responses": [{
                "is": {
                    "statusCode": 200,
                    "headers": { "X-Mixed": "audit\nlog{{ request.query.x }}" },
                    "body": "ok"
                },
                "_rift": { "templated": true }
            }]}]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:21361/x?x=fine")
        .await
        .expect("request");

    assert_eq!(
        resp.status(),
        500,
        "the repair covers the substituted span only, never the literal text around it"
    );
}

/// The half that must not regress: a control character arriving *through* a token is still removed.
#[tokio::test]
async fn a_substituted_control_char_is_still_repaired() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21362, "protocol": "http",
            "stubs": [{ "responses": [{
                "is": {
                    "statusCode": 200,
                    "headers": { "X-Echo": "{{ request.query.x }}" },
                    "body": "ok"
                },
                "_rift": { "templated": true }
            }]}]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:21362/x?x=A%0D%0AInjected:%20yes")
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-echo")
            .expect("x-echo present")
            .as_bytes(),
        b"AInjected: yes",
        "CR and LF from the substitution are still removed"
    );
    assert!(
        resp.headers().get("injected").is_none(),
        "the #359 B3 injection defence still holds"
    );
}

/// The filter itself is unchanged: HTAB and non-ASCII are legal in a header value (#1058) and must
/// survive a substitution byte-exact.
#[tokio::test]
async fn a_substituted_tab_and_non_ascii_survive_byte_exact() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21363, "protocol": "http",
            "stubs": [{ "responses": [{
                "is": {
                    "statusCode": 200,
                    "headers": { "X-Echo": "{{ request.query.x }}" },
                    "body": "ok"
                },
                "_rift": { "templated": true }
            }]}]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:21363/x?x=a%09b-Jos%C3%A9")
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-echo")
            .expect("x-echo present")
            .as_bytes(),
        "a\tb-José".as_bytes()
    );
}

/// Several tokens in one value are each repaired independently, and the literal text between them
/// is left exactly as written.
#[tokio::test]
async fn several_tokens_in_one_value_are_each_repaired() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21364, "protocol": "http",
            "stubs": [{ "responses": [{
                "is": {
                    "statusCode": 200,
                    "headers": { "X-Echo": "{{ request.query.a }}|{{ request.query.b }}" },
                    "body": "ok"
                },
                "_rift": { "templated": true }
            }]}]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:21364/x?a=A%0DX&b=B%0AY")
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-echo")
            .expect("x-echo present")
            .as_bytes(),
        b"AX|BY",
        "each substitution is repaired; the literal pipe between them is untouched"
    );
}

/// Bodies are never filtered — only header values. A templated body keeps every byte it rendered.
#[tokio::test]
async fn the_templated_body_is_not_sanitized() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21365, "protocol": "http",
            "stubs": [{ "responses": [{
                "is": { "statusCode": 200, "body": "[{{ request.query.x }}]" },
                "_rift": { "templated": true }
            }]}]
        }),
    )
    .await;

    let body = reqwest::get("http://127.0.0.1:21365/x?x=a%0Ab")
        .await
        .expect("request")
        .text()
        .await
        .expect("body");

    assert_eq!(
        body, "[a\nb]",
        "a newline in a rendered body is data, not a defect"
    );
}
