//! Issue #1565: Mountebank's `--apikey` reads the key from the `x-api-key` header; Rift's
//! `--api-key` read it only from `Authorization`. A Mountebank client therefore got 401 from Rift
//! with a key that worked against `mb`. The admin API now accepts either header. When both are
//! sent, `Authorization` decides alone, so a request never authenticates on whichever of two
//! headers happens to match. Also `--apikey`, Mountebank's spelling of the flag (deferred from
//! #1559 until the header it implies was read).

use std::sync::Arc;

use clap::Parser;
use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use rift_http_proxy::server::Cli;

const KEY: &str = "s3cret-token";

async fn start() -> (String, Arc<ImposterManager>, RunningAdminApi) {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        manager.clone(),
        Some(KEY.to_string()),
    )
    .bind()
    .await
    .expect("admin binds");
    (format!("http://{}", running.local_addr()), manager, running)
}

async fn get_imposters(base: &str, headers: &[(&str, &str)]) -> u16 {
    let mut request = reqwest::Client::new().get(format!("{base}/imposters"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request
        .send()
        .await
        .expect("GET /imposters")
        .status()
        .as_u16()
}

#[tokio::test]
async fn x_api_key_alone_authenticates() {
    let (base, _manager, admin) = start().await;
    assert_eq!(
        get_imposters(&base, &[("x-api-key", KEY)]).await,
        200,
        "the key in Mountebank's x-api-key header must authenticate, as it does against mb"
    );
    // Header names are case-insensitive.
    assert_eq!(get_imposters(&base, &[("X-API-KEY", KEY)]).await, 200);
    admin.shutdown().await;
}

#[tokio::test]
async fn a_wrong_x_api_key_is_refused() {
    let (base, _manager, admin) = start().await;
    assert_eq!(
        get_imposters(&base, &[("x-api-key", "s3cret-tokeX")]).await,
        401
    );
    assert_eq!(get_imposters(&base, &[("x-api-key", "")]).await, 401);
    assert_eq!(get_imposters(&base, &[]).await, 401, "no key at all");
    admin.shutdown().await;
}

// Authorization wins: a wrong Authorization is not rescued by a right x-api-key ...
#[tokio::test]
async fn a_wrong_authorization_is_not_rescued_by_a_right_x_api_key() {
    let (base, _manager, admin) = start().await;
    assert_eq!(
        get_imposters(&base, &[("authorization", "wrong"), ("x-api-key", KEY)]).await,
        401,
        "Authorization decides when both headers are sent"
    );
    // An empty Authorization is still present, and still decides.
    assert_eq!(
        get_imposters(&base, &[("authorization", ""), ("x-api-key", KEY)]).await,
        401
    );
    admin.shutdown().await;
}

// ... and the other order: a right Authorization is not spoiled by a wrong x-api-key.
#[tokio::test]
async fn a_right_authorization_ignores_a_wrong_x_api_key() {
    let (base, _manager, admin) = start().await;
    assert_eq!(
        get_imposters(&base, &[("authorization", KEY), ("x-api-key", "wrong")]).await,
        200
    );
    assert_eq!(
        get_imposters(&base, &[("x-api-key", "wrong"), ("authorization", KEY)]).await,
        200,
        "header order on the wire does not matter"
    );
    admin.shutdown().await;
}

// The `/__rift/` gateway is data-plane traffic and needs no admin key in either header (#212), so
// an app under test never has to carry x-api-key for its requests to match.
#[tokio::test]
async fn the_gateway_needs_neither_header() {
    let (base, manager, admin) = start().await;
    let port = manager
        .create_imposter(
            serde_json::from_value(serde_json::json!({
                "protocol": "http",
                "stubs": [{
                    "predicates": [{"equals": {"path": "/x"}}],
                    "responses": [{"is": {"statusCode": 200, "body": "open"}}]
                }]
            }))
            .expect("config"),
        )
        .await
        .expect("create imposter");
    let client = reqwest::Client::new();

    let bare = client
        .get(format!("{base}/__rift/{port}/x"))
        .send()
        .await
        .expect("gateway");
    assert_eq!(bare.status(), 200, "the gateway is not gated");
    assert_eq!(bare.text().await.expect("body"), "open");

    let wrong = client
        .get(format!("{base}/__rift/{port}/x"))
        .header("x-api-key", "not-the-admin-key")
        .send()
        .await
        .expect("gateway");
    assert_eq!(
        wrong.status(),
        200,
        "a wrong x-api-key on the gateway is the app's own header, not a failed admin login"
    );

    let _ = manager.delete_imposter(port).await;
    admin.shutdown().await;
}

#[test]
fn apikey_is_an_alias_of_api_key() {
    let cli = Cli::try_parse_from(["rift", "--apikey", KEY]).expect("--apikey must parse");
    assert_eq!(cli.api_key.as_deref(), Some(KEY));
    // Global like the long flag, so it binds after a subcommand too (#1316).
    let cli = Cli::try_parse_from(["rift", "start", "--apikey", KEY]).expect("--apikey must parse");
    assert_eq!(cli.api_key.as_deref(), Some(KEY));
}
