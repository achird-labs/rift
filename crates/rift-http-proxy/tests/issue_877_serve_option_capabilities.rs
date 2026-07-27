//! Issue #877: `GET /config` publishes the same serve-option capability list the C-ABI does, so an
//! HTTP-only consumer can feature-detect without going through `rift_build_info`.
//!
//! The list exists because `ServeOptions` fields have no symbol to probe — `rift_abi_version`'s
//! "additive symbols are discovered by presence" convention cannot reach them. Absence of the key
//! is the signal an engine is too old to report capabilities, which is why this works against
//! already-released engines where a `deny_unknown_fields` rejection would not.

use rift_http_proxy::admin_api::{AdminApiServer, SERVE_OPTION_KEYS};
use rift_http_proxy::imposter::ImposterManager;
use std::sync::Arc;

// AC6: the HTTP surface advertises exactly the shared constant — not a hand-copied second literal,
// which is the drift this asserts against.
#[tokio::test]
async fn get_config_publishes_the_serve_option_capability_list() {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());

    let config: serde_json::Value = reqwest::get(format!("{base}/config"))
        .await
        .expect("GET /config")
        .json()
        .await
        .expect("config is JSON");

    let published: Vec<&str> = config["serveOptions"]
        .as_array()
        .expect("GET /config exposes a serveOptions array")
        .iter()
        .map(|v| v.as_str().expect("entries are strings"))
        .collect();
    assert_eq!(
        published, SERVE_OPTION_KEYS,
        "the HTTP surface must publish the shared list verbatim"
    );

    // The Mountebank-compatible `options` object is deliberately untouched: the capability list is
    // a sibling key, not a new member of a shape existing clients already parse.
    assert!(
        config["options"]["port"].is_number(),
        "the Mountebank-shaped options object must be unchanged"
    );
    assert!(
        config["options"]["serveOptions"].is_null(),
        "the capability list must NOT be nested inside the Mountebank options object"
    );

    running.shutdown().await;
}
