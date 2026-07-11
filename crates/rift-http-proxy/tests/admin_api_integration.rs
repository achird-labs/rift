//! Admin HTTP API integration tests (relocated from rift-mock-core in issue #203, since they spin up
//! the `AdminApiServer` which lives in this server crate).

use rift_http_proxy::imposter::ImposterManager;

async fn text(c: &reqwest::Client, url: String) -> String {
    c.get(url)
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text")
}
async fn json(c: &reqwest::Client, url: String) -> serde_json::Value {
    serde_json::from_str(&text(c, url).await).expect("json")
}
async fn get(c: &reqwest::Client, port: u16, path: &str, space: Option<&str>) -> reqwest::Response {
    let mut req = c.get(format!("http://127.0.0.1:{port}{path}"));
    if let Some(s) = space {
        req = req.header("X-Mock-Space", s);
    }
    req.send().await.expect("send")
}

fn order_fsm(port: u16, flow_id_source: Option<&str>) -> serde_json::Value {
    let mut flow_state = serde_json::json!({ "backend": "inmemory", "ttlSeconds": 300 });
    if let Some(src) = flow_id_source {
        flow_state["flowIdSource"] = serde_json::json!(src);
    }
    serde_json::json!({
        "port": port, "protocol": "http",
        "_rift": { "flowState": flow_state },
        "stubs": [
            { "scenarioName": "order", "requiredScenarioState": "Started",
              "predicates": [{ "equals": { "path": "/status" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "unpaid" } }] },
            { "scenarioName": "order", "requiredScenarioState": "Started", "newScenarioState": "paid",
              "predicates": [{ "equals": { "path": "/pay" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] },
            { "scenarioName": "order", "requiredScenarioState": "paid",
              "predicates": [{ "equals": { "path": "/status" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "paid" } }] }
        ]
    })
}

fn correlated_config(port: u16, stubs: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "port": port, "protocol": "http", "recordRequests": true,
        "_rift": { "flowState": { "backend": "inmemory", "ttlSeconds": 300,
            "flowIdSource": "header:X-Mock-Space" } },
        "stubs": stubs
    })
}

#[tokio::test]
async fn scenario_admin_endpoints_arrange_inspect_reset() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(order_fsm(19763, None)).unwrap();
    manager.create_imposter(config).await.expect("create");

    let admin_addr = "127.0.0.1:12590".parse().unwrap();
    let server = rift_http_proxy::admin_api::AdminApiServer::new(admin_addr, manager.clone(), None);
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let c = reqwest::Client::new();
    let admin = "http://127.0.0.1:12590";

    // GET scenarios → initial "Started"
    let v = json(&c, format!("{admin}/imposters/19763/scenarios")).await;
    assert_eq!(v["scenarios"][0]["name"], "order");
    assert_eq!(v["scenarios"][0]["state"], "Started");

    // PUT state=paid → a subsequent request observes it
    let r = c
        .put(format!("{admin}/imposters/19763/scenarios/order/state"))
        .header("content-type", "application/json")
        .body(r#"{"state":"paid"}"#)
        .send()
        .await
        .expect("put");
    assert_eq!(r.status(), 200);
    assert_eq!(
        text(&c, "http://127.0.0.1:19763/status".to_string()).await,
        "paid"
    );

    // GET reflects the transition
    let v = json(&c, format!("{admin}/imposters/19763/scenarios")).await;
    assert_eq!(v["scenarios"][0]["state"], "paid");

    // reset → back to initial
    let r = c
        .post(format!("{admin}/imposters/19763/scenarios/reset"))
        .send()
        .await
        .expect("reset");
    assert_eq!(r.status(), 200);
    assert_eq!(
        text(&c, "http://127.0.0.1:19763/status".to_string()).await,
        "unpaid"
    );

    // flow-state KV: PUT / GET / DELETE (default flow_id = imposter_port = "19763")
    let kv = format!("{admin}/admin/imposters/19763/flow-state/19763/mykey");
    let r = c
        .put(&kv)
        .header("content-type", "application/json")
        .body(r#"{"value":42}"#)
        .send()
        .await
        .expect("put kv");
    assert_eq!(r.status(), 200);
    assert_eq!(json(&c, kv.clone()).await["value"], 42);
    let r = c.delete(&kv).send().await.expect("del kv");
    assert_eq!(r.status(), 200);
    let r = c.get(&kv).send().await.expect("get kv");
    assert_eq!(r.status(), 404, "deleted key → 404");

    let _ = manager.delete_imposter(19763).await;
}

#[tokio::test]
async fn scenario_admin_reset_is_per_flow_with_explicit_flow_id() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(order_fsm(19765, Some("header:X-Mock-Space"))).unwrap();
    manager.create_imposter(config).await.expect("create");

    let admin_addr = "127.0.0.1:12591".parse().unwrap();
    let server = rift_http_proxy::admin_api::AdminApiServer::new(admin_addr, manager.clone(), None);
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let c = reqwest::Client::new();
    let admin = "http://127.0.0.1:12591";
    let set_state = |flow: &str| {
        c.put(format!("{admin}/imposters/19765/scenarios/order/state"))
            .header("content-type", "application/json")
            .body(format!(r#"{{"state":"paid","flowId":"{flow}"}}"#))
            .send()
    };
    // Arrange both flows to "paid" via explicit flowId
    assert_eq!(set_state("alpha").await.expect("a").status(), 200);
    assert_eq!(set_state("beta").await.expect("b").status(), 200);

    // Reset ONLY alpha
    let r = c
        .post(format!("{admin}/imposters/19765/scenarios/reset"))
        .header("content-type", "application/json")
        .body(r#"{"flowId":"alpha"}"#)
        .send()
        .await
        .expect("reset");
    assert_eq!(r.status(), 200);

    // alpha back to initial; beta untouched — via GET ?flowId=
    let a = json(
        &c,
        format!("{admin}/imposters/19765/scenarios?flowId=alpha"),
    )
    .await;
    let b = json(&c, format!("{admin}/imposters/19765/scenarios?flowId=beta")).await;
    assert_eq!(
        a["scenarios"][0]["state"], "Started",
        "alpha reset to initial"
    );
    assert_eq!(
        b["scenarios"][0]["state"], "paid",
        "beta untouched by alpha reset"
    );

    let _ = manager.delete_imposter(19765).await;
}

#[tokio::test]
async fn space_teardown_is_isolated() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(correlated_config(
        19773,
        serde_json::json!([
            { "space": "alpha", "predicates": [{ "equals": { "path": "/data" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "ALPHA" } }] },
            { "space": "beta", "predicates": [{ "equals": { "path": "/data" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "BETA" } }] }
        ]),
    ))
    .unwrap();
    manager.create_imposter(config).await.expect("create");

    let admin_addr = "127.0.0.1:12592".parse().unwrap();
    let server = rift_http_proxy::admin_api::AdminApiServer::new(admin_addr, manager.clone(), None);
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let c = reqwest::Client::new();
    let admin = "http://127.0.0.1:12592";

    // record one request per space
    let _ = get(&c, 19773, "/data", Some("alpha")).await;
    let _ = get(&c, 19773, "/data", Some("beta")).await;

    // tear down alpha only
    let r = c
        .delete(format!("{admin}/imposters/19773/spaces/alpha"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // alpha's recorded requests are cleared (check BEFORE any new alpha request below,
    // which would re-record one).
    let alpha_reqs = c
        .get(format!(
            "{admin}/imposters/19773/requests?match=header:X-Mock-Space=alpha"
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(alpha_reqs, "[]", "alpha recorded requests cleared");
    // alpha's stub no longer matches /data
    let alpha_after = get(&c, 19773, "/data", Some("alpha"))
        .await
        .text()
        .await
        .unwrap();
    assert_ne!(alpha_after, "ALPHA", "alpha stubs removed");

    // beta is fully intact
    assert_eq!(
        get(&c, 19773, "/data", Some("beta"))
            .await
            .text()
            .await
            .unwrap(),
        "BETA",
        "beta untouched"
    );
    let beta_space = c
        .get(format!("{admin}/imposters/19773/spaces/beta/stubs"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        beta_space.contains("BETA"),
        "beta stubs intact: {beta_space}"
    );

    let _ = manager.delete_imposter(19773).await;
}

#[tokio::test]
async fn space_teardown_resets_scenario_state_and_leaves_others() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    // Two spaces each running the "order" FSM, declared only on their own scoped stubs.
    let fsm = |space: &str| {
        serde_json::json!({
            "space": space, "scenarioName": "order",
            "requiredScenarioState": "Started", "newScenarioState": "paid",
            "predicates": [{ "equals": { "path": "/pay" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "ok" } }]
        })
    };
    let config = serde_json::from_value(correlated_config(
        19774,
        serde_json::json!([fsm("alpha"), fsm("beta")]),
    ))
    .unwrap();
    manager.create_imposter(config).await.expect("create");

    let admin_addr = "127.0.0.1:12593".parse().unwrap();
    let server = rift_http_proxy::admin_api::AdminApiServer::new(admin_addr, manager.clone(), None);
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let c = reqwest::Client::new();
    let admin = "http://127.0.0.1:12593";
    let state_url = |flow: &str| format!("{admin}/admin/imposters/19774/flow-state/{flow}/order");

    // advance both spaces' "order" scenario to paid
    let _ = get(&c, 19774, "/pay", Some("alpha")).await;
    let _ = get(&c, 19774, "/pay", Some("beta")).await;
    assert_eq!(
        c.get(state_url("alpha")).send().await.unwrap().status(),
        200,
        "alpha order state set before teardown"
    );

    // tear down alpha
    let r = c
        .delete(format!("{admin}/imposters/19774/spaces/alpha"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // alpha's scenario state is reset (key deleted ⇒ 404); beta's survives
    assert_eq!(
        c.get(state_url("alpha")).send().await.unwrap().status(),
        404,
        "alpha scenario state reset by teardown"
    );
    let beta_state = c.get(state_url("beta")).send().await.unwrap();
    assert_eq!(beta_state.status(), 200, "beta scenario state untouched");
    assert!(beta_state.text().await.unwrap().contains("paid"));

    let _ = manager.delete_imposter(19774).await;
}

#[tokio::test]
async fn space_stub_registration_and_inspection_endpoints() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(correlated_config(19775, serde_json::json!([]))).unwrap();
    manager.create_imposter(config).await.expect("create");

    let admin_addr = "127.0.0.1:12594".parse().unwrap();
    let server = rift_http_proxy::admin_api::AdminApiServer::new(admin_addr, manager.clone(), None);
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let c = reqwest::Client::new();
    let admin = "http://127.0.0.1:12594";

    // register a stub scoped to "alpha" via the space endpoint
    let r = c
        .post(format!("{admin}/imposters/19775/spaces/alpha/stubs"))
        .header("content-type", "application/json")
        .body(r#"{"predicates":[{"equals":{"path":"/data"}}],"responses":[{"is":{"statusCode":200,"body":"ALPHA"}}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // it matches alpha's requests and is gated from other spaces
    assert_eq!(
        get(&c, 19775, "/data", Some("alpha"))
            .await
            .text()
            .await
            .unwrap(),
        "ALPHA"
    );
    let beta = get(&c, 19775, "/data", Some("beta"))
        .await
        .text()
        .await
        .unwrap();
    assert_ne!(
        beta, "ALPHA",
        "space-scoped stub must not match other spaces"
    );

    // inspection: GET /spaces/alpha reports the stub + a per-space request count
    let body = c
        .get(format!("{admin}/imposters/19775/spaces/alpha"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let space: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(space["space"], "alpha");
    assert_eq!(space["stubs"].as_array().unwrap().len(), 1);
    assert_eq!(space["numberOfRequests"], 1, "one alpha request recorded");

    let _ = manager.delete_imposter(19775).await;
}

// Issue #202: id-addressed stub operations over the admin HTTP API.
#[tokio::test]
async fn stub_by_id_admin_endpoints() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(serde_json::json!({
        "port": 19776, "protocol": "http", "stubs": []
    }))
    .unwrap();
    manager.create_imposter(config).await.expect("create");

    let admin_addr = "127.0.0.1:12596".parse().unwrap();
    let server = rift_http_proxy::admin_api::AdminApiServer::new(admin_addr, manager.clone(), None);
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let c = reqwest::Client::new();
    let admin = "http://127.0.0.1:12596";

    let add = |id: serde_json::Value, body: &str| {
        let stub = serde_json::json!({
            "id": id,
            "predicates": [{ "equals": { "path": "/p" } }],
            "responses": [{ "is": { "statusCode": 200, "body": body } }]
        });
        c.post(format!("{admin}/imposters/19776/stubs"))
            .header("content-type", "application/json")
            .body(serde_json::json!({ "stub": stub }).to_string())
            .send()
    };

    // add a stub with an explicit id
    assert_eq!(
        add(serde_json::json!("s1"), "one").await.unwrap().status(),
        200
    );

    // GET by id → 200 with the stub
    let got: serde_json::Value =
        serde_json::from_str(&text(&c, format!("{admin}/imposters/19776/stubs/by-id/s1")).await)
            .unwrap();
    assert_eq!(got["id"], "s1");

    // duplicate id → 409 Conflict
    assert_eq!(
        add(serde_json::json!("s1"), "dup").await.unwrap().status(),
        409
    );

    // PUT by id replaces in place
    let put = c
        .put(format!("{admin}/imposters/19776/stubs/by-id/s1"))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "id": "s1",
                "predicates": [{ "equals": { "path": "/p" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "two" } }]
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);
    // GET back confirms the content actually changed (not just the id preserved)
    let after: serde_json::Value =
        serde_json::from_str(&text(&c, format!("{admin}/imposters/19776/stubs/by-id/s1")).await)
            .unwrap();
    assert_eq!(
        after["responses"][0]["is"]["body"], "two",
        "PUT replaced the content"
    );

    // unknown id → 404 on GET and DELETE
    assert_eq!(
        c.get(format!("{admin}/imposters/19776/stubs/by-id/nope"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        c.delete(format!("{admin}/imposters/19776/stubs/by-id/nope"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // DELETE by id → 200, then it's gone
    assert_eq!(
        c.delete(format!("{admin}/imposters/19776/stubs/by-id/s1"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        c.get(format!("{admin}/imposters/19776/stubs/by-id/s1"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // POST without an id generates one so it is by-id addressable
    assert_eq!(
        add(serde_json::Value::Null, "auto").await.unwrap().status(),
        200
    );
    let imposter = manager.get_imposter(19776).unwrap();
    assert!(
        imposter.get_stubs()[0].id.is_some(),
        "POST without id should generate one"
    );

    let _ = manager.delete_imposter(19776).await;
}

// Issue #206: an imposter declared `protocol: "https"` terminates TLS on its own port.
mod https {
    use super::*;
    use rift_http_proxy::imposter::TlsDefaults;
    use std::sync::Arc;
    use std::time::Duration;

    fn gen_cert() -> (String, String) {
        let c = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .unwrap();
        (c.cert.pem(), c.key_pair.serialize_pem())
    }

    fn stub_config(port: u16, protocol: &str, cert: Option<(&str, &str)>) -> serde_json::Value {
        let mut cfg = serde_json::json!({
            "port": port, "protocol": protocol,
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "secure-ok"}}]}]
        });
        if let Some((cert, key)) = cert {
            cfg["cert"] = serde_json::json!(cert);
            cfg["key"] = serde_json::json!(key);
        }
        cfg
    }

    fn tls_client() -> reqwest::Client {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true) // self-signed / test certs
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap()
    }

    async fn serve(manager: &Arc<ImposterManager>, cfg: serde_json::Value) {
        manager
            .create_imposter(serde_json::from_value(cfg).unwrap())
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn https_imposter_with_inline_cert_serves_tls() {
        let (cert, key) = gen_cert();
        let manager = Arc::new(ImposterManager::new());
        serve(&manager, stub_config(19840, "https", Some((&cert, &key)))).await;

        let body = tls_client()
            .get("https://127.0.0.1:19840/x")
            .send()
            .await
            .expect("TLS request should succeed")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "secure-ok");
        let _ = manager.delete_imposter(19840).await;
    }

    #[tokio::test]
    async fn http_imposter_unchanged() {
        let manager = Arc::new(ImposterManager::new());
        serve(&manager, stub_config(19841, "http", None)).await;
        let body = reqwest::get("http://127.0.0.1:19841/x")
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "secure-ok", "plain http imposter unaffected");
        let _ = manager.delete_imposter(19841).await;
    }

    #[tokio::test]
    async fn https_zero_config_uses_self_signed() {
        // No cert/key, default manager (self-signed enabled) → still serves over TLS.
        let manager = Arc::new(ImposterManager::new());
        serve(&manager, stub_config(19842, "https", None)).await;
        let body = tls_client()
            .get("https://127.0.0.1:19842/x")
            .send()
            .await
            .expect("zero-config https should serve via self-signed cert")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "secure-ok");
        let _ = manager.delete_imposter(19842).await;
    }

    #[tokio::test]
    async fn https_uses_server_default_cert() {
        // Self-signed DISABLED + a server default cert → the default must be used (proves the
        // default path, not the self-signed fallback).
        let (cert, key) = gen_cert();
        let manager = Arc::new(ImposterManager::new().with_tls_defaults(TlsDefaults {
            default_cert: Some(cert),
            default_key: Some(key),
            allow_self_signed: false,
        }));
        serve(&manager, stub_config(19843, "https", None)).await;
        let body = tls_client()
            .get("https://127.0.0.1:19843/x")
            .send()
            .await
            .expect("server-default cert should serve the https imposter")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "secure-ok");
        let _ = manager.delete_imposter(19843).await;
    }

    #[tokio::test]
    async fn https_no_cert_and_self_signed_disabled_errors() {
        let manager = Arc::new(ImposterManager::new().with_tls_defaults(TlsDefaults {
            default_cert: None,
            default_key: None,
            allow_self_signed: false,
        }));
        let result = manager
            .create_imposter(serde_json::from_value(stub_config(19844, "https", None)).unwrap())
            .await;
        assert!(
            matches!(
                result,
                Err(rift_http_proxy::imposter::ImposterError::Tls(_))
            ),
            "https with no cert + self-signed disabled must error, got {result:?}"
        );
    }

    #[tokio::test]
    async fn https_invalid_cert_errors_not_panics() {
        let manager = Arc::new(ImposterManager::new());
        let result = manager
            .create_imposter(
                serde_json::from_value(stub_config(
                    19845,
                    "https",
                    Some((
                        "-----BEGIN CERTIFICATE-----\nnonsense\n-----END CERTIFICATE-----",
                        "bad",
                    )),
                ))
                .unwrap(),
            )
            .await;
        assert!(
            matches!(
                result,
                Err(rift_http_proxy::imposter::ImposterError::Tls(_))
            ),
            "invalid cert/key must be a defined Tls error, got {result:?}"
        );
    }

    #[tokio::test]
    async fn https_cert_without_key_errors() {
        let (cert, _key) = gen_cert();
        let manager = Arc::new(ImposterManager::new());
        let mut cfg = stub_config(19846, "https", None);
        cfg["cert"] = serde_json::json!(cert); // cert present, key absent
        let result = manager
            .create_imposter(serde_json::from_value(cfg).unwrap())
            .await;
        assert!(
            matches!(
                result,
                Err(rift_http_proxy::imposter::ImposterError::Tls(_))
            ),
            "cert without key must error (both-or-neither), got {result:?}"
        );
    }

    #[tokio::test]
    async fn https_partial_server_default_errors() {
        // Only a default cert (operator forgot --default-tls-key) must error, NOT silently
        // downgrade to self-signed.
        let (cert, _key) = gen_cert();
        let manager = Arc::new(ImposterManager::new().with_tls_defaults(TlsDefaults {
            default_cert: Some(cert),
            default_key: None,
            allow_self_signed: true,
        }));
        let result = manager
            .create_imposter(serde_json::from_value(stub_config(19847, "https", None)).unwrap())
            .await;
        assert!(
            matches!(
                result,
                Err(rift_http_proxy::imposter::ImposterError::Tls(_))
            ),
            "half-configured server default must error, not downgrade to self-signed, got {result:?}"
        );
    }

    #[tokio::test]
    async fn https_mismatched_valid_pair_never_serves_cleartext() {
        // cert from one keypair, key from another — both individually valid. rustls does not
        // cross-validate at build time, so this may bind; the TLS handshake must then fail and the
        // client must NEVER receive a normal response. Either outcome (creation error OR handshake
        // failure) is acceptable; serving the stub would not be (criterion 6 + no silent cleartext).
        let (cert_a, _key_a) = gen_cert();
        let (_cert_b, key_b) = gen_cert();
        let manager = Arc::new(ImposterManager::new());
        let mut cfg = stub_config(19848, "https", None);
        cfg["cert"] = serde_json::json!(cert_a);
        cfg["key"] = serde_json::json!(key_b);

        match manager
            .create_imposter(serde_json::from_value(cfg).unwrap())
            .await
        {
            Err(rift_http_proxy::imposter::ImposterError::Tls(_)) => {} // caught at creation — ideal
            Ok(port) => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let r = tls_client()
                    .get(format!("https://127.0.0.1:{port}/x"))
                    .send()
                    .await;
                assert!(
                    r.is_err(),
                    "mismatched cert/key must fail the TLS handshake, never serve a response"
                );
                let _ = manager.delete_imposter(port).await;
            }
            Err(other) => panic!("unexpected non-Tls error: {other:?}"),
        }
    }
}

// Issue #239: _rift.fault.tcp must produce a REAL client-observable transport failure (not a 502).
mod tcp_faults {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    async fn fault_imposter(port: u16, kind: &str) -> Arc<ImposterManager> {
        let manager = Arc::new(ImposterManager::new());
        let config = serde_json::from_value(serde_json::json!({
            "port": port, "protocol": "http",
            "stubs": [{"responses": [{
                "is": {"statusCode": 200, "body": "should-never-be-seen"},
                "_rift": {"fault": {"tcp": kind}}
            }]}]
        }))
        .unwrap();
        manager
            .create_imposter(config)
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;
        manager
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Observed {
        /// The request failed before a valid HTTP response (reset / empty close / bad framing).
        SendFailed,
        /// Response headers parsed (real status line) but the body read failed.
        BodyFailed,
        /// A complete, normal HTTP response — the fault did NOT fire (regression).
        FullResponse,
    }

    /// What an HTTP client observes against the imposter — used to assert that a real transport
    /// failure occurred and to distinguish the fault kinds (never `FullResponse`).
    async fn observe(port: u16) -> Observed {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        match client
            .get(format!("http://127.0.0.1:{port}/x"))
            .send()
            .await
        {
            Err(_) => Observed::SendFailed,
            Ok(resp) => match resp.bytes().await {
                Err(_) => Observed::BodyFailed,
                Ok(_) => Observed::FullResponse,
            },
        }
    }

    #[tokio::test]
    async fn tcp_fault_reset_is_real() {
        let manager = fault_imposter(19830, "CONNECTION_RESET_BY_PEER").await;
        assert_eq!(
            observe(19830).await,
            Observed::SendFailed,
            "reset must fail the request at the transport layer, not return a 502"
        );
        let _ = manager.delete_imposter(19830).await;
    }

    #[tokio::test]
    async fn tcp_fault_empty_response_is_real() {
        let manager = fault_imposter(19831, "EMPTY_RESPONSE").await;
        assert_eq!(
            observe(19831).await,
            Observed::SendFailed,
            "empty-response must close with no response, failing the request"
        );
        let _ = manager.delete_imposter(19831).await;
    }

    #[tokio::test]
    async fn tcp_fault_malformed_chunk_is_real() {
        let manager = fault_imposter(19832, "MALFORMED_RESPONSE_CHUNK").await;
        // Distinct signal: the real status line parses (send succeeds) but the malformed chunked
        // body fails to decode — proves this kind is not collapsed into reset/empty.
        assert_eq!(
            observe(19832).await,
            Observed::BodyFailed,
            "malformed-chunk must deliver a status line then fail the body read"
        );
        let _ = manager.delete_imposter(19832).await;
    }

    #[tokio::test]
    async fn tcp_fault_random_data_is_real() {
        let manager = fault_imposter(19833, "RANDOM_DATA_THEN_CLOSE").await;
        assert_eq!(
            observe(19833).await,
            Observed::SendFailed,
            "random-data must fail HTTP parsing, not return a normal response"
        );
        let _ = manager.delete_imposter(19833).await;
    }
}

// Issue #238: multi-value header support on the served response and the recorded request.
mod multi_value_headers {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    async fn serve(config: serde_json::Value) -> Arc<ImposterManager> {
        let manager = Arc::new(ImposterManager::new());
        manager
            .create_imposter(serde_json::from_value(config).unwrap())
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;
        manager
    }

    #[tokio::test]
    async fn serves_both_values_of_a_response_header() {
        let manager = serve(serde_json::json!({
            "port": 19820, "protocol": "http",
            "stubs": [{"responses": [{"is": {"statusCode": 200,
                "headers": {"Set-Cookie": ["a=1", "b=2"]}, "body": "ok"}}]}]
        }))
        .await;

        let resp = reqwest::get("http://127.0.0.1:19820/x").await.unwrap();
        let cookies: Vec<String> = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            cookies.len(),
            2,
            "both Set-Cookie lines served, got {cookies:?}"
        );
        assert!(cookies.contains(&"a=1".to_string()) && cookies.contains(&"b=2".to_string()));

        let _ = manager.delete_imposter(19820).await;
    }

    #[tokio::test]
    async fn default_response_serves_both_values_of_a_header() {
        // No stub matches → the imposter's defaultResponse is served (a separate emission site).
        let manager = serve(serde_json::json!({
            "port": 19823, "protocol": "http",
            "defaultResponse": {"statusCode": 200,
                "headers": {"Set-Cookie": ["a=1", "b=2"]}, "body": "def"},
            "stubs": []
        }))
        .await;

        let resp = reqwest::get("http://127.0.0.1:19823/nomatch")
            .await
            .unwrap();
        let cookies: Vec<String> = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            cookies.len(),
            2,
            "defaultResponse serves both Set-Cookie lines, got {cookies:?}"
        );
        assert!(cookies.contains(&"a=1".to_string()) && cookies.contains(&"b=2".to_string()));

        let _ = manager.delete_imposter(19823).await;
    }

    #[tokio::test]
    async fn single_value_response_header_still_works() {
        let manager = serve(serde_json::json!({
            "port": 19821, "protocol": "http",
            "stubs": [{"responses": [{"is": {"statusCode": 200,
                "headers": {"X-Custom": "v"}, "body": "ok"}}]}]
        }))
        .await;

        let resp = reqwest::get("http://127.0.0.1:19821/x").await.unwrap();
        assert_eq!(resp.headers().get("x-custom").unwrap(), "v");
        let _ = manager.delete_imposter(19821).await;
    }

    #[tokio::test]
    async fn records_both_values_of_a_request_header() {
        let manager = serve(serde_json::json!({
            "port": 19822, "protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "ok"}}]}]
        }))
        .await;

        reqwest::Client::new()
            .get("http://127.0.0.1:19822/x")
            .header("X-Multi", "one")
            .header("X-Multi", "two")
            .send()
            .await
            .unwrap();

        let admin = "127.0.0.1:12720";
        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            admin.parse().unwrap(),
            manager.clone(),
            None,
        );
        tokio::spawn(server.run());
        tokio::time::sleep(Duration::from_millis(200)).await;

        let recorded = json(
            &reqwest::Client::new(),
            format!("http://{admin}/imposters/19822/requests"),
        )
        .await;
        let values: Vec<String> = recorded[0]["headers"]["X-Multi"]
            .as_array()
            .expect("multi-value header serialized as array")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            values.contains(&"one".to_string()) && values.contains(&"two".to_string()),
            "both request header values recorded, got {values:?}"
        );

        let _ = manager.delete_imposter(19822).await;
    }
}

// Issue #197: POST /admin/reload re-reads the config source and replaces imposters.
// Several tests still seed state through the deprecated `reload()` on purpose — it is
// kept working alongside `apply_config` (issue #316).
#[allow(deprecated)]
mod reload {
    use super::*;
    use rift_http_proxy::config_loader::{ConfigSource, load_configs};

    fn cfg(port: u16, body: &str) -> String {
        format!(
            r#"{{"imposters":[{{"port":{port},"protocol":"http","stubs":[
                {{"predicates":[{{"equals":{{"path":"/p"}}}}],
                 "responses":[{{"is":{{"statusCode":200,"body":"{body}"}}}}]}}]}}]}}"#
        )
    }

    fn stub_body(manager: &ImposterManager, port: u16) -> String {
        let stubs = manager.get_imposter(port).unwrap().get_stubs();
        serde_json::to_value(&stubs[0]).unwrap()["responses"][0]["is"]["body"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn start(port: u16, src: Option<ConfigSource>) -> std::sync::Arc<ImposterManager> {
        let manager = std::sync::Arc::new(ImposterManager::new());
        let mut server = rift_http_proxy::admin_api::AdminApiServer::new(
            format!("127.0.0.1:{port}").parse().unwrap(),
            manager.clone(),
            None,
        );
        if let Some(src) = src {
            server = server.with_config_source(src);
        }
        tokio::spawn(server.run());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        manager
    }

    #[tokio::test]
    async fn reload_replaces_imposters_from_configfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("imposters.json");
        std::fs::write(&path, cfg(19790, "v1")).unwrap();
        let source = ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        };

        let manager = start(12597, Some(source.clone())).await;
        manager
            .reload(load_configs(&source).unwrap())
            .await
            .unwrap();
        assert_eq!(stub_body(&manager, 19790), "v1");

        // change the file on disk, then reload via the admin endpoint
        std::fs::write(&path, cfg(19790, "v2")).unwrap();
        let resp = reqwest::Client::new()
            .post("http://127.0.0.1:12597/admin/reload")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            stub_body(&manager, 19790),
            "v2",
            "reload picked up the file change"
        );
        // the listener was actually re-bound on the same port and now serves v2 over the wire
        let served = reqwest::get("http://127.0.0.1:19790/p")
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(served, "v2", "reloaded imposter serves the new content");

        let _ = manager.delete_imposter(19790).await;
    }

    #[tokio::test]
    async fn reload_semantic_error_keeps_existing_imposters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("imposters.json");
        std::fs::write(&path, cfg(19792, "good")).unwrap();
        let source = ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        };

        let manager = start(12600, Some(source.clone())).await;
        manager
            .reload(load_configs(&source).unwrap())
            .await
            .unwrap();
        assert_eq!(stub_body(&manager, 19792), "good");

        // parses fine, but the protocol is invalid → must be rejected BEFORE delete_all,
        // leaving the running imposter intact (the destructive partial-teardown guard)
        std::fs::write(
            &path,
            r#"{"imposters":[{"port":19792,"protocol":"tcp","stubs":[]}]}"#,
        )
        .unwrap();
        let resp = reqwest::Client::new()
            .post("http://127.0.0.1:12600/admin/reload")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 500);
        assert_eq!(
            stub_body(&manager, 19792),
            "good",
            "a semantically-invalid config must not tear down the running imposters"
        );

        let _ = manager.delete_imposter(19792).await;
    }

    #[tokio::test]
    async fn reload_parse_error_keeps_existing_imposters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("imposters.json");
        std::fs::write(&path, cfg(19791, "good")).unwrap();
        let source = ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        };

        let manager = start(12598, Some(source.clone())).await;
        manager
            .reload(load_configs(&source).unwrap())
            .await
            .unwrap();
        assert_eq!(stub_body(&manager, 19791), "good");

        // corrupt the file → reload must 500 and leave the running imposter intact
        std::fs::write(&path, "{ not valid json").unwrap();
        let resp = reqwest::Client::new()
            .post("http://127.0.0.1:12598/admin/reload")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 500);
        assert_eq!(
            stub_body(&manager, 19791),
            "good",
            "existing imposter unchanged on parse error"
        );

        let _ = manager.delete_imposter(19791).await;
    }

    fn two_imposter_cfg(keep_body: &str, sibling_body: &str) -> String {
        format!(
            r#"{{"imposters":[
                {{"port":19475,"protocol":"http","recordRequests":true,"stubs":[
                    {{"predicates":[{{"equals":{{"path":"/p"}}}}],
                     "responses":[{{"is":{{"statusCode":200,"body":"{keep_body}"}}}}]}}]}},
                {{"port":19476,"protocol":"http","stubs":[
                    {{"predicates":[{{"equals":{{"path":"/p"}}}}],
                     "responses":[{{"is":{{"statusCode":200,"body":"{sibling_body}"}}}}]}}]}}]}}"#
        )
    }

    // Issue #316: reload reconciles incrementally — an untouched imposter keeps its
    // runtime state (recorded requests, listener) while a sibling is modified.
    #[tokio::test]
    async fn reload_preserves_untouched_imposter_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("imposters.json");
        std::fs::write(&path, two_imposter_cfg("keep", "v1")).unwrap();
        let source = ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        };

        let manager = start(12601, Some(source.clone())).await;
        manager
            .apply_config(load_configs(&source).unwrap())
            .await
            .unwrap();

        // Drive the untouched imposter so it accrues runtime state.
        let served = reqwest::get("http://127.0.0.1:19475/p")
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(served, "keep");
        let before = manager.get_imposter(19475).unwrap();
        assert_eq!(before.get_recorded_requests().len(), 1);

        // Change only the sibling on disk, then reload via the admin endpoint.
        std::fs::write(&path, two_imposter_cfg("keep", "v2")).unwrap();
        let resp = reqwest::Client::new()
            .post("http://127.0.0.1:12601/admin/reload")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .starts_with("Reloaded"),
            "message stays backward-compatible, got: {body}"
        );
        assert_eq!(
            body["replaced"],
            serde_json::json!([19476]),
            "single-stub rewrite is a degenerate patch, so the sibling is replaced wholesale"
        );

        let after = manager.get_imposter(19475).unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&before, &after),
            "reload must not recreate an unchanged imposter"
        );
        assert_eq!(
            after.get_recorded_requests().len(),
            1,
            "reload must not reset an unchanged imposter's recorded requests"
        );
        assert_eq!(stub_body(&manager, 19476), "v2");

        manager.delete_all().await;
    }

    #[tokio::test]
    async fn reload_with_no_source_is_noop_200() {
        let manager = start(12599, None).await;
        let resp = reqwest::Client::new()
            .post("http://127.0.0.1:12599/admin/reload")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = manager;
    }
}

// Issue #195: {{DAYS+N}}/{{NOW}} date templates are expanded in served stub bodies.
mod date_templates {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn served_is_body_resolves_date_template() {
        let manager = Arc::new(ImposterManager::new());
        manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "port": 19860, "protocol": "http",
                    "stubs": [{"responses": [{"is": {"statusCode": 200,
                        "body": "{\"exp\":\"{{DAYS+5}}\",\"now\":\"{{NOW}}\"}"}}]}]
                }))
                .unwrap(),
            )
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let body = reqwest::get("http://127.0.0.1:19860/x")
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            !body.contains("{{DAYS+5}}") && !body.contains("{{NOW}}"),
            "date templates must be expanded in the served body, got {body}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let exp = parsed["exp"].as_str().unwrap();
        let expected = (chrono::Utc::now() + chrono::Duration::days(5)).date_naive();
        let got = chrono::DateTime::parse_from_rfc3339(exp)
            .unwrap()
            .date_naive();
        assert_eq!(got, expected, "{{DAYS+5}} resolved to the wrong date");

        let _ = manager.delete_imposter(19860).await;
    }

    #[tokio::test]
    async fn served_default_response_resolves_date_template() {
        let manager = Arc::new(ImposterManager::new());
        manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "port": 19861, "protocol": "http",
                    "defaultResponse": {"statusCode": 200, "body": "{\"d\":\"{{DAYS+3}}\"}"},
                    "stubs": []
                }))
                .unwrap(),
            )
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let body = reqwest::get("http://127.0.0.1:19861/nomatch")
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            !body.contains("{{DAYS+3}}"),
            "defaultResponse body must expand date templates too, got {body}"
        );
        let _ = manager.delete_imposter(19861).await;
    }
}

// Issue #212: reach imposters through the single admin port via `/__rift/:port/<path>`.
mod gateway {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// Start an imposter (with the given stubs) + an AdminApiServer; returns (manager, admin_base).
    async fn setup(
        imposter_port: u16,
        admin_port: u16,
        stubs: serde_json::Value,
    ) -> (Arc<ImposterManager>, String) {
        let manager = Arc::new(ImposterManager::new());
        manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "port": imposter_port, "protocol": "http", "stubs": stubs
                }))
                .unwrap(),
            )
            .await
            .expect("create imposter");
        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            format!("127.0.0.1:{admin_port}").parse().unwrap(),
            manager.clone(),
            None,
        );
        tokio::spawn(server.run());
        tokio::time::sleep(Duration::from_millis(200)).await;
        (manager, format!("http://127.0.0.1:{admin_port}"))
    }

    #[tokio::test]
    async fn gateway_routes_to_imposter() {
        let (manager, admin) = setup(
            19850,
            12750,
            serde_json::json!([{
                "predicates": [{"equals": {"path": "/api/data"}}],
                "responses": [{"is": {"statusCode": 200, "body": "routed"}}]
            }]),
        )
        .await;

        // Hit the imposter through the admin port — the imposter must see path /api/data.
        let resp = reqwest::get(format!("{admin}/__rift/19850/api/data"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "routed");
        let _ = manager.delete_imposter(19850).await;
    }

    #[tokio::test]
    async fn gateway_preserves_method_and_query() {
        let (manager, admin) = setup(
            19851,
            12751,
            serde_json::json!([{
                "predicates": [{"equals": {"method": "POST", "path": "/submit", "query": {"q": "1"}}}],
                "responses": [{"is": {"statusCode": 201, "body": "posted"}}]
            }]),
        )
        .await;

        let resp = reqwest::Client::new()
            .post(format!("{admin}/__rift/19851/submit?q=1"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "method + query must reach the imposter");
        assert_eq!(resp.text().await.unwrap(), "posted");
        let _ = manager.delete_imposter(19851).await;
    }

    #[tokio::test]
    async fn gateway_unknown_port_404() {
        let (manager, admin) = setup(
            19852,
            12752,
            serde_json::json!([{"responses": [{"is": {"statusCode": 200, "body": "x"}}]}]),
        )
        .await;
        let resp = reqwest::get(format!("{admin}/__rift/59999/anything"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "no imposter on that port → 404");
        let _ = manager.delete_imposter(19852).await;
    }

    #[tokio::test]
    async fn gateway_forwards_post_body() {
        let (manager, admin) = setup(
            19854,
            12754,
            serde_json::json!([{
                "predicates": [{"equals": {"method": "POST", "path": "/echo", "body": "hello-body"}}],
                "responses": [{"is": {"statusCode": 200, "body": "got-body"}}]
            }]),
        )
        .await;
        let resp = reqwest::Client::new()
            .post(format!("{admin}/__rift/19854/echo"))
            .body("hello-body")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.text().await.unwrap(),
            "got-body",
            "the POST body must reach the imposter through the gateway"
        );
        let _ = manager.delete_imposter(19854).await;
    }

    #[tokio::test]
    async fn gateway_no_subpath_routes_to_root() {
        let (manager, admin) = setup(
            19855,
            12755,
            serde_json::json!([{
                "predicates": [{"equals": {"path": "/"}}],
                "responses": [{"is": {"statusCode": 200, "body": "root"}}]
            }]),
        )
        .await;
        let resp = reqwest::get(format!("{admin}/__rift/19855")).await.unwrap();
        assert_eq!(
            resp.text().await.unwrap(),
            "root",
            "no sub-path → imposter root"
        );
        let _ = manager.delete_imposter(19855).await;
    }

    #[tokio::test]
    async fn gateway_non_numeric_port_400() {
        let (manager, admin) = setup(
            19856,
            12756,
            serde_json::json!([{"responses": [{"is": {"statusCode": 200, "body": "x"}}]}]),
        )
        .await;
        let resp = reqwest::get(format!("{admin}/__rift/notaport/x"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "non-numeric port is a malformed gateway target → 400, distinct from 404"
        );
        let _ = manager.delete_imposter(19856).await;
    }

    #[tokio::test]
    async fn gateway_not_gated_by_admin_apikey() {
        // The gateway is data-plane imposter traffic: it must work WITHOUT the admin key, even
        // when --apikey is set (otherwise the app-under-test would need the admin key).
        let manager = Arc::new(ImposterManager::new());
        manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "port": 19857, "protocol": "http",
                    "stubs": [{"predicates": [{"equals": {"path": "/x"}}],
                              "responses": [{"is": {"statusCode": 200, "body": "open"}}]}]
                }))
                .unwrap(),
            )
            .await
            .expect("create imposter");
        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:12757".parse().unwrap(),
            manager.clone(),
            Some("secret".to_string()),
        );
        tokio::spawn(server.run());
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Admin route without the key → 401 (control plane stays protected).
        let admin_resp = reqwest::get("http://127.0.0.1:12757/imposters")
            .await
            .unwrap();
        assert_eq!(
            admin_resp.status(),
            401,
            "admin route still requires the apikey"
        );

        // Gateway without the key → serves (data plane is not gated).
        let gw = reqwest::get("http://127.0.0.1:12757/__rift/19857/x")
            .await
            .unwrap();
        assert_eq!(gw.status(), 200);
        assert_eq!(
            gw.text().await.unwrap(),
            "open",
            "gateway works without the admin key"
        );
        let _ = manager.delete_imposter(19857).await;
    }
}

// Issue #260: GET /imposters/:port exposes the imposter's flowState (so tools can read
// flowIdSource), with the redis block redacted.
#[tokio::test]
async fn get_imposter_exposes_flowstate_redacted() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    // inmemory backend ignores a stray `redis` block (no connection attempted) — include one with a
    // credentialed URL to prove the GET projection actually strips it end-to-end.
    let config = serde_json::from_value(serde_json::json!({
        "port": 19771, "protocol": "http",
        "_rift": { "flowState": { "backend": "inmemory", "ttlSeconds": 300,
            "redis": { "url": "redis://user:topsecret@host:6379" },
            "flowIdSource": "header:X-Mock-Space" } },
        "stubs": [{ "predicates": [{ "equals": { "path": "/x" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }]
    }))
    .unwrap();
    manager.create_imposter(config).await.expect("create");

    let admin_addr = "127.0.0.1:12596".parse().unwrap();
    let server = rift_http_proxy::admin_api::AdminApiServer::new(admin_addr, manager.clone(), None);
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let c = reqwest::Client::new();
    let v = json(&c, "http://127.0.0.1:12596/imposters/19771".to_string()).await;
    assert_eq!(
        v.pointer("/_rift/flowState/flowIdSource")
            .and_then(|x| x.as_str()),
        Some("header:X-Mock-Space"),
        "GET must expose flat flowIdSource so rift-verify can drive correlated isolation: {v}"
    );
    assert!(
        v.pointer("/_rift/flowState/redis").is_none(),
        "redis config must be redacted from the exposed flowState: {v}"
    );
    assert!(
        !v.to_string().contains("topsecret"),
        "the redis credential must not survive anywhere in the GET response"
    );
}
