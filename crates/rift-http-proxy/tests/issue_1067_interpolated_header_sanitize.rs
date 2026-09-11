//! Issue #1067 — a header value the `${request.*}` pass rewrote is repaired, not 500'd.
//!
//! The `{{ }}` pass has sanitized its rendered header values since #359 B3. The `${request.*}`
//! pass runs *after* it on the same map and did not, so a control character a client put in a
//! query string or body reached `Builder::header` and surfaced as a 500 `Response build error`.
//! Same stub, same byte, two different answers depending on which syntax the author used.
//!
//! The boundary these tests also pin: a header value that is bad *in the config* still fails
//! loudly. Only values derived from request data at serve time are repaired.

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

/// A CR/LF a client put in the query string is removed, the request succeeds, and no second
/// header line is smuggled in. Before the fix this was a 500.
#[tokio::test]
async fn query_interpolated_header_with_crlf_is_repaired_not_500() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21350, "protocol": "http",
            "stubs": [{ "responses": [{ "is": {
                "statusCode": 200,
                "headers": { "X-Echo": "${request.query.x}" },
                "body": "ok"
            }}]}]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:21350/x?x=safe%0D%0AInjected:%20yes")
        .await
        .expect("request");

    assert_eq!(
        resp.status(),
        200,
        "a client-supplied CR/LF must not 500 the stub"
    );
    assert_eq!(
        resp.headers()
            .get("x-echo")
            .expect("x-echo present")
            .as_bytes(),
        b"safeInjected: yes",
        "CR and LF are removed, the rest of the value survives"
    );
    assert!(
        resp.headers().get("injected").is_none(),
        "the CR/LF must not have opened a second header line"
    );
}

/// A NUL in the request body reaches the header through `${request.body}` and is removed.
#[tokio::test]
async fn body_interpolated_header_with_nul_is_repaired() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21351, "protocol": "http",
            "stubs": [{ "responses": [{ "is": {
                "statusCode": 200,
                "headers": { "X-Echo": "${request.body}" },
                "body": "ok"
            }}]}]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .post("http://127.0.0.1:21351/x")
        .body(vec![b'a', 0, b'b'])
        .send()
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-echo")
            .expect("x-echo present")
            .as_bytes(),
        b"ab",
        "NUL is a byte a header value cannot carry; the rest survives"
    );
}

/// HTAB and non-ASCII obs-text are *legal* in a header value, so the repair must not touch them.
/// This is the #1058 rule the `{{ }}` pass already follows; the `${}` pass must not re-introduce
/// the over-stripping that issue removed.
#[tokio::test]
async fn interpolated_header_keeps_tab_and_non_ascii_byte_exact() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21352, "protocol": "http",
            "stubs": [{ "responses": [{ "is": {
                "statusCode": 200,
                "headers": { "X-Echo": "${request.query.x}" },
                "body": "ok"
            }}]}]
        }),
    )
    .await;

    // `a<TAB>b-José`
    let resp = reqwest::get("http://127.0.0.1:21352/x?x=a%09b-Jos%C3%A9")
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-echo")
            .expect("x-echo present")
            .as_bytes(),
        "a\tb-José".as_bytes(),
        "tab and obs-text are representable and must survive byte-exact"
    );
}

/// Only header values are repaired. A body may carry any byte, and interpolating one must not
/// start filtering it — that would corrupt binary and text payloads alike.
#[tokio::test]
async fn the_interpolated_body_is_not_sanitized() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21353, "protocol": "http",
            "stubs": [{ "responses": [{ "is": {
                "statusCode": 200,
                "body": "[${request.query.x}]"
            }}]}]
        }),
    )
    .await;

    let body = reqwest::get("http://127.0.0.1:21353/x?x=a%0Ab")
        .await
        .expect("request")
        .text()
        .await
        .expect("body");

    assert_eq!(
        body, "[a\nb]",
        "a newline in an interpolated body is data, not a defect"
    );
}

/// The boundary. A header that is bad *in the config* is an authoring error and still fails
/// loudly, even on a response whose other header went through the interpolation pass. This is
/// what proves the repair was not centralized at the response builder.
#[tokio::test]
async fn a_literal_bad_header_still_fails_loudly_beside_an_interpolated_one() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21354, "protocol": "http",
            "stubs": [{ "responses": [{ "is": {
                "statusCode": 200,
                "headers": { "X-Echo": "${request.query.x}", "X-Bad": "line1\nline2" },
                "body": "ok"
            }}]}]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:21354/x?x=fine")
        .await
        .expect("request");

    assert_eq!(
        resp.status(),
        500,
        "a literal control character in the config is an authoring bug and stays loud"
    );
}

/// The sharper half of the boundary: a literal control character sitting *beside* an interpolated
/// token is still an authoring bug. Only the substituted span is repaired, so this response still
/// fails loudly — the token's presence must not launder the author's own bad byte.
#[tokio::test]
async fn a_literal_control_char_beside_an_interpolated_token_still_fails_loudly() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21357, "protocol": "http",
            "stubs": [{ "responses": [{ "is": {
                "statusCode": 200,
                "headers": { "X-Echo": "audit\nlog${request.query.x}" },
                "body": "ok"
            }}]}]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:21357/x?x=fine")
        .await
        .expect("request");

    assert_eq!(
        resp.status(),
        500,
        "the repair covers the substituted value only, never the literal text around it"
    );
}

/// Both passes run over the same value, in order. Each removes what it must, and the value the
/// `{{ }}` pass already sanitized passes through the second repair unchanged.
#[tokio::test]
async fn templated_then_interpolated_value_is_repaired_on_both_passes() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21355, "protocol": "http",
            "stubs": [{ "responses": [{
                "is": {
                    "statusCode": 200,
                    "headers": { "X-Echo": "{{ request.query.a }}|${request.query.b}" },
                    "body": "ok"
                },
                "_rift": { "templated": true }
            }]}]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:21355/x?a=A%0D%0AX&b=B%0AY")
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-echo")
            .expect("x-echo present")
            .as_bytes(),
        b"AX|BY",
        "the handlebars half and the dollar-brace half are both repaired"
    );
}

/// A repeated header keeps one line per value through the repair (RFC 7230 §3.2.2 forbids
/// folding `Set-Cookie`), and only the value that carried a bad byte is altered.
#[tokio::test]
async fn multi_value_headers_keep_their_multiplicity_through_the_repair() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 21356, "protocol": "http",
            "stubs": [{ "responses": [{ "is": {
                "statusCode": 200,
                "headers": { "Set-Cookie": ["a=1", "b=${request.query.x}"] },
                "body": "ok"
            }}]}]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:21356/x?x=2%0D%0AEvil:%20yes")
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    let cookies: Vec<&[u8]> = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.as_bytes())
        .collect();
    assert_eq!(
        cookies,
        vec![&b"a=1"[..], &b"b=2Evil: yes"[..]],
        "two cookie lines survive; only the interpolated one is repaired"
    );
    assert!(resp.headers().get("evil").is_none());
}
