//! Issue #1188 (part B of #1184): Mountebank runs a response's behaviors on `inject` responses,
//! and honours `repeat` on every response type — including its canonical top-level
//! `response.repeat`, the form `mb save` writes. Rift ran behaviors on `is` only and read `repeat`
//! from an `is` response's block only.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

async fn start_admin() -> (reqwest::Client, String, Arc<ImposterManager>) {
    let manager = Arc::new(ImposterManager::new());
    let admin_port = free_port();
    let server = AdminApiServer::new(
        format!("127.0.0.1:{admin_port}").parse().expect("addr"),
        manager.clone(),
        None,
    )
    .with_allow_injection(true);
    tokio::spawn(server.run());
    let admin = format!("http://127.0.0.1:{admin_port}");
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client
            .get(format!("{admin}/imposters"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (client, admin, manager)
}

async fn create(client: &reqwest::Client, admin: &str, imposter: &Value) -> reqwest::Response {
    client
        .post(format!("{admin}/imposters"))
        .json(imposter)
        .send()
        .await
        .expect("POST")
}

async fn created(client: &reqwest::Client, admin: &str, imposter: &Value) -> Value {
    let response = create(client, admin, imposter).await;
    assert_eq!(response.status().as_u16(), 201, "{imposter}");
    response.json().await.expect("json")
}

fn ignored_warnings(body: &Value) -> Vec<Value> {
    body["_rift"]["warnings"]
        .as_array()
        .map(|w| {
            w.iter()
                .filter(|w| w["warningType"] == "config_key_ignored")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The bodies of `n` sequential GETs; a request the imposter refused (a fault) reads as `"<error>"`.
async fn bodies(client: &reqwest::Client, port: u16, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    for _ in 0..n {
        let body = match client.get(format!("http://127.0.0.1:{port}/")).send().await {
            Ok(r) => r.text().await.unwrap_or_else(|_| "<error>".to_string()),
            Err(_) => "<error>".to_string(),
        };
        out.push(body);
    }
    out
}

const INJECT: &str =
    "function (config) { return { statusCode: 200, headers: { 'x-a': '1' }, body: 'inj' }; }";

fn inject_imposter(port: u16, behaviors: Value) -> Value {
    json!({"port": port, "protocol": "http", "stubs": [{"responses": [
        {"inject": INJECT, "_behaviors": behaviors}
    ]}]})
}

// ---- behaviors on an inject response ------------------------------------------------------------

#[tokio::test]
async fn decorate_rewrites_an_inject_response() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    created(
        &client,
        &admin,
        &inject_imposter(
            port,
            json!({"decorate": "function (request, response) { response.body = response.body + '-decorated'; }"}),
        ),
    )
    .await;
    let response = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["x-a"], "1");
    assert_eq!(response.headers()["x-rift-inject"], "true");
    assert_eq!(response.text().await.expect("body"), "inj-decorated");
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn copy_on_an_inject_response_reads_the_request() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    let inject = "function (config) { return { statusCode: 200, body: 'path=${P}' }; }";
    created(
        &client,
        &admin,
        &json!({"port": port, "protocol": "http", "stubs": [{"responses": [{
            "inject": inject,
            "_behaviors": {"copy": [{"from": "path", "into": "${P}",
                                      "using": {"method": "regex", "selector": ".+"}}]}
        }]}]}),
    )
    .await;
    let body = client
        .get(format!("http://127.0.0.1:{port}/orders/7"))
        .send()
        .await
        .expect("GET")
        .text()
        .await
        .expect("body");
    assert_eq!(body, "path=/orders/7");
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn wait_delays_an_inject_response() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    created(
        &client,
        &admin,
        &inject_imposter(port, json!({"wait": 400})),
    )
    .await;
    let start = Instant::now();
    let body = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("GET")
        .text()
        .await
        .expect("body");
    assert_eq!(body, "inj");
    assert!(
        start.elapsed() >= Duration::from_millis(400),
        "served after {:?}",
        start.elapsed()
    );
    let _ = manager.delete_imposter(port).await;
}

/// Mountebank throws before `behaviors.execute`, so a failing inject runs none of them.
#[tokio::test]
async fn a_failing_inject_runs_no_behavior() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    created(
        &client,
        &admin,
        &json!({"port": port, "protocol": "http", "stubs": [{"responses": [{
            "inject": "function (config) { throw new Error('boom'); }",
            "_behaviors": {"wait": 1500}
        }]}]}),
    )
    .await;
    let start = Instant::now();
    let response = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status().as_u16(), 400);
    assert_eq!(response.headers()["x-rift-inject-error"], "true");
    assert!(
        start.elapsed() < Duration::from_millis(1200),
        "the wait ran: served after {:?}",
        start.elapsed()
    );
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn a_failing_decorate_on_an_inject_response_is_signalled_or_strict() {
    let (client, admin, manager) = start_admin().await;
    let throwing = json!({"decorate": "function (request, response) { throw new Error('boom'); }"});

    let port = free_port();
    created(&client, &admin, &inject_imposter(port, throwing.clone())).await;
    let response = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["x-rift-decorate-error"], "true");
    assert_eq!(response.headers()["x-a"], "1");
    assert_eq!(response.text().await.expect("body"), "inj");
    let _ = manager.delete_imposter(port).await;

    let port = free_port();
    let mut strict = inject_imposter(port, throwing);
    strict["strictBehaviors"] = json!(true);
    created(&client, &admin, &strict).await;
    let response = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status().as_u16(), 500);
    assert_eq!(response.headers()["x-rift-decorate-error"], "true");
    let _ = manager.delete_imposter(port).await;
}

// ---- repeat on every response type --------------------------------------------------------------

#[tokio::test]
async fn repeat_in_the_block_cycles_every_response_type() {
    let (client, admin, manager) = start_admin().await;
    let upstream = free_port();
    created(
        &client,
        &admin,
        &json!({"port": upstream, "protocol": "http", "stubs": [{"responses": [{"is": {"body": "up"}}]}]}),
    )
    .await;

    let cases = [
        (
            "proxy",
            json!({"proxy": {"to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyTransparent"}}),
            "up",
        ),
        ("inject", json!({"inject": INJECT}), "inj"),
        (
            "fault",
            json!({"fault": "CONNECTION_RESET_BY_PEER"}),
            "<error>",
        ),
        (
            "_rift",
            json!({"_rift": {"script": {"engine": "rhai", "code": "fn respond(ctx) { http(200, \"x\") }"}}}),
            "x",
        ),
    ];
    for (shape, response, first) in cases {
        let port = free_port();
        let mut response = response;
        response["_behaviors"] = json!({"repeat": 2});
        let body = created(
            &client,
            &admin,
            &json!({"port": port, "protocol": "http", "stubs": [{"responses": [
                response, {"is": {"body": "next"}}
            ]}]}),
        )
        .await;
        assert_eq!(
            ignored_warnings(&body),
            Vec::<Value>::new(),
            "{shape}: {body}"
        );
        assert_eq!(
            bodies(&client, port, 3).await,
            vec![first, first, "next"],
            "{shape}"
        );
        let _ = manager.delete_imposter(port).await;
    }
    let _ = manager.delete_imposter(upstream).await;
}

/// `{"is": …, "repeat": 2}` is how Mountebank itself stores a repeat (and what `mb save` writes).
#[tokio::test]
async fn top_level_repeat_cycles_an_is_response() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    created(
        &client,
        &admin,
        &json!({"port": port, "protocol": "http", "stubs": [{"responses": [
            {"is": {"body": "a"}, "repeat": 2},
            {"is": {"body": "b"}}
        ]}]}),
    )
    .await;
    assert_eq!(bodies(&client, port, 4).await, vec!["a", "a", "b", "a"]);

    let fetched: Value = client
        .get(format!("{admin}/imposters/{port}"))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(
        fetched["stubs"][0]["responses"][0]["behaviors"],
        json!([{"repeat": 2}]),
        "{fetched}"
    );
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn top_level_repeat_cycles_an_inject_and_a_fault_response() {
    let (client, admin, manager) = start_admin().await;
    for (response, first) in [
        (json!({"inject": INJECT, "repeat": 2}), "inj"),
        (
            json!({"fault": "CONNECTION_RESET_BY_PEER", "repeat": 2}),
            "<error>",
        ),
    ] {
        let port = free_port();
        let body = created(
            &client,
            &admin,
            &json!({"port": port, "protocol": "http", "stubs": [{"responses": [
                response.clone(), {"is": {"body": "next"}}
            ]}]}),
        )
        .await;
        assert_eq!(ignored_warnings(&body), Vec::<Value>::new(), "{response}");
        assert_eq!(
            bodies(&client, port, 3).await,
            vec![first, first, "next"],
            "{response}"
        );
        let fetched: Value = client
            .get(format!("{admin}/imposters/{port}"))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json");
        assert_eq!(
            fetched["stubs"][0]["responses"][0]["behaviors"],
            json!([{"repeat": 2}]),
            "{fetched}"
        );
        let _ = manager.delete_imposter(port).await;
    }
}

/// Mountebank reads the top-level form; when both are written, that is the one that counts.
#[tokio::test]
async fn top_level_repeat_wins_over_the_block() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    created(
        &client,
        &admin,
        &json!({"port": port, "protocol": "http", "stubs": [{"responses": [
            {"is": {"body": "a"}, "repeat": 3, "_behaviors": {"repeat": 1, "wait": 1}},
            {"is": {"body": "b"}}
        ]}]}),
    )
    .await;
    assert_eq!(bodies(&client, port, 4).await, vec!["a", "a", "a", "b"]);
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn a_top_level_repeat_that_is_not_a_u32_is_refused() {
    let (client, admin, manager) = start_admin().await;
    for repeat in [json!(2.0), json!(4_294_967_296_u64), json!("2"), json!(-1)] {
        let port = free_port();
        let response = create(
            &client,
            &admin,
            &json!({"port": port, "protocol": "http", "stubs": [{"responses": [
                {"is": {"body": "a"}, "repeat": repeat}
            ]}]}),
        )
        .await;
        assert_eq!(response.status().as_u16(), 400, "{repeat}");
        let text = response.text().await.expect("body");
        assert!(text.contains("`repeat`"), "{repeat}: {text}");
        let _ = manager.delete_imposter(port).await;
    }
}

/// Both are held to the rule, as rift-lint holds them: the top-level one replacing a malformed
/// block `repeat` does not make the file valid.
#[tokio::test]
async fn a_malformed_block_repeat_is_refused_under_a_valid_top_level_one() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    let imposter = json!({"port": port, "protocol": "http", "stubs": [{"responses": [
        {"is": {"body": "a"}, "repeat": 2, "_behaviors": {"repeat": "bad"}}
    ]}]});
    let response = create(&client, &admin, &imposter).await;
    assert_eq!(response.status().as_u16(), 400);
    let text = response.text().await.expect("body");
    assert!(text.contains("`repeat`"), "{text}");
    let lint = rift_lint::lint_json(
        &imposter.to_string(),
        "fixture.json",
        &rift_lint::LintOptions::default(),
    );
    let e035: Vec<_> = lint
        .issues
        .iter()
        .filter(|i| i.code == "E035")
        .map(|i| i.location.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        e035,
        vec!["stubs[0].responses[0]._behaviors.repeat".to_string()]
    );
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn a_null_top_level_repeat_is_absent() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    created(
        &client,
        &admin,
        &json!({"port": port, "protocol": "http", "stubs": [{"responses": [
            {"is": {"body": "a"}, "repeat": null, "_behaviors": {"repeat": 2}},
            {"is": {"body": "b"}}
        ]}]}),
    )
    .await;
    assert_eq!(bodies(&client, port, 3).await, vec!["a", "a", "b"]);
    let _ = manager.delete_imposter(port).await;
}

// ---- what is still reported as ignored ----------------------------------------------------------

#[tokio::test]
async fn a_behaviors_block_is_reported_only_where_something_in_it_is_ignored() {
    let (client, admin, manager) = start_admin().await;
    let with = |response: Value, behaviors: Value| {
        let mut response = response;
        response["_behaviors"] = behaviors;
        response
    };
    let fault = json!({"fault": "CONNECTION_RESET_BY_PEER"});
    let proxy = json!({"proxy": {"to": "http://127.0.0.1:1"}});
    let stubs: Vec<Value> = [
        with(json!({"inject": INJECT}), json!({"wait": 1})),
        with(fault.clone(), json!({"repeat": 2})),
        with(proxy.clone(), json!({"repeat": 2})),
        with(fault.clone(), json!({"repeat": 2, "wait": null})),
        with(fault, json!({"repeat": 2, "wait": 1})),
        with(proxy, json!({"wait": 1})),
    ]
    .into_iter()
    .map(|r| json!({"responses": [r]}))
    .collect();
    let port = free_port();
    let imposter = json!({"port": port, "protocol": "http", "stubs": stubs});
    let body = created(&client, &admin, &imposter).await;
    let messages: Vec<Value> = ignored_warnings(&body)
        .into_iter()
        .map(|w| w["message"].clone())
        .collect();
    assert_eq!(
        messages,
        vec![
            json!(
                "A behaviors block on a `proxy` response has no effect except `repeat`: \
                 Mountebank applies the rest, Rift does not yet (stubs 5)"
            ),
            json!(
                "A behaviors block on a `fault` response has no effect except `repeat`: the \
                 rest do not apply to a fault, as in Mountebank (stubs 4)"
            ),
        ],
        "{body}"
    );
    let _ = manager.delete_imposter(port).await;

    let lint = rift_lint::lint_json(
        &imposter.to_string(),
        "fixture.json",
        &rift_lint::LintOptions::default(),
    );
    let w017: Vec<_> = lint
        .issues
        .iter()
        .filter(|i| i.code == "W017")
        .map(|i| i.location.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        w017,
        vec![
            "stubs[4].responses[0]._behaviors".to_string(),
            "stubs[5].responses[0]._behaviors".to_string(),
        ],
        "{:?}",
        lint.issues
    );
}

// ---- the linter reads the top-level form like the engine ----------------------------------------

#[test]
fn the_linter_validates_a_top_level_repeat_like_the_engine() {
    let lint = |repeat: Value| {
        let imposter = json!({"port": 4545, "protocol": "http", "stubs": [{"responses": [
            {"is": {"body": "a"}, "repeat": repeat}
        ]}]});
        rift_lint::lint_json(
            &imposter.to_string(),
            "fixture.json",
            &rift_lint::LintOptions::default(),
        )
        .issues
        .into_iter()
        .filter(|i| i.code == "E035")
        .map(|i| i.location.unwrap_or_default())
        .collect::<Vec<_>>()
    };
    assert_eq!(lint(json!(3)), Vec::<String>::new());
    assert_eq!(lint(json!(null)), Vec::<String>::new());
    for bad in [json!(2.0), json!(4_294_967_296_u64), json!("2"), json!(0)] {
        assert_eq!(
            lint(bad.clone()),
            vec!["stubs[0].responses[0].repeat".to_string()],
            "{bad}"
        );
    }
}
