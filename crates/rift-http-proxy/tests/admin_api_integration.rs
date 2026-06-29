//! Admin HTTP API integration tests (relocated from rift-core in issue #203, since they spin up
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
        flow_state["mountebankStateMapping"] = serde_json::json!({ "flowIdSource": src });
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
            "mountebankStateMapping": { "flowIdSource": "header:X-Mock-Space" } } },
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
mod reload {
    use super::*;
    use rift_http_proxy::config_loader::{load_configs, ConfigSource};

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
