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
async fn bind_admin(server: rift_http_proxy::admin_api::AdminApiServer) -> String {
    let running = server.bind().await.expect("admin API binds");
    format!("http://{}", running.local_addr())
}
async fn get(c: &reqwest::Client, port: u16, path: &str, space: Option<&str>) -> reqwest::Response {
    let mut req = c.get(format!("http://127.0.0.1:{port}{path}"));
    if let Some(s) = space {
        req = req.header("X-Mock-Space", s);
    }
    req.send().await.expect("send")
}

fn order_fsm(flow_id_source: Option<&str>) -> serde_json::Value {
    let mut flow_state = serde_json::json!({ "ttlSeconds": 300 });
    if let Some(src) = flow_id_source {
        flow_state["flowIdSource"] = serde_json::json!(src);
    }
    serde_json::json!({
        "protocol": "http",
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

fn correlated_config(stubs: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "protocol": "http", "recordRequests": true,
        "_rift": { "flowState": { "ttlSeconds": 300,
            "flowIdSource": "header:X-Mock-Space" } },
        "stubs": stubs
    })
}

#[tokio::test]
async fn scenario_admin_endpoints_arrange_inspect_reset() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(order_fsm(None)).unwrap();
    let port = manager.create_imposter(config).await.expect("create");

    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;

    let c = reqwest::Client::new();

    // GET scenarios → initial "Started"
    let v = json(&c, format!("{admin}/imposters/{port}/scenarios")).await;
    assert_eq!(v["scenarios"][0]["name"], "order");
    assert_eq!(v["scenarios"][0]["state"], "Started");

    // PUT state=paid → a subsequent request observes it
    let r = c
        .put(format!("{admin}/imposters/{port}/scenarios/order/state"))
        .header("content-type", "application/json")
        .body(r#"{"state":"paid"}"#)
        .send()
        .await
        .expect("put");
    assert_eq!(r.status(), 200);
    assert_eq!(
        text(&c, format!("http://127.0.0.1:{port}/status")).await,
        "paid"
    );

    // GET reflects the transition
    let v = json(&c, format!("{admin}/imposters/{port}/scenarios")).await;
    assert_eq!(v["scenarios"][0]["state"], "paid");

    // reset → back to initial
    let r = c
        .post(format!("{admin}/imposters/{port}/scenarios/reset"))
        .send()
        .await
        .expect("reset");
    assert_eq!(r.status(), 200);
    assert_eq!(
        text(&c, format!("http://127.0.0.1:{port}/status")).await,
        "unpaid"
    );

    // flow-state KV: PUT / GET / DELETE (default flow_id = imposter_port = "<port>")
    let kv = format!("{admin}/admin/imposters/{port}/flow-state/{port}/mykey");
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

    let _ = manager.delete_imposter(port).await;
}

// Issue #530: DELETE /admin/imposters/:port/flow-state/:flow_id clears every key in the flow.
#[tokio::test]
async fn delete_flow_state_clears_whole_flow() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(order_fsm(None)).unwrap();
    let imposter_port = manager.create_imposter(config).await.expect("create");

    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;

    let c = reqwest::Client::new();
    let key_url = |k: &str| format!("{admin}/admin/imposters/{imposter_port}/flow-state/flowX/{k}");

    // Arrange two keys under flow "flowX".
    for (k, v) in [("a", "1"), ("b", "2")] {
        let r = c
            .put(key_url(k))
            .header("content-type", "application/json")
            .body(format!(r#"{{"value":{v}}}"#))
            .send()
            .await
            .expect("put");
        assert_eq!(r.status(), 200);
    }

    // Clear the whole flow.
    let r = c
        .delete(format!(
            "{admin}/admin/imposters/{imposter_port}/flow-state/flowX"
        ))
        .send()
        .await
        .expect("clear");
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["cleared"],
        true
    );

    // Both keys are gone.
    assert_eq!(c.get(key_url("a")).send().await.unwrap().status(), 404);
    assert_eq!(c.get(key_url("b")).send().await.unwrap().status(), 404);

    // Idempotent: clearing again still succeeds; an unknown port is 404.
    assert_eq!(
        c.delete(format!(
            "{admin}/admin/imposters/{imposter_port}/flow-state/flowX"
        ))
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    assert_eq!(
        c.delete(format!("{admin}/admin/imposters/29999/flow-state/flowX"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    let _ = manager.delete_imposter(imposter_port).await;
}

// Issue #1463: `flowState.backend` is gone. A config that still carries one (even the removed
// `"redis"`) is not refused: the key decodes into `extra`, selects nothing, and the imposter gets
// the in-memory store.
#[tokio::test]
async fn create_imposter_with_a_leftover_backend_key_is_accepted() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;

    let r = reqwest::Client::new()
        .post(format!("{admin}/imposters"))
        .json(&serde_json::json!({
            "port": 0, "protocol": "http", "stubs": [],
            "_rift": { "flowState": { "backend": "redis",
                "redis": { "url": "redis://127.0.0.1:6379" } } }
        }))
        .send()
        .await
        .expect("post imposter");
    assert_eq!(
        r.status(),
        201,
        "a leftover flowState.backend must not be refused"
    );

    manager.delete_all().await;
}

// Issue #530: creating an imposter with flowState.ttlSeconds < 1 is rejected with 400.
#[tokio::test]
async fn create_imposter_rejects_non_positive_ttl_seconds() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;

    let c = reqwest::Client::new();
    let r = c
        .post(format!("{admin}/imposters"))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "protocol": "http", "stubs": [],
                "_rift": { "flowState": { "ttlSeconds": 0 } }
            })
            .to_string(),
        )
        .send()
        .await
        .expect("post");
    assert_eq!(r.status(), 400, "ttlSeconds:0 must be rejected at creation");
    assert!(r.text().await.unwrap().contains("ttlSeconds"));
}

// Issue #1304: `PUT /imposters` carries the apply report beside `imposters` on success.
#[tokio::test]
async fn put_imposters_success_carries_the_apply_report() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;

    let r = reqwest::Client::new()
        .put(format!("{admin}/imposters"))
        .header("content-type", "application/json")
        .body(r#"{"imposters":[{"protocol":"http","stubs":[]}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    let imposter_port = body["imposters"][0]["port"]
        .as_u64()
        .unwrap_or_else(|| panic!("the assigned port: {body}"));
    assert_ne!(imposter_port, 0, "{body}");
    assert_eq!(
        body["created"],
        serde_json::json!([imposter_port]),
        "{body}"
    );
    assert_eq!(body["toggled"], serde_json::json!([]), "{body}");

    manager.delete_all().await;
}

// Issue #546: a request body past the body ceiling is rejected with 413 by the live admin route,
// proving the limit is wired into the real request path (not just the collect_body unit) — the
// size check fires before JSON parsing. Since #1439 the ceiling is the manager's configurable
// limit, so a small one keeps the test cheap; one byte over keeps the 413 from becoming a reset.
#[tokio::test]
async fn oversized_admin_body_is_rejected_with_413() {
    const LIMIT: usize = 64 * 1024;
    let manager = std::sync::Arc::new(ImposterManager::new().with_body_limits(
        rift_http_proxy::imposter::body_limits::BodyLimits::new(LIMIT, None).expect("valid"),
    ));
    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;

    let c = reqwest::Client::new();
    let r = c
        .post(format!("{admin}/imposters"))
        .header("content-type", "application/json")
        .body(vec![b'a'; LIMIT + 1])
        .send()
        .await
        .expect("post");
    assert_eq!(
        r.status(),
        413,
        "a body over the ceiling must be rejected with 413 Payload Too Large"
    );
}

#[tokio::test]
async fn scenario_admin_reset_is_per_flow_with_explicit_flow_id() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(order_fsm(Some("header:X-Mock-Space"))).unwrap();
    let port = manager.create_imposter(config).await.expect("create");

    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;

    let c = reqwest::Client::new();
    let set_state = |flow: &str| {
        c.put(format!("{admin}/imposters/{port}/scenarios/order/state"))
            .header("content-type", "application/json")
            .body(format!(r#"{{"state":"paid","flowId":"{flow}"}}"#))
            .send()
    };
    // Arrange both flows to "paid" via explicit flowId
    assert_eq!(set_state("alpha").await.expect("a").status(), 200);
    assert_eq!(set_state("beta").await.expect("b").status(), 200);

    // Reset ONLY alpha
    let r = c
        .post(format!("{admin}/imposters/{port}/scenarios/reset"))
        .header("content-type", "application/json")
        .body(r#"{"flowId":"alpha"}"#)
        .send()
        .await
        .expect("reset");
    assert_eq!(r.status(), 200);

    // alpha back to initial; beta untouched — via GET ?flowId=
    let a = json(
        &c,
        format!("{admin}/imposters/{port}/scenarios?flowId=alpha"),
    )
    .await;
    let b = json(
        &c,
        format!("{admin}/imposters/{port}/scenarios?flowId=beta"),
    )
    .await;
    assert_eq!(
        a["scenarios"][0]["state"], "Started",
        "alpha reset to initial"
    );
    assert_eq!(
        b["scenarios"][0]["state"], "paid",
        "beta untouched by alpha reset"
    );

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn space_teardown_is_isolated() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(correlated_config(serde_json::json!([
        { "space": "alpha", "predicates": [{ "equals": { "path": "/data" } }],
          "responses": [{ "is": { "statusCode": 200, "body": "ALPHA" } }] },
        { "space": "beta", "predicates": [{ "equals": { "path": "/data" } }],
          "responses": [{ "is": { "statusCode": 200, "body": "BETA" } }] }
    ])))
    .unwrap();
    let port = manager.create_imposter(config).await.expect("create");

    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;
    let c = reqwest::Client::new();

    // record one request per space
    let _ = get(&c, port, "/data", Some("alpha")).await;
    let _ = get(&c, port, "/data", Some("beta")).await;

    // tear down alpha only
    let r = c
        .delete(format!("{admin}/imposters/{port}/spaces/alpha"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // alpha's recorded requests are cleared (check BEFORE any new alpha request below,
    // which would re-record one).
    let alpha_reqs = c
        .get(format!(
            "{admin}/imposters/{port}/requests?match=header:X-Mock-Space=alpha"
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(alpha_reqs, "[]", "alpha recorded requests cleared");
    // alpha's stub no longer matches /data
    let alpha_after = get(&c, port, "/data", Some("alpha"))
        .await
        .text()
        .await
        .unwrap();
    assert_ne!(alpha_after, "ALPHA", "alpha stubs removed");

    // beta is fully intact
    assert_eq!(
        get(&c, port, "/data", Some("beta"))
            .await
            .text()
            .await
            .unwrap(),
        "BETA",
        "beta untouched"
    );
    let beta_space = c
        .get(format!("{admin}/imposters/{port}/spaces/beta/stubs"))
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

    let _ = manager.delete_imposter(port).await;
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
    let config = serde_json::from_value(correlated_config(serde_json::json!([
        fsm("alpha"),
        fsm("beta")
    ])))
    .unwrap();
    let port = manager.create_imposter(config).await.expect("create");

    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;
    let c = reqwest::Client::new();
    let state_url = |flow: &str| format!("{admin}/admin/imposters/{port}/flow-state/{flow}/order");

    // advance both spaces' "order" scenario to paid
    let _ = get(&c, port, "/pay", Some("alpha")).await;
    let _ = get(&c, port, "/pay", Some("beta")).await;
    assert_eq!(
        c.get(state_url("alpha")).send().await.unwrap().status(),
        200,
        "alpha order state set before teardown"
    );

    // tear down alpha
    let r = c
        .delete(format!("{admin}/imposters/{port}/spaces/alpha"))
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

    let _ = manager.delete_imposter(port).await;
}

/// Issue #336: a body that is not a stub must be refused, not turned into a catch-all.
///
/// Before this, `Stub`'s every-field-`default` deserialization meant *any* JSON object was a valid
/// stub, so a typo produced `201` and installed the worst stub the engine can hold: no predicates
/// (matches everything in the space, shadowing the stubs meant to answer), no responses (serves a
/// default nobody wrote), and no id (the console cannot address it to delete it). Recovery was
/// tearing down the whole space.
///
/// The `{"stub": …}` case is called out separately because it is how the bug is actually reached:
/// the sibling route `POST /imposters/:port/stubs` requires that envelope and this one does not, so
/// posting the documented shape of one at the other is an ordinary mistake with a catastrophic
/// outcome. It gets an error that names the mistake rather than one that sends the operator
/// re-reading a stub that was never at fault.
#[tokio::test]
async fn a_space_stub_body_that_is_not_a_stub_is_refused() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(correlated_config(serde_json::json!([]))).unwrap();
    let imposter_port = manager.create_imposter(config).await.expect("create");

    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;
    let c = reqwest::Client::new();
    let post = |body: &'static str| {
        c.post(format!(
            "{admin}/imposters/{imposter_port}/spaces/alpha/stubs"
        ))
        .header("content-type", "application/json")
        .body(body)
        .send()
    };

    // The exact body from the report.
    let r = post(r#"{"complete":"nonsense","zzz":123}"#).await.unwrap();
    assert_eq!(r.status(), 400, "a non-stub body must not be accepted");
    let text = r.text().await.unwrap();
    assert!(
        text.contains("not a stub") && text.contains("predicates"),
        "the refusal must say what was expected: {text}"
    );

    // The envelope mistake, named.
    let r = post(r#"{"stub":{"predicates":[],"responses":[]}}"#)
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let text = r.text().await.unwrap();
    assert!(
        text.contains("envelope") && text.contains("/imposters/{port}/stubs"),
        "the envelope mistake must be named, not reported as a generic shape error: {text}"
    );

    // An empty object names no stub field either, so it is refused too (#1230): it is exactly the
    // no-predicate, no-response stub this rule exists to keep out. The docs once said otherwise.
    let r = post("{}").await.unwrap();
    assert_eq!(r.status(), 400, "`{{}}` is not a stub");
    let text = r.text().await.unwrap();
    assert!(text.contains("not a stub"), "{text}");

    // And nothing was installed by any attempt — the harm was never the status code, it was the
    // stub left behind.
    let listed = c
        .get(format!(
            "{admin}/imposters/{imposter_port}/spaces/alpha/stubs"
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(
        listed["stubs"].as_array().map(Vec::len),
        Some(0),
        "a refused body must leave no stub behind: {listed}"
    );

    // The rule is about shape, not emptiness: an explicitly authored space-wide default still
    // works. Rejecting this would break a legitimate use and change what a legal stub is.
    let r = post(r#"{"predicates":[],"responses":[{"is":{"statusCode":204}}]}"#)
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        201,
        "an explicit empty-predicate stub is legal Mountebank semantics and must still be accepted"
    );
}

#[tokio::test]
async fn space_stub_registration_and_inspection_endpoints() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(correlated_config(serde_json::json!([]))).unwrap();
    let port = manager.create_imposter(config).await.expect("create");

    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;
    let c = reqwest::Client::new();

    // register a stub scoped to "alpha" via the space endpoint
    let r = c
        .post(format!("{admin}/imposters/{port}/spaces/alpha/stubs"))
        .header("content-type", "application/json")
        .body(r#"{"predicates":[{"equals":{"path":"/data"}}],"responses":[{"is":{"statusCode":200,"body":"ALPHA"}}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // it matches alpha's requests and is gated from other spaces
    assert_eq!(
        get(&c, port, "/data", Some("alpha"))
            .await
            .text()
            .await
            .unwrap(),
        "ALPHA"
    );
    let beta = get(&c, port, "/data", Some("beta"))
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
        .get(format!("{admin}/imposters/{port}/spaces/alpha"))
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

    let _ = manager.delete_imposter(port).await;
}

// Issue #202: id-addressed stub operations over the admin HTTP API.
#[tokio::test]
async fn stub_by_id_admin_endpoints() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http", "stubs": []
    }))
    .unwrap();
    let imposter_port = manager.create_imposter(config).await.expect("create");

    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;
    let c = reqwest::Client::new();

    let add = |id: serde_json::Value, body: &str| {
        let stub = serde_json::json!({
            "id": id,
            "predicates": [{ "equals": { "path": "/p" } }],
            "responses": [{ "is": { "statusCode": 200, "body": body } }]
        });
        c.post(format!("{admin}/imposters/{imposter_port}/stubs"))
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
    let got: serde_json::Value = serde_json::from_str(
        &text(
            &c,
            format!("{admin}/imposters/{imposter_port}/stubs/by-id/s1"),
        )
        .await,
    )
    .unwrap();
    assert_eq!(got["id"], "s1");

    // duplicate id → 409 Conflict
    assert_eq!(
        add(serde_json::json!("s1"), "dup").await.unwrap().status(),
        409
    );

    // PUT by id replaces in place
    let put = c
        .put(format!("{admin}/imposters/{imposter_port}/stubs/by-id/s1"))
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
    let after: serde_json::Value = serde_json::from_str(
        &text(
            &c,
            format!("{admin}/imposters/{imposter_port}/stubs/by-id/s1"),
        )
        .await,
    )
    .unwrap();
    assert_eq!(
        after["responses"][0]["is"]["body"], "two",
        "PUT replaced the content"
    );

    // unknown id → 404 on GET and DELETE
    assert_eq!(
        c.get(format!(
            "{admin}/imposters/{imposter_port}/stubs/by-id/nope"
        ))
        .send()
        .await
        .unwrap()
        .status(),
        404
    );
    assert_eq!(
        c.delete(format!(
            "{admin}/imposters/{imposter_port}/stubs/by-id/nope"
        ))
        .send()
        .await
        .unwrap()
        .status(),
        404
    );

    // DELETE by id → 200, then it's gone
    assert_eq!(
        c.delete(format!("{admin}/imposters/{imposter_port}/stubs/by-id/s1"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        c.get(format!("{admin}/imposters/{imposter_port}/stubs/by-id/s1"))
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
    let imposter = manager.get_imposter(imposter_port).unwrap();
    assert!(
        imposter.get_stubs()[0].id.is_some(),
        "POST without id should generate one"
    );

    let _ = manager.delete_imposter(imposter_port).await;
}

// Issue #1334: Mountebank's imposterPrinter prints `defaultResponse` in every form (create body,
// detail, list and `?list=true`), not only `?replayable=true`.
#[tokio::test]
async fn default_response_printed_in_every_imposter_form() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;
    let c = reqwest::Client::new();

    let created = c
        .post(format!("{admin}/imposters"))
        .json(&serde_json::json!({
            "protocol": "http",
            "defaultResponse": { "statusCode": 400, "body": "nope" },
            "stubs": []
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    let created: serde_json::Value = created.json().await.expect("201 json");
    // Printed as written (issue #1369): the omitted `headers` is not invented as `{}`, in every
    // form, `?replayable=true` included.
    let expected = serde_json::json!({ "statusCode": 400, "body": "nope" });
    assert_eq!(created["defaultResponse"], expected, "201 body: {created}");
    let port = created["port"].as_u64().expect("assigned port");

    let plain = c
        .post(format!("{admin}/imposters"))
        .json(&serde_json::json!({ "protocol": "http", "stubs": [] }))
        .send()
        .await
        .expect("POST /imposters");
    let plain: serde_json::Value = plain.json().await.expect("201 json");
    assert!(
        plain.get("defaultResponse").is_none(),
        "unset defaultResponse must be absent: {plain}"
    );
    let plain_port = plain["port"].as_u64().expect("assigned port");

    let detail = json(&c, format!("{admin}/imposters/{port}")).await;
    assert_eq!(detail["defaultResponse"], expected, "detail: {detail}");
    let replayable = json(&c, format!("{admin}/imposters/{port}?replayable=true")).await;
    assert_eq!(replayable["defaultResponse"], expected);
    let plain_detail = json(&c, format!("{admin}/imposters/{plain_port}")).await;
    assert!(plain_detail.get("defaultResponse").is_none());

    for query in ["", "?list=true"] {
        let list = json(&c, format!("{admin}/imposters{query}")).await;
        let entries = list["imposters"].as_array().expect("imposters array");
        let entry = |p: u64| {
            entries
                .iter()
                .find(|e| e["port"].as_u64() == Some(p))
                .unwrap_or_else(|| panic!("port {p} missing from GET /imposters{query}: {list}"))
        };
        assert_eq!(
            entry(port)["defaultResponse"],
            expected,
            "GET /imposters{query}: {list}"
        );
        assert!(entry(plain_port).get("defaultResponse").is_none());
    }

    for p in [port, plain_port] {
        let _ = manager.delete_imposter(p as u16).await;
    }
}

// Issue #1377: `defaultResponse.statusCode` accepts a numeric string, as `is.statusCode` and
// Mountebank do. It was refused with a 400 ("expected u16"). Printing stays a number.
#[tokio::test]
async fn default_response_status_code_accepts_a_numeric_string() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;
    let c = reqwest::Client::new();

    let created = c
        .post(format!("{admin}/imposters"))
        .json(&serde_json::json!({
            "protocol": "http",
            "defaultResponse": { "statusCode": "404" },
            "stubs": []
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    let created: serde_json::Value = created.json().await.expect("201 json");
    let port = created["port"].as_u64().expect("assigned port");

    let served = c
        .get(format!("http://127.0.0.1:{port}/unmatched"))
        .send()
        .await
        .expect("request to the imposter");
    assert_eq!(served.status().as_u16(), 404);

    let detail = json(&c, format!("{admin}/imposters/{port}")).await;
    assert_eq!(
        detail["defaultResponse"]["statusCode"],
        serde_json::json!(404),
        "detail: {detail}"
    );

    let _ = manager.delete_imposter(port as u16).await;
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
        (c.cert.pem(), c.signing_key.serialize_pem())
    }

    fn stub_config(protocol: &str, cert: Option<(&str, &str)>) -> serde_json::Value {
        let mut cfg = serde_json::json!({
            "protocol": protocol,
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

    async fn serve(manager: &Arc<ImposterManager>, cfg: serde_json::Value) -> u16 {
        let port = manager
            .create_imposter(serde_json::from_value(cfg).unwrap())
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;
        port
    }

    #[tokio::test]
    async fn https_imposter_with_inline_cert_serves_tls() {
        let (cert, key) = gen_cert();
        let manager = Arc::new(ImposterManager::new());
        let port = serve(&manager, stub_config("https", Some((&cert, &key)))).await;

        let body = tls_client()
            .get(format!("https://127.0.0.1:{port}/x"))
            .send()
            .await
            .expect("TLS request should succeed")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "secure-ok");
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn http_imposter_unchanged() {
        let manager = Arc::new(ImposterManager::new());
        let port = serve(&manager, stub_config("http", None)).await;
        let body = reqwest::get(format!("http://127.0.0.1:{port}/x"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "secure-ok", "plain http imposter unaffected");
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn https_zero_config_uses_self_signed() {
        // No cert/key, default manager (self-signed enabled) → still serves over TLS.
        let manager = Arc::new(ImposterManager::new());
        let port = serve(&manager, stub_config("https", None)).await;
        let body = tls_client()
            .get(format!("https://127.0.0.1:{port}/x"))
            .send()
            .await
            .expect("zero-config https should serve via self-signed cert")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "secure-ok");
        let _ = manager.delete_imposter(port).await;
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
        let port = serve(&manager, stub_config("https", None)).await;
        let body = tls_client()
            .get(format!("https://127.0.0.1:{port}/x"))
            .send()
            .await
            .expect("server-default cert should serve the https imposter")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "secure-ok");
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn https_no_cert_and_self_signed_disabled_errors() {
        let manager = Arc::new(ImposterManager::new().with_tls_defaults(TlsDefaults {
            default_cert: None,
            default_key: None,
            allow_self_signed: false,
        }));
        let result = manager
            .create_imposter(serde_json::from_value(stub_config("https", None)).unwrap())
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
        let mut cfg = stub_config("https", None);
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
            .create_imposter(serde_json::from_value(stub_config("https", None)).unwrap())
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
        let mut cfg = stub_config("https", None);
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

    async fn fault_imposter(kind: &str) -> (Arc<ImposterManager>, u16) {
        let manager = Arc::new(ImposterManager::new());
        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": [{"responses": [{
                "is": {"statusCode": 200, "body": "should-never-be-seen"},
                "_rift": {"fault": {"tcp": kind}}
            }]}]
        }))
        .unwrap();
        let port = manager
            .create_imposter(config)
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;
        (manager, port)
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
        let (manager, port) = fault_imposter("CONNECTION_RESET_BY_PEER").await;
        assert_eq!(
            observe(port).await,
            Observed::SendFailed,
            "reset must fail the request at the transport layer, not return a 502"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn tcp_fault_empty_response_is_real() {
        let (manager, port) = fault_imposter("EMPTY_RESPONSE").await;
        assert_eq!(
            observe(port).await,
            Observed::SendFailed,
            "empty-response must close with no response, failing the request"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn tcp_fault_malformed_chunk_is_real() {
        let (manager, port) = fault_imposter("MALFORMED_RESPONSE_CHUNK").await;
        // Distinct signal: the real status line parses (send succeeds) but the malformed chunked
        // body fails to decode — proves this kind is not collapsed into reset/empty.
        assert_eq!(
            observe(port).await,
            Observed::BodyFailed,
            "malformed-chunk must deliver a status line then fail the body read"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn tcp_fault_random_data_is_real() {
        let (manager, port) = fault_imposter("RANDOM_DATA_THEN_CLOSE").await;
        assert_eq!(
            observe(port).await,
            Observed::SendFailed,
            "random-data must fail HTTP parsing, not return a normal response"
        );
        let _ = manager.delete_imposter(port).await;
    }
}

// Issue #1048: a non-ASCII UTF-8 request header, end to end.
mod non_ascii_request_headers {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::time::Duration;

    async fn serve(config: serde_json::Value) -> (Arc<ImposterManager>, u16) {
        let manager = Arc::new(ImposterManager::new());
        let port = manager
            .create_imposter(serde_json::from_value(config).unwrap())
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;
        (manager, port)
    }

    // The collector is the one decode site every request-side consumer reads, so the unit test
    // proves the decode. What this proves is the *claim a user cares about*: that such a header
    // reaches matching and the journal at all. Before #1048 `to_str` rejected every byte above
    // 0x7F, so this predicate could not match and the header was absent from `savedRequests` —
    // while the server logged that a valid UTF-8 value was "non-UTF-8".
    #[tokio::test]
    async fn a_non_ascii_utf8_header_matches_a_predicate_and_is_recorded_byte_exact() {
        let (manager, imposter_port) = serve(serde_json::json!({
            "protocol": "http", "recordRequests": true,
            "stubs": [{
                "predicates": [{"equals": {"headers": {"X-User-Name": "José"}}}],
                "responses": [{"is": {"statusCode": 200, "body": "matched"}}]
            }]
        }))
        .await;

        let body = tokio::task::spawn_blocking(move || {
            let mut sock = std::net::TcpStream::connect(format!("127.0.0.1:{imposter_port}"))
                .expect("connect");
            // Written as raw bytes, not through a client that might re-encode: the point is that
            // the UTF-8 bytes on the wire survive.
            let mut raw = b"GET /x HTTP/1.1\r\nHost: 127.0.0.1\r\nX-User-Name: ".to_vec();
            raw.extend_from_slice("José".as_bytes());
            raw.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
            sock.write_all(&raw).expect("write");
            let mut out = String::new();
            let _ = sock.read_to_string(&mut out);
            out
        })
        .await
        .expect("raw request");

        assert!(
            body.contains("matched"),
            "a predicate on a non-ASCII UTF-8 header must match; before #1048 the value was \
             dropped before matching ever saw it. got: {body}"
        );

        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:0".parse().unwrap(),
            manager.clone(),
            None,
        );
        let admin = bind_admin(server).await;

        let recorded = json(
            &reqwest::Client::new(),
            format!("{admin}/imposters/{imposter_port}/requests"),
        )
        .await;
        // A single-valued header serialises as a bare string, not a one-element array (#238), so
        // this reads the value directly.
        assert_eq!(
            recorded[0]["headers"]["X-User-Name"].as_str(),
            Some("José"),
            "the journal must record it byte-exact, not drop it: {recorded}"
        );

        let _ = manager.delete_imposter(imposter_port).await;
    }

    // The forwarding third of the claim. #1041 got a relay test for the response direction; this
    // is its request-side mirror, and without it the CHANGELOG's "matched, forwarded and recorded
    // consistently" would be prose with only two thirds of it under test. Upstream here is a
    // second, recording imposter, so its own journal is the evidence of what actually arrived.
    #[tokio::test]
    async fn a_non_ascii_utf8_header_is_forwarded_upstream_byte_exact() {
        let manager = Arc::new(ImposterManager::new());
        let port_a = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "http", "recordRequests": true,
                    "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "upstream"}}]}]
                }))
                .unwrap(),
            )
            .await
            .expect("upstream imposter");
        let port_b = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "http",
                    "stubs": [{"responses": [{"proxy": {
                        "to": format!("http://127.0.0.1:{port_a}"), "mode": "proxyAlways"
                    }}]}]
                }))
                .unwrap(),
            )
            .await
            .expect("proxy imposter");
        tokio::time::sleep(Duration::from_millis(300)).await;

        tokio::task::spawn_blocking(move || {
            let mut sock =
                std::net::TcpStream::connect(format!("127.0.0.1:{port_b}")).expect("connect");
            let mut raw = b"GET /x HTTP/1.1\r\nHost: 127.0.0.1\r\nX-User-Name: ".to_vec();
            raw.extend_from_slice("José".as_bytes());
            raw.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
            sock.write_all(&raw).expect("write");
            let mut out = String::new();
            let _ = sock.read_to_string(&mut out);
        })
        .await
        .expect("proxied request");

        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:0".parse().unwrap(),
            manager.clone(),
            None,
        );
        let admin = bind_admin(server).await;

        let upstream = json(
            &reqwest::Client::new(),
            format!("{admin}/imposters/{port_a}/requests"),
        )
        .await;
        assert_eq!(
            upstream[0]["headers"]["X-User-Name"].as_str(),
            Some("José"),
            "the upstream must receive the header byte-exact; before #1048 the collector dropped \
             it, so it never reached the forward at all: {upstream}"
        );

        let _ = manager.delete_imposter(port_b).await;
        let _ = manager.delete_imposter(port_a).await;
    }
}

// Issue #1041: the upstream proxy's response-header relay.
mod proxy_relays_response_headers_honestly {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::time::Duration;

    async fn serve(config: serde_json::Value) -> (Arc<ImposterManager>, u16) {
        let manager = Arc::new(ImposterManager::new());
        let port = manager
            .create_imposter(serde_json::from_value(config).unwrap())
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;
        (manager, port)
    }

    /// A raw-socket origin, because no ordinary server will emit a header value that is not valid
    /// UTF-8 — which is precisely the value the relay used to blank. Answers `count` connections
    /// with one fixed response, then exits.
    fn raw_origin(count: usize) -> (std::thread::JoinHandle<()>, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind origin");
        let port = listener.local_addr().expect("origin addr").port();
        let origin = std::thread::spawn(move || {
            for _ in 0..count {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                // Read the request head so the client is not answered before it finishes writing.
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf);

                let mut raw = b"HTTP/1.1 200 OK\r\nX-Ok: 1\r\nX-Bin: ".to_vec();
                raw.extend_from_slice(&[0xFF, 0xFE]);
                raw.extend_from_slice(b"\r\nContent-Disposition: attachment; filename=\"");
                raw.extend_from_slice("résumé".as_bytes());
                raw.extend_from_slice(b".pdf\"\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n");
                raw.extend_from_slice(b"Content-Length: 2\r\nConnection: close\r\n\r\nhi");
                let _ = sock.write_all(&raw);
                let _ = sock.flush();
            }
        });
        (origin, port)
    }

    #[tokio::test]
    async fn a_proxied_response_drops_undecodable_headers_and_keeps_valid_utf8_ones() {
        let (_origin, port_a) = raw_origin(1);

        let (manager, port_b) = serve(serde_json::json!({
            "protocol": "http",
            "stubs": [{"responses": [{"proxy": {
                "to": format!("http://127.0.0.1:{port_a}"), "mode": "proxyOnce",
                // `predicateGenerators` is what makes the replay come from the stub
                // `create_stub_from_proxy_response` synthesises, rather than from the
                // `RecordedResponse` lookup. Without it that branch never runs, and the durable
                // half this test claims to cover would go untested.
                "predicateGenerators": [{"matches": {"path": true}}]
            }}]}]
        }))
        .await;

        let first = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port_b}/x"))
            .send()
            .await
            .expect("proxied request");

        assert!(
            first.headers().get("x-bin").is_none(),
            "an undecodable upstream header must not reach the client at all; before #1041 it \
             arrived as an empty string the origin never sent"
        );
        assert_eq!(
            first.headers().get("x-ok").map(|v| v.as_bytes()),
            Some(b"1".as_slice()),
            "a decodable header alongside it is unaffected"
        );
        assert_eq!(
            first
                .headers()
                .get("content-disposition")
                .map(|v| v.as_bytes()),
            Some(r#"attachment; filename="résumé.pdf""#.as_bytes()),
            "valid UTF-8 beyond ASCII must relay byte-exact — `HeaderValue::to_str` rejected it, \
             so the old code blanked a header the origin sent perfectly correctly"
        );
        let cookies: Vec<&[u8]> = first
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.as_bytes())
            .collect();
        assert_eq!(
            cookies,
            vec![b"a=1".as_slice(), b"b=2".as_slice()],
            "multiplicity survives the relay (RFC 7230 §3.2.2 forbids folding Set-Cookie)"
        );

        // The durable half. The origin accepts exactly ONE connection, so a second request that
        // succeeds can only have been answered from the stub `proxyOnce` generated — which is
        // where a blanked value used to be persisted and then served forever after, long after the
        // origin was out of the picture. Asserting it through the replay rather than through the
        // admin JSON keeps the test on the behaviour instead of on a listing shape.
        let replayed = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port_b}/x"))
            .send()
            .await
            .expect("replayed request");
        assert_eq!(
            replayed.headers().get("x-ok").map(|v| v.as_bytes()),
            Some(b"1".as_slice()),
            "sanity: the replay was served from the generated stub, not the (now closed) origin"
        );
        assert!(
            replayed.headers().get("x-bin").is_none(),
            "the dropped header must not be resurrected from the recorded stub"
        );
        assert_eq!(
            replayed
                .headers()
                .get("content-disposition")
                .map(|v| v.as_bytes()),
            Some(r#"attachment; filename="résumé.pdf""#.as_bytes()),
            "and the valid UTF-8 value must survive into the stub, not just the live relay"
        );

        let _ = manager.delete_imposter(port_b).await;
    }
}

// Issue #238: multi-value header support on the served response and the recorded request.
mod multi_value_headers {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    async fn serve(config: serde_json::Value) -> (Arc<ImposterManager>, u16) {
        let manager = Arc::new(ImposterManager::new());
        let port = manager
            .create_imposter(serde_json::from_value(config).unwrap())
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;
        (manager, port)
    }

    #[tokio::test]
    async fn serves_both_values_of_a_response_header() {
        let (manager, imposter_port) = serve(serde_json::json!({
            "protocol": "http",
            "stubs": [{"responses": [{"is": {"statusCode": 200,
                "headers": {"Set-Cookie": ["a=1", "b=2"]}, "body": "ok"}}]}]
        }))
        .await;

        let resp = reqwest::get(format!("http://127.0.0.1:{imposter_port}/x"))
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
            "both Set-Cookie lines served, got {cookies:?}"
        );
        assert!(cookies.contains(&"a=1".to_string()) && cookies.contains(&"b=2".to_string()));

        let _ = manager.delete_imposter(imposter_port).await;
    }

    #[tokio::test]
    async fn default_response_serves_both_values_of_a_header() {
        // No stub matches → the imposter's defaultResponse is served (a separate emission site).
        let (manager, imposter_port) = serve(serde_json::json!({
            "protocol": "http",
            "defaultResponse": {"statusCode": 200,
                "headers": {"Set-Cookie": ["a=1", "b=2"]}, "body": "def"},
            "stubs": []
        }))
        .await;

        let resp = reqwest::get(format!("http://127.0.0.1:{imposter_port}/nomatch"))
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

        let _ = manager.delete_imposter(imposter_port).await;
    }

    #[tokio::test]
    async fn single_value_response_header_still_works() {
        let (manager, imposter_port) = serve(serde_json::json!({
            "protocol": "http",
            "stubs": [{"responses": [{"is": {"statusCode": 200,
                "headers": {"X-Custom": "v"}, "body": "ok"}}]}]
        }))
        .await;

        let resp = reqwest::get(format!("http://127.0.0.1:{imposter_port}/x"))
            .await
            .unwrap();
        assert_eq!(resp.headers().get("x-custom").unwrap(), "v");
        let _ = manager.delete_imposter(imposter_port).await;
    }

    #[tokio::test]
    async fn records_both_values_of_a_request_header() {
        let (manager, imposter_port) = serve(serde_json::json!({
            "protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "ok"}}]}]
        }))
        .await;

        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{imposter_port}/x"))
            .header("X-Multi", "one")
            .header("X-Multi", "two")
            .send()
            .await
            .unwrap();

        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:0".parse().unwrap(),
            manager.clone(),
            None,
        );
        let admin = bind_admin(server).await;

        let recorded = json(
            &reqwest::Client::new(),
            format!("{admin}/imposters/{imposter_port}/requests"),
        )
        .await;
        let values: Vec<String> = recorded[0]["headers"]["X-Multi"]
            .as_array()
            .expect("multi-value header serialized as array")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            values,
            vec!["one".to_string(), "two".to_string()],
            "both request header values recorded, in the order the client sent them (#1025 \
             tightened this from a presence check — order is what `first()` selects at the \
             scripting and flow-id boundaries), got {values:?}"
        );

        let _ = manager.delete_imposter(imposter_port).await;
    }
}

// Issue #1025: the imposter listener collapsed a repeated REQUEST header to its last value before
// matching, and turned a non-UTF-8 value into an empty string. These drive the real listener over
// a socket, because the defect lives in how the listener builds its header map — a unit test on
// the collector alone cannot show that the map actually reaches the matcher.
mod repeated_request_headers_reach_matching {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::time::Duration;

    async fn serve(config: serde_json::Value) -> (Arc<ImposterManager>, u16) {
        let manager = Arc::new(ImposterManager::new());
        let port = manager
            .create_imposter(serde_json::from_value(config).unwrap())
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;
        (manager, port)
    }

    /// An imposter whose only stub matches `X-Test: <value>` and answers "hit"; anything else
    /// falls through to the 200 "miss" default.
    fn matching_on(predicate: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "protocol": "http", "recordRequests": true,
            "defaultResponse": {"statusCode": 200, "body": "miss"},
            "stubs": [{
                "predicates": [predicate],
                "responses": [{"is": {"statusCode": 200, "body": "hit"}}]
            }]
        })
    }

    async fn send_repeated(port: u16) -> String {
        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/x"))
            .header("X-Test", "first")
            .header("X-Test", "second")
            .send()
            .await
            .expect("request sent");
        resp.text().await.expect("body read")
    }

    #[tokio::test]
    async fn a_repeated_header_matches_on_its_first_value() {
        // THE BUG. Last-wins meant `first` was shadowed by `second` and could never match.
        let (manager, port) = serve(matching_on(
            serde_json::json!({"equals": {"headers": {"X-Test": "first"}}}),
        ))
        .await;
        assert_eq!(
            send_repeated(port).await,
            "hit",
            "the first value of a repeated header must be matchable"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn a_repeated_header_still_matches_on_its_last_value() {
        // Passes before the fix too, and is here precisely for that reason: without it, an
        // implementation that merely swapped which single value survives would satisfy the test
        // above and still be wrong.
        let (manager, port) = serve(matching_on(
            serde_json::json!({"equals": {"headers": {"X-Test": "second"}}}),
        ))
        .await;
        assert_eq!(
            send_repeated(port).await,
            "hit",
            "the last value must keep matching — this is a widening, not a swap"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn a_value_the_client_never_sent_does_not_match() {
        let (manager, port) = serve(matching_on(
            serde_json::json!({"equals": {"headers": {"X-Test": "third"}}}),
        ))
        .await;
        assert_eq!(
            send_repeated(port).await,
            "miss",
            "any-value matching must not degrade into matching anything"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn not_no_longer_matches_a_request_carrying_the_shadowed_value() {
        // The one behaviour change that can surprise a user, so it is pinned rather than left to
        // the CHANGELOG: `first` used to be invisible, which made the negation true.
        let (manager, port) = serve(matching_on(
            serde_json::json!({"not": {"equals": {"headers": {"X-Test": "first"}}}}),
        ))
        .await;
        assert_eq!(
            send_repeated(port).await,
            "miss",
            "`not` sees every value now, so a request carrying `first` fails the negation"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn a_non_utf8_header_value_is_not_offered_to_predicates_as_an_empty_string() {
        // `v.to_str().unwrap_or("")` made `{"equals":{"headers":{"x-bin":""}}}` match a request
        // that sent raw bytes — a data-path swallow inventing a value the client never sent.
        // Sent over a hand-written socket because reqwest will not build a non-UTF-8 header value.
        let (manager, port) = serve(matching_on(
            serde_json::json!({"equals": {"headers": {"X-Bin": ""}}}),
        ))
        .await;

        let body = tokio::task::spawn_blocking(move || {
            let mut sock =
                std::net::TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
            let mut raw = b"GET /x HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Bin: ".to_vec();
            raw.extend_from_slice(&[0xFF, 0xFE]);
            raw.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
            sock.write_all(&raw).expect("write");
            let mut out = String::new();
            let _ = sock.read_to_string(&mut out);
            out
        })
        .await
        .expect("raw request");

        assert!(
            body.contains("miss"),
            "a non-UTF-8 value must not be matchable as an empty string, got: {body}"
        );

        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:0".parse().unwrap(),
            manager.clone(),
            None,
        );
        let admin = bind_admin(server).await;
        let recorded = json(
            &reqwest::Client::new(),
            format!("{admin}/imposters/{port}/requests"),
        )
        .await;
        assert_eq!(
            recorded[0]["headers"]["X-Bin"],
            serde_json::Value::Null,
            "the journal must not claim the client sent an empty X-Bin either, got {}",
            recorded[0]["headers"]
        );

        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn a_header_whose_only_value_is_non_utf8_does_not_exist_for_predicates() {
        // The map-level assertion (`!contains_key`) lives in the collector's unit tests; this is
        // the behavioural half — what `exists` actually answers at the predicate layer for a name
        // the collector emptied. "Present but empty" and "absent" are different answers to
        // `exists`, and only one of them is honest about what the client sent.
        let (manager, port) = serve(matching_on(
            serde_json::json!({"exists": {"headers": {"X-Bin": true}}}),
        ))
        .await;

        let body = tokio::task::spawn_blocking(move || {
            let mut sock =
                std::net::TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
            let mut raw = b"GET /x HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Bin: ".to_vec();
            raw.extend_from_slice(&[0xFF, 0xFE]);
            raw.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
            sock.write_all(&raw).expect("write");
            let mut out = String::new();
            let _ = sock.read_to_string(&mut out);
            out
        })
        .await
        .expect("raw request");

        assert!(
            body.contains("miss"),
            "a name left with no decodable value must not report as present, got: {body}"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn a_proxied_request_forwards_both_values_of_a_repeated_header() {
        // AC2's "forwarded" half. `handle_proxy_request` iterated the single-value map, so a
        // proxied request reached the upstream carrying only whichever value used to survive the
        // collapse. Upstream here is a second, recording imposter, so its journal is the evidence
        // of what actually went over the wire.
        let manager = Arc::new(ImposterManager::new());
        let port_a = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "http", "recordRequests": true,
                    "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "upstream"}}]}]
                }))
                .unwrap(),
            )
            .await
            .expect("upstream imposter");
        let port_b = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "http",
                    "stubs": [{"responses": [{"proxy": {
                        "to": format!("http://127.0.0.1:{port_a}"), "mode": "proxyTransparent"
                    }}]}]
                }))
                .unwrap(),
            )
            .await
            .expect("proxy imposter");
        tokio::time::sleep(Duration::from_millis(300)).await;

        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port_b}/x"))
            .header("X-Test", "first")
            .header("X-Test", "second")
            .send()
            .await
            .expect("proxied request");

        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:0".parse().unwrap(),
            manager.clone(),
            None,
        );
        let admin = bind_admin(server).await;

        let recorded = json(
            &reqwest::Client::new(),
            format!("{admin}/imposters/{port_a}/requests"),
        )
        .await;
        let values: Vec<String> = recorded[0]["headers"]["X-Test"]
            .as_array()
            .expect("upstream recorded X-Test as an array")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            values,
            vec!["first".to_string(), "second".to_string()],
            "both values must reach the upstream, in send order, got {values:?}"
        );

        let _ = manager.delete_imposter(port_b).await;
        let _ = manager.delete_imposter(port_a).await;
    }

    // Issue #1040: the behaviours' and templating's view of the request headers used to be a
    // SECOND pass over the raw `HeaderMap` — coercing an undecodable value to `""` and keeping a
    // repeated header's LAST value, while predicates, forwarding, the journal and inject all read
    // the collected map and took the first. One request, two answers, depending on which surface
    // asked.
    //
    // This is the wiring half of the fix, and it is the half a unit test cannot reach: the request
    // map is shadowed inside the handler by the response header map of the same name, so passing
    // the wrong one compiles cleanly and yields a plausible wrong answer. `copy` and
    // `${request.headers.*}` are asserted together because they are the two independent consumers
    // of that wiring.
    #[tokio::test]
    async fn behaviors_and_templates_see_the_first_value_and_never_an_emptied_header() {
        let (manager, imposter_port) = serve(serde_json::json!({
            "protocol": "http",
            "stubs": [{"responses": [{
                "is": {"statusCode": 200,
                       "body": "copied=${ID}|bin=[${BIN}]|tmpl=${request.headers.x-id}"},
                "_rift": {"templated": true},
                "_behaviors": {"copy": [
                    {"from": {"headers": "X-Id"}, "into": "${ID}",
                     "using": {"method": "regex", "selector": ".+"}},
                    {"from": {"headers": "X-Bin"}, "into": "${BIN}",
                     "using": {"method": "regex", "selector": ".+"}}
                ]}
            }]}]
        }))
        .await;

        let body = tokio::task::spawn_blocking(move || {
            let mut sock = std::net::TcpStream::connect(format!("127.0.0.1:{imposter_port}"))
                .expect("connect");
            let mut raw =
                b"GET /x HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Id: a\r\nX-Id: b\r\nX-Bin: ".to_vec();
            raw.extend_from_slice(&[0xFF, 0xFE]);
            raw.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
            sock.write_all(&raw).expect("write");
            let mut out = String::new();
            let _ = sock.read_to_string(&mut out);
            out
        })
        .await
        .expect("raw request");

        assert!(
            body.contains("copied=a"),
            "`copy` must take the FIRST value of a repeated header, matching predicates and \
             inject; before #1040 it took the last (`b`). got: {body}"
        );
        // Deliberately NOT asserted here: that the undecodable `X-Bin` is *absent* rather than
        // present-as-empty. `copy` maps both to the same output by design — a source it cannot
        // find and a present-but-empty value that fails the `.+` selector both leave the token as
        // authored, as in Mountebank (issue #1402). Template substitution collapses them
        // the same way (`get` returns `None`, then `unwrap_or_default`). The absence is real and is
        // pinned where it is observable: `from_request_omits_a_header_whose_only_value_was_
        // undecodable` at the map level, and `a_header_whose_only_value_is_non_utf8_does_not_exist_
        // for_predicates` above at the `exists` level. Asserting it here would only have looked
        // like coverage.
        assert!(
            body.contains("bin=[${BIN}]"),
            "sanity: the undecodable header contributes nothing, however it is represented. \
             got: {body}"
        );
        assert!(
            body.contains("tmpl=a"),
            "template substitution reads the same collected view, so it agrees with `copy`; \
             before #1040 it made its own last-wins pass. got: {body}"
        );
        let _ = manager.delete_imposter(imposter_port).await;
    }
}

// Issue #197: POST /admin/reload re-reads the config source and replaces imposters.
// Several tests still seed state through the deprecated `reload()` on purpose — it is
// kept working alongside `apply_config` (issue #316).
#[allow(deprecated)]
mod reload {
    use super::*;
    use rift_http_proxy::config_loader::{ConfigSource, load_configs};

    /// One imposter serving `body` on `/p`; `None` leaves the port to the manager.
    fn cfg(port: Option<u16>, body: &str) -> String {
        let port = port.map_or(String::new(), |p| format!(r#""port":{p},"#));
        format!(
            r#"{{"imposters":[{{{port}"protocol":"http","stubs":[
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

    /// The ports the manager is serving, ascending.
    fn ports(manager: &ImposterManager) -> Vec<u16> {
        let mut ports: Vec<u16> = manager
            .list_imposters()
            .iter()
            .filter_map(|i| i.current_config().port)
            .collect();
        ports.sort_unstable();
        ports
    }

    async fn start(src: Option<ConfigSource>) -> (std::sync::Arc<ImposterManager>, String) {
        let manager = std::sync::Arc::new(ImposterManager::new());
        let mut server = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:0".parse().unwrap(),
            manager.clone(),
            None,
        );
        if let Some(src) = src {
            server = server.with_config_source(src);
        }
        let admin = bind_admin(server).await;
        (manager, admin)
    }

    #[tokio::test]
    async fn reload_replaces_imposters_from_configfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("imposters.json");
        std::fs::write(&path, cfg(None, "v1")).unwrap();
        let source = ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        };

        let (manager, admin) = start(Some(source.clone())).await;
        manager
            .reload(load_configs(&source).unwrap())
            .await
            .unwrap();
        let [imposter_port] = ports(&manager)[..] else {
            panic!("one imposter seeded")
        };
        assert_eq!(stub_body(&manager, imposter_port), "v1");

        // change the file on disk, then reload via the admin endpoint
        std::fs::write(&path, cfg(Some(imposter_port), "v2")).unwrap();
        let resp = reqwest::Client::new()
            .post(format!("{admin}/admin/reload"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            stub_body(&manager, imposter_port),
            "v2",
            "reload picked up the file change"
        );
        // the listener was actually re-bound on the same port and now serves v2 over the wire
        let served = reqwest::get(format!("http://127.0.0.1:{imposter_port}/p"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(served, "v2", "reloaded imposter serves the new content");

        let _ = manager.delete_imposter(imposter_port).await;
    }

    #[tokio::test]
    async fn reload_semantic_error_keeps_existing_imposters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("imposters.json");
        std::fs::write(&path, cfg(None, "good")).unwrap();
        let source = ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        };

        let (manager, admin) = start(Some(source.clone())).await;
        manager
            .reload(load_configs(&source).unwrap())
            .await
            .unwrap();
        let [imposter_port] = ports(&manager)[..] else {
            panic!("one imposter seeded")
        };
        assert_eq!(stub_body(&manager, imposter_port), "good");

        // parses fine, but the protocol is invalid → must be rejected BEFORE delete_all,
        // leaving the running imposter intact (the destructive partial-teardown guard)
        std::fs::write(
            &path,
            format!(r#"{{"imposters":[{{"port":{imposter_port},"protocol":"tcp","stubs":[]}}]}}"#),
        )
        .unwrap();
        let resp = reqwest::Client::new()
            .post(format!("{admin}/admin/reload"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 500);
        assert_eq!(
            stub_body(&manager, imposter_port),
            "good",
            "a semantically-invalid config must not tear down the running imposters"
        );

        let _ = manager.delete_imposter(imposter_port).await;
    }

    #[tokio::test]
    async fn reload_parse_error_keeps_existing_imposters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("imposters.json");
        std::fs::write(&path, cfg(None, "good")).unwrap();
        let source = ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        };

        let (manager, admin) = start(Some(source.clone())).await;
        manager
            .reload(load_configs(&source).unwrap())
            .await
            .unwrap();
        let [port] = ports(&manager)[..] else {
            panic!("one imposter seeded")
        };
        assert_eq!(stub_body(&manager, port), "good");

        // corrupt the file → reload must 500 and leave the running imposter intact
        std::fs::write(&path, "{ not valid json").unwrap();
        let resp = reqwest::Client::new()
            .post(format!("{admin}/admin/reload"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 500);
        assert_eq!(
            stub_body(&manager, port),
            "good",
            "existing imposter unchanged on parse error"
        );

        let _ = manager.delete_imposter(port).await;
    }

    /// `None` leaves both ports to the manager.
    fn two_imposter_cfg(ports: Option<(u16, u16)>, keep_body: &str, sibling_body: &str) -> String {
        let (port_a, port_b) = ports.map_or((String::new(), String::new()), |(a, b)| {
            (format!(r#""port":{a},"#), format!(r#""port":{b},"#))
        });
        format!(
            r#"{{"imposters":[
                {{{port_a}"protocol":"http","recordRequests":true,"stubs":[
                    {{"predicates":[{{"equals":{{"path":"/p"}}}}],
                     "responses":[{{"is":{{"statusCode":200,"body":"{keep_body}"}}}}]}}]}},
                {{{port_b}"protocol":"http","stubs":[
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
        std::fs::write(&path, two_imposter_cfg(None, "keep", "v1")).unwrap();
        let source = ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        };

        let (manager, admin) = start(Some(source.clone())).await;
        // Seeded as the server seeds a config file: trusted, the access its reload grants. A reload
        // never widens an imposter's access in place (issue #1645), so a seed with less would make
        // the reload replace both imposters.
        manager
            .apply_config_from(
                load_configs(&source).unwrap(),
                rift_http_proxy::imposter::ScriptFileAccess::trusted_cwd(),
            )
            .await
            .unwrap();
        // The manager assigned both ports; tell them apart by what they serve. The rewrite below
        // names them, so the reload reconciles these two imposters rather than creating others.
        let [first, second] = ports(&manager)[..] else {
            panic!("two imposters seeded")
        };
        let (port_a, port_b) = if stub_body(&manager, first) == "keep" {
            (first, second)
        } else {
            (second, first)
        };

        // Drive the untouched imposter so it accrues runtime state.
        let served = reqwest::get(format!("http://127.0.0.1:{port_a}/p"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(served, "keep");
        let before = manager.get_imposter(port_a).unwrap();
        assert_eq!(before.get_recorded_requests().len(), 1);

        // Change only the sibling on disk, then reload via the admin endpoint.
        std::fs::write(
            &path,
            two_imposter_cfg(Some((port_a, port_b)), "keep", "v2"),
        )
        .unwrap();
        let resp = reqwest::Client::new()
            .post(format!("{admin}/admin/reload"))
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
            serde_json::json!([port_b]),
            "single-stub rewrite is a degenerate patch, so the sibling is replaced wholesale"
        );

        let after = manager.get_imposter(port_a).unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&before, &after),
            "reload must not recreate an unchanged imposter"
        );
        assert_eq!(
            after.get_recorded_requests().len(),
            1,
            "reload must not reset an unchanged imposter's recorded requests"
        );
        assert_eq!(stub_body(&manager, port_b), "v2");

        manager.delete_all().await;
    }

    // Issue #1122: with `--configfile` and `--datadir` together, reload re-reads both stores. It
    // used to re-apply the config file alone, so the sweep deleted every admin-created imposter and
    // unlinked its `{port}.json`, which also lost it across a restart.
    #[tokio::test]
    async fn reload_with_configfile_and_datadir_keeps_persisted_imposters() {
        use rift_http_proxy::imposter::Persistence;
        use rift_http_proxy::sources::{FileSource, SourceRef, SourceRegistry, SourceSet};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let datadir = dir.path().join("data");
        std::fs::create_dir(&datadir).unwrap();
        let path = dir.path().join("imposters.json");
        std::fs::write(&path, cfg(None, "file")).unwrap();

        let manager = std::sync::Arc::new(ImposterManager::with_datadir(Some(datadir.clone())));
        // What startup does for a `--configfile` imposter.
        let [config]: [_; 1] = load_configs(&ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        })
        .unwrap()
        .try_into()
        .expect("one config-file imposter");
        let from_file = manager
            .create_imposter_as(config, Persistence::Ephemeral)
            .await
            .unwrap();
        // Name the assigned port in the file, so a reload reconciles this imposter instead of
        // recreating a port-less one.
        std::fs::write(&path, cfg(Some(from_file), "file")).unwrap();

        let mut registry = SourceRegistry::new();
        registry.register(Arc::new(FileSource::new(false))).unwrap();
        let set = Arc::new(SourceSet::new(
            vec![SourceRef::new(format!("file:{}", path.display()))],
            registry,
        ));
        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:0".parse().unwrap(),
            manager.clone(),
            None,
        )
        .with_imposter_sources(set, Some(datadir.clone()));
        let admin = bind_admin(server).await;

        let client = reqwest::Client::new();
        let created = client
            .post(format!("{admin}/imposters"))
            .json(&serde_json::json!({"protocol": "http", "stubs": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), 201);
        let created: serde_json::Value = created.json().await.unwrap();
        let from_api = created["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or_else(|| panic!("the assigned port: {created}"));
        let api_file = datadir.join(format!("{from_api}.json"));
        assert!(api_file.exists(), "an admin-API create is persisted");

        let reload = || async {
            let resp = client
                .post(format!("{admin}/admin/reload"))
                .send()
                .await
                .unwrap();
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap();
            (status, body)
        };

        let (status, body) = reload().await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["deleted"], serde_json::json!([]), "{body}");
        assert!(
            manager.get_imposter(from_api).is_ok(),
            "admin imposter still served"
        );
        assert!(
            manager.get_imposter(from_file).is_ok(),
            "config-file imposter still served"
        );
        assert!(
            api_file.exists(),
            "reload must not unlink a persisted imposter's file"
        );
        assert!(
            !datadir.join(format!("{from_file}.json")).exists(),
            "a config-file imposter is never written to the datadir"
        );

        // Each store still sweeps what it no longer declares.
        std::fs::write(&path, r#"{"imposters":[]}"#).unwrap();
        let (status, body) = reload().await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["deleted"], serde_json::json!([from_file]), "{body}");
        assert!(
            manager.get_imposter(from_api).is_ok(),
            "the datadir imposter is kept"
        );

        std::fs::remove_file(&api_file).unwrap();
        let (status, body) = reload().await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["deleted"], serde_json::json!([from_api]), "{body}");

        manager.delete_all().await;
    }

    #[tokio::test]
    async fn reload_with_no_source_is_noop_200() {
        let (manager, admin) = start(None).await;
        let resp = reqwest::Client::new()
            .post(format!("{admin}/admin/reload"))
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
        let imposter_port = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "http",
                    "stubs": [{"responses": [{"is": {"statusCode": 200,
                        "body": "{\"exp\":\"{{DAYS+5}}\",\"now\":\"{{NOW}}\"}"}}]}]
                }))
                .unwrap(),
            )
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let body = reqwest::get(format!("http://127.0.0.1:{imposter_port}/x"))
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

        let _ = manager.delete_imposter(imposter_port).await;
    }

    #[tokio::test]
    async fn served_default_response_resolves_date_template() {
        let manager = Arc::new(ImposterManager::new());
        let imposter_port = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "http",
                    "defaultResponse": {"statusCode": 200, "body": "{\"d\":\"{{DAYS+3}}\"}"},
                    "stubs": []
                }))
                .unwrap(),
            )
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let body = reqwest::get(format!("http://127.0.0.1:{imposter_port}/nomatch"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            !body.contains("{{DAYS+3}}"),
            "defaultResponse body must expand date templates too, got {body}"
        );
        let _ = manager.delete_imposter(imposter_port).await;
    }
}

// Issue #212: reach imposters through the single admin port via `/__rift/:port/<path>`.
mod gateway {
    use super::*;
    use std::sync::Arc;

    /// Start an imposter (with the given stubs) + an AdminApiServer; returns
    /// (manager, admin_base, imposter_port).
    async fn setup(stubs: serde_json::Value) -> (Arc<ImposterManager>, String, u16) {
        let manager = Arc::new(ImposterManager::new());
        let imposter_port = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "http", "stubs": stubs
                }))
                .unwrap(),
            )
            .await
            .expect("create imposter");
        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:0".parse().unwrap(),
            manager.clone(),
            None,
        );
        let admin = bind_admin(server).await;
        (manager, admin, imposter_port)
    }

    #[tokio::test]
    async fn gateway_routes_to_imposter() {
        let (manager, admin, imposter_port) = setup(serde_json::json!([{
            "predicates": [{"equals": {"path": "/api/data"}}],
            "responses": [{"is": {"statusCode": 200, "body": "routed"}}]
        }]))
        .await;

        // Hit the imposter through the admin port — the imposter must see path /api/data.
        let resp = reqwest::get(format!("{admin}/__rift/{imposter_port}/api/data"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "routed");
        let _ = manager.delete_imposter(imposter_port).await;
    }

    #[tokio::test]
    async fn gateway_preserves_method_and_query() {
        let (manager, admin, port) = setup(serde_json::json!([{
            "predicates": [{"equals": {"method": "POST", "path": "/submit", "query": {"q": "1"}}}],
            "responses": [{"is": {"statusCode": 201, "body": "posted"}}]
        }]))
        .await;

        let resp = reqwest::Client::new()
            .post(format!("{admin}/__rift/{port}/submit?q=1"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "method + query must reach the imposter");
        assert_eq!(resp.text().await.unwrap(), "posted");
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn gateway_unknown_port_404() {
        let (manager, admin, imposter_port) =
            setup(serde_json::json!([{"responses": [{"is": {"statusCode": 200, "body": "x"}}]}]))
                .await;
        let resp = reqwest::get(format!("{admin}/__rift/59999/anything"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "no imposter on that port → 404");
        let _ = manager.delete_imposter(imposter_port).await;
    }

    #[tokio::test]
    async fn gateway_forwards_post_body() {
        let (manager, admin, port) = setup(serde_json::json!([{
            "predicates": [{"equals": {"method": "POST", "path": "/echo", "body": "hello-body"}}],
            "responses": [{"is": {"statusCode": 200, "body": "got-body"}}]
        }]))
        .await;
        let resp = reqwest::Client::new()
            .post(format!("{admin}/__rift/{port}/echo"))
            .body("hello-body")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.text().await.unwrap(),
            "got-body",
            "the POST body must reach the imposter through the gateway"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn gateway_no_subpath_routes_to_root() {
        let (manager, admin, port) = setup(serde_json::json!([{
            "predicates": [{"equals": {"path": "/"}}],
            "responses": [{"is": {"statusCode": 200, "body": "root"}}]
        }]))
        .await;
        let resp = reqwest::get(format!("{admin}/__rift/{port}"))
            .await
            .unwrap();
        assert_eq!(
            resp.text().await.unwrap(),
            "root",
            "no sub-path → imposter root"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn gateway_non_numeric_port_400() {
        let (manager, admin, imposter_port) =
            setup(serde_json::json!([{"responses": [{"is": {"statusCode": 200, "body": "x"}}]}]))
                .await;
        let resp = reqwest::get(format!("{admin}/__rift/notaport/x"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "non-numeric port is a malformed gateway target → 400, distinct from 404"
        );
        let _ = manager.delete_imposter(imposter_port).await;
    }

    #[tokio::test]
    async fn gateway_not_gated_by_admin_apikey() {
        // The gateway is data-plane imposter traffic: it must work WITHOUT the admin key, even
        // when --apikey is set (otherwise the app-under-test would need the admin key).
        let manager = Arc::new(ImposterManager::new());
        let imposter_port = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "http",
                    "stubs": [{"predicates": [{"equals": {"path": "/x"}}],
                              "responses": [{"is": {"statusCode": 200, "body": "open"}}]}]
                }))
                .unwrap(),
            )
            .await
            .expect("create imposter");
        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:0".parse().unwrap(),
            manager.clone(),
            Some("secret".to_string()),
        );
        let admin = bind_admin(server).await;

        // Admin route without the key → 401 (control plane stays protected).
        let admin_resp = reqwest::get(format!("{admin}/imposters")).await.unwrap();
        assert_eq!(
            admin_resp.status(),
            401,
            "admin route still requires the apikey"
        );

        // Gateway without the key → serves (data plane is not gated).
        let gw = reqwest::get(format!("{admin}/__rift/{imposter_port}/x"))
            .await
            .unwrap();
        assert_eq!(gw.status(), 200);
        assert_eq!(
            gw.text().await.unwrap(),
            "open",
            "gateway works without the admin key"
        );
        let _ = manager.delete_imposter(imposter_port).await;
    }
}

// Issue #260: GET /imposters/:port exposes the imposter's flowState (so tools can read
// flowIdSource), with anything beyond the allowlist redacted.
#[tokio::test]
async fn get_imposter_exposes_flowstate_redacted() {
    let manager = std::sync::Arc::new(ImposterManager::new());
    // a leftover `redis` block (written before the backend was removed) still decodes into the
    // flattened extras — include one with a credentialed URL to prove the GET projection strips it.
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http",
        "_rift": { "flowState": { "ttlSeconds": 300,
            "redis": { "url": "redis://user:topsecret@host:6379" },
            "flowIdSource": "header:X-Mock-Space" } },
        "stubs": [{ "predicates": [{ "equals": { "path": "/x" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }]
    }))
    .unwrap();
    let imposter_port = manager.create_imposter(config).await.expect("create");

    let server = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().unwrap(),
        manager.clone(),
        None,
    );
    let admin = bind_admin(server).await;

    let c = reqwest::Client::new();
    let v = json(&c, format!("{admin}/imposters/{imposter_port}")).await;
    assert_eq!(
        v.pointer("/_rift/flowState/flowIdSource")
            .and_then(|x| x.as_str()),
        Some("header:X-Mock-Space"),
        "GET must expose flat flowIdSource so rift-verify can drive correlated isolation: {v}"
    );
    assert!(
        v.pointer("/_rift/flowState/redis").is_none(),
        "a leftover redis block must be redacted from the exposed flowState: {v}"
    );
    assert!(
        !v.to_string().contains("topsecret"),
        "the redis credential must not survive anywhere in the GET response"
    );
}

// Issue #977: `mutualAuth` / `rejectUnauthorized` / `ca` were documented on HTTPS imposters and
// silently dropped — an author asking for client certificates got a listener accepting everyone.
mod mutual_tls {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
        KeyUsagePurpose,
    };
    use std::sync::Arc;
    use std::time::Duration;

    /// A CA plus a client certificate it signed.
    ///
    /// The client cert must carry `ExtendedKeyUsage = ClientAuth`; webpki rejects a ServerAuth-only
    /// certificate for client authentication, so the engine's own `mint_leaf` (ServerAuth) cannot
    /// be reused here.
    fn ca_and_client_cert() -> (String, String, String) {
        let mut ca_params = CertificateParams::new(Vec::new()).expect("ca params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "rift-977-test-ca");
        let ca_key = KeyPair::generate().expect("ca key");
        let ca_cert = ca_params.self_signed(&ca_key).expect("self-signed ca");

        let mut leaf = CertificateParams::new(vec!["rift-977-client".to_string()]).expect("leaf");
        leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        leaf.distinguished_name
            .push(DnType::CommonName, "rift-977-client");
        let leaf_key = KeyPair::generate().expect("leaf key");
        let leaf_cert = leaf
            .signed_by(&leaf_key, &rcgen::Issuer::from_params(&ca_params, &ca_key))
            .expect("sign leaf");

        (ca_cert.pem(), leaf_cert.pem(), leaf_key.serialize_pem())
    }

    fn server_cert() -> (String, String) {
        let c = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("server cert");
        (c.cert.pem(), c.signing_key.serialize_pem())
    }

    fn client_presenting(cert_pem: &str, key_pem: &str) -> reqwest::Client {
        let identity = reqwest::Identity::from_pem(format!("{cert_pem}{key_pem}").as_bytes())
            .expect("identity");
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .identity(identity)
            .timeout(Duration::from_secs(3))
            .build()
            .expect("client with identity")
    }

    fn bare_client() -> reqwest::Client {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(3))
            .build()
            .expect("bare client")
    }

    fn cfg(extra: serde_json::Value) -> serde_json::Value {
        let (cert, key) = server_cert();
        let mut c = serde_json::json!({
            "protocol": "https",
            "cert": cert,
            "key": key,
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "mtls-ok"}}]}]
        });
        for (k, v) in extra.as_object().expect("object") {
            c[k] = v.clone();
        }
        c
    }

    async fn serve(manager: &Arc<ImposterManager>, c: serde_json::Value) -> u16 {
        let port = manager
            .create_imposter(serde_json::from_value(c).expect("config"))
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;
        port
    }

    #[tokio::test]
    async fn mutual_auth_rejects_a_client_with_no_certificate() {
        let manager = Arc::new(ImposterManager::new());
        let port = serve(&manager, cfg(serde_json::json!({"mutualAuth": true}))).await;

        // The refusal is at the TLS handshake, not an HTTP status — which is the whole point:
        // a real mTLS gateway never lets a cert-less client reach the application.
        let err = bare_client()
            .get(format!("https://localhost:{port}/"))
            .send()
            .await
            .expect_err("a client with no certificate must not complete the handshake");
        assert!(
            !err.is_status(),
            "expected a transport/handshake failure, got an HTTP status: {err}"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn mutual_auth_accepts_any_presented_certificate() {
        let (_ca, client_cert, client_key) = ca_and_client_cert();
        let manager = Arc::new(ImposterManager::new());
        let port = serve(&manager, cfg(serde_json::json!({"mutualAuth": true}))).await;

        let resp = client_presenting(&client_cert, &client_key)
            .get(format!("https://localhost:{port}/"))
            .send()
            .await
            .expect("a presented certificate is accepted without validation");
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap_or_default(), "mtls-ok");
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn reject_unauthorized_accepts_a_certificate_from_the_ca() {
        let (ca, client_cert, client_key) = ca_and_client_cert();
        let manager = Arc::new(ImposterManager::new());
        let port = serve(
            &manager,
            cfg(serde_json::json!({"mutualAuth": true, "rejectUnauthorized": true, "ca": ca})),
        )
        .await;

        let resp = client_presenting(&client_cert, &client_key)
            .get(format!("https://localhost:{port}/"))
            .send()
            .await
            .expect("a certificate from the declared CA is accepted");
        assert_eq!(resp.status(), 200);
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn reject_unauthorized_refuses_a_certificate_from_another_ca() {
        let (ca, _, _) = ca_and_client_cert();
        // A second, unrelated CA — the imposter trusts only the first.
        let (_other_ca, other_cert, other_key) = ca_and_client_cert();
        let manager = Arc::new(ImposterManager::new());
        let port = serve(
            &manager,
            cfg(serde_json::json!({"mutualAuth": true, "rejectUnauthorized": true, "ca": ca})),
        )
        .await;

        let err = client_presenting(&other_cert, &other_key)
            .get(format!("https://localhost:{port}/"))
            .send()
            .await
            .expect_err("a certificate from an untrusted CA must be refused");
        assert!(!err.is_status(), "expected a handshake failure, got: {err}");
        let _ = manager.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn mutual_auth_works_on_the_self_signed_fallback() {
        // No inline cert/key: the imposter generates its own server cert, and client auth must
        // still apply — the derivation happens before the cert precedence, so it cannot be skipped.
        let (_ca, client_cert, client_key) = ca_and_client_cert();
        let manager = Arc::new(ImposterManager::new());
        let imposter_port = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "https",
                    "mutualAuth": true,
                    "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "mtls-ok"}}]}]
                }))
                .expect("config"),
            )
            .await
            .expect("create imposter");
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(
            bare_client()
                .get(format!("https://localhost:{imposter_port}/"))
                .send()
                .await
                .is_err(),
            "the self-signed fallback must still require a client certificate"
        );
        let resp = client_presenting(&client_cert, &client_key)
            .get(format!("https://localhost:{imposter_port}/"))
            .send()
            .await
            .expect("a presented certificate is accepted on the self-signed path");
        assert_eq!(resp.status(), 200);
        let _ = manager.delete_imposter(imposter_port).await;
    }

    #[tokio::test]
    async fn reject_unauthorized_without_ca_is_a_creation_error() {
        let manager = Arc::new(ImposterManager::new());
        let err = manager
            .create_imposter(
                serde_json::from_value(cfg(
                    serde_json::json!({"mutualAuth": true, "rejectUnauthorized": true}),
                ))
                .expect("config"),
            )
            .await
            .expect_err("rejectUnauthorized with no ca must not create an imposter");
        assert!(
            err.to_string().contains("ca"),
            "the error must name the missing key, got: {err}"
        );
    }

    #[tokio::test]
    async fn mutual_auth_on_http_is_a_creation_error() {
        let manager = Arc::new(ImposterManager::new());
        let err = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "http",
                    "mutualAuth": true,
                    "stubs": []
                }))
                .expect("config"),
            )
            .await
            .expect_err("a client certificate cannot be requested on a cleartext listener");
        assert!(
            err.to_string().contains("https"),
            "the error must name the required protocol, got: {err}"
        );
    }

    // The three combinations below all mean "you asked for something that cannot take effect".
    // Each was a `warn!` in the first draft of this change — which reaches the server log, never
    // the POST /imposters response, so the author who wrote the setting is the one person who
    // would not learn it does nothing. That is the pre-#977 bug wearing a different hat.
    #[tokio::test]
    async fn reject_unauthorized_without_mutual_auth_is_a_creation_error() {
        let manager = Arc::new(ImposterManager::new());
        let err = manager
            .create_imposter(
                serde_json::from_value(cfg(
                    serde_json::json!({"rejectUnauthorized": true, "ca": "PEM"}),
                ))
                .expect("config"),
            )
            .await
            .expect_err("without mutualAuth no certificate is requested, so neither key can apply");
        assert!(
            err.to_string().contains("mutualAuth"),
            "the error must name the key that is missing, got: {err}"
        );
    }

    #[tokio::test]
    async fn ca_without_reject_unauthorized_is_a_creation_error() {
        let (ca, _, _) = ca_and_client_cert();
        let manager = Arc::new(ImposterManager::new());
        let err = manager
            .create_imposter(
                serde_json::from_value(cfg(serde_json::json!({"mutualAuth": true, "ca": ca})))
                    .expect("config"),
            )
            .await
            .expect_err("supplying a CA that would never be consulted must not be accepted");
        assert!(
            err.to_string().contains("rejectUnauthorized"),
            "the error must name the key that would make the CA count, got: {err}"
        );
    }

    #[tokio::test]
    async fn an_empty_ca_array_is_a_creation_error_naming_ca() {
        let manager = Arc::new(ImposterManager::new());
        let err = manager
            .create_imposter(
                serde_json::from_value(cfg(
                    serde_json::json!({"mutualAuth": true, "rejectUnauthorized": true, "ca": []}),
                ))
                .expect("config"),
            )
            .await
            .expect_err("an empty anchor list validates nothing");
        assert!(
            err.to_string().contains("ca"),
            "the error must name `ca`, got: {err}"
        );
    }

    #[tokio::test]
    async fn a_ca_pem_with_no_certificate_is_a_creation_error() {
        // A private key instead of a certificate — the classic wrong-half-of-the-pair mistake.
        let key_only = rcgen::KeyPair::generate().expect("key").serialize_pem();
        let manager = Arc::new(ImposterManager::new());
        let err = manager
            .create_imposter(
                serde_json::from_value(cfg(serde_json::json!({
                    "mutualAuth": true, "rejectUnauthorized": true, "ca": key_only
                })))
                .expect("config"),
            )
            .await
            .expect_err("a PEM with no certificate cannot be a trust anchor");
        assert!(
            err.to_string().contains("certificate"),
            "the error must say what was missing, got: {err}"
        );
    }

    #[tokio::test]
    async fn mutual_auth_false_on_http_is_accepted() {
        // `"mutualAuth": false` on an http imposter appears in our own documentation example, so it
        // must keep working — only `true` is protocol-constrained.
        let manager = Arc::new(ImposterManager::new());
        let imposter_port = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({
                    "protocol": "http",
                    "mutualAuth": false,
                    "stubs": []
                }))
                .expect("config"),
            )
            .await
            .expect("mutualAuth: false must remain valid on http");
        let _ = manager.delete_imposter(imposter_port).await;
    }

    // Issue #1334: Mountebank prints an https imposter's TLS metadata (`httpsServer.js` metadata:
    // key, cert, mutualAuth, rejectUnauthorized, ca) on the detail; an http detail carries none.
    #[tokio::test]
    async fn https_detail_prints_tls_metadata_and_http_detail_does_not() {
        let (ca, _, _) = ca_and_client_cert();
        let manager = Arc::new(ImposterManager::new());
        let server = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:0".parse().unwrap(),
            manager.clone(),
            None,
        );
        let admin = bind_admin(server).await;
        let c = reqwest::Client::new();

        let https_cfg = cfg(serde_json::json!({
            "mutualAuth": true, "rejectUnauthorized": true, "ca": ca
        }));
        let (cert, key) = (https_cfg["cert"].clone(), https_cfg["key"].clone());
        let created: serde_json::Value = c
            .post(format!("{admin}/imposters"))
            .json(&https_cfg)
            .send()
            .await
            .expect("POST https imposter")
            .json()
            .await
            .expect("201 json");
        let port = created["port"].as_u64().expect("assigned port");

        let detail = json(&c, format!("{admin}/imposters/{port}")).await;
        assert_eq!(detail["cert"], cert, "detail: {detail}");
        assert_eq!(detail["key"], key);
        assert_eq!(detail["mutualAuth"], true);
        assert_eq!(detail["rejectUnauthorized"], true);
        // A single anchor prints as the string it was given, as `?replayable=true` does.
        assert_eq!(detail["ca"], serde_json::json!(ca));
        let replayable = json(&c, format!("{admin}/imposters/{port}?replayable=true")).await;
        for k in ["cert", "key", "mutualAuth", "rejectUnauthorized", "ca"] {
            assert_eq!(
                detail[k], replayable[k],
                "{k}: detail and replayable must agree"
            );
        }

        let listed = json(&c, format!("{admin}/imposters")).await;
        let entry = listed["imposters"]
            .as_array()
            .expect("imposters")
            .iter()
            .find(|e| e["port"].as_u64() == Some(port))
            .expect("listed")
            .clone();
        for k in ["cert", "key", "mutualAuth", "rejectUnauthorized", "ca"] {
            assert!(
                entry.get(k).is_none(),
                "list entry must not carry {k}: {entry}"
            );
        }

        let zero: serde_json::Value = c
            .post(format!("{admin}/imposters"))
            .json(&serde_json::json!({"protocol": "https", "stubs": []}))
            .send()
            .await
            .expect("POST zero-config https")
            .json()
            .await
            .expect("201 json");
        assert_eq!(zero["mutualAuth"], false, "zero-config https: {zero}");
        assert_eq!(zero["rejectUnauthorized"], false);
        for k in ["cert", "key"] {
            assert!(zero.get(k).is_none(), "no configured {k} to print: {zero}");
        }
        // Issue #1576: `ca` is always printed on https, `null` when unset, as Mountebank does.
        assert_eq!(zero.get("ca"), Some(&serde_json::Value::Null), "{zero}");
        let zero_port = zero["port"].as_u64().expect("assigned port");

        let http: serde_json::Value = c
            .post(format!("{admin}/imposters"))
            .json(&serde_json::json!({"protocol": "http", "stubs": []}))
            .send()
            .await
            .expect("POST http imposter")
            .json()
            .await
            .expect("201 json");
        for k in ["cert", "key", "mutualAuth", "rejectUnauthorized", "ca"] {
            assert!(
                http.get(k).is_none(),
                "http detail must not carry {k}: {http}"
            );
        }
        let http_port = http["port"].as_u64().expect("assigned port");
        let http_detail = json(&c, format!("{admin}/imposters/{http_port}")).await;
        for k in ["cert", "key", "mutualAuth", "rejectUnauthorized", "ca"] {
            assert!(http_detail.get(k).is_none(), "http GET must not carry {k}");
        }
        let zero_detail = json(&c, format!("{admin}/imposters/{zero_port}")).await;
        assert_eq!(zero_detail["mutualAuth"], false, "zero-config https GET");
        assert!(zero_detail.get("cert").is_none());

        for p in [port, zero_port, http_port] {
            let _ = manager.delete_imposter(p as u16).await;
        }
    }
}
