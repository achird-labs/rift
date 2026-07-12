//! Runnable end-to-end example (epic #394, slice 5): mock a hard-coded external HTTPS config CDN
//! with Rift's intercept proxy — replacing a mitmproxy sidecar with no committed crypto.
//!
//! Run: `cargo test -p rift-http-proxy --test intercept_config_cdn_example`
//!
//! Scenario: a feature-flag SDK always fetches `https://cdn.example.com/config.json`. Its target
//! host is hard-coded, so we can't point it at a mock port — we intercept the TLS call and answer
//! it from Rift. This is the executable companion to `docs/features/intercept-proxy.md`.

use std::collections::HashMap;
use std::sync::Arc;

use rift_http_proxy::intercept::InterceptListener;
use rift_http_proxy::intercept_rules::{InterceptAction, InterceptRule, InterceptRules, ServeStub};
use rift_mock_core::proxy::intercept_ca::{CertificateAuthority, SniCertResolver};

#[tokio::test]
async fn intercepts_external_config_cdn_without_mitmproxy() {
    // 1. Rift generates an intercept CA at startup (or load one via `CertificateAuthority::load_pem`).
    //    No CA cert, private key, or truststore is committed to the repo.
    let ca = Arc::new(CertificateAuthority::generate().expect("generate intercept CA"));
    let ca_pem = ca.ca_cert_pem().to_string();

    // 2. Declare an intercept rule: when the SUT calls cdn.example.com/config.json, serve the flag
    //    config inline. (Swap `Serve` for `InterceptAction::Forward { port }` to route the call to
    //    one of your imposters instead.)
    let rules = InterceptRules::new();
    let path_predicate =
        serde_json::from_value(serde_json::json!({ "equals": { "path": "/config.json" } }))
            .expect("valid predicate");
    let mut headers = HashMap::new();
    headers.insert("content-type".to_string(), "application/json".to_string());
    rules
        .add(InterceptRule {
            host: Some("cdn.example.com".to_string()),
            predicates: vec![path_predicate],
            action: InterceptAction::Serve(ServeStub {
                status_code: 200,
                headers,
                body: Some(r#"{"featureX":"ON"}"#.to_string()),
            }),
        })
        .unwrap();

    // 3. Start the intercept listener. An embedder would also expose the admin API by building the
    //    admin server `with_intercept(...)`; here we drive the rule store directly.
    let resolver = Arc::new(SniCertResolver::new(ca));
    let listener = InterceptListener::bind("127.0.0.1:0".parse().unwrap(), resolver, rules)
        .await
        .expect("bind intercept listener");

    // 4. The SUT: an HTTP client that trusts ONLY the intercept CA and routes HTTPS through the
    //    listener. The JVM equivalent is:
    //      -Djavax.net.ssl.trustStore=ts.jks -Djavax.net.ssl.trustStorePassword=changeit
    //      -Dhttps.proxyHost=<host> -Dhttps.proxyPort=<port>
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(format!("http://{}", listener.local_addr())).unwrap())
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
        .build()
        .unwrap();

    // 5. The SUT's hard-coded HTTPS call is intercepted and answered by Rift — no mitmproxy.
    let resp = client
        .get("https://cdn.example.com/config.json")
        .send()
        .await
        .expect("config fetched through the intercept proxy");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(resp.text().await.unwrap(), r#"{"featureX":"ON"}"#);

    listener.shutdown().await;
}
