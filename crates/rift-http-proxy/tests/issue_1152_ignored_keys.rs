//! Issue #1152: keys and flags the engine parsed and then ignored, with nothing said at any level.
//! The value read back unchanged, so nothing distinguished "honoured" from "dropped".
//!
//! Imposter keys are now reported where the author sees them — `_rift.warnings` on create and GET,
//! plus a load-time log line for the doors with no response — and `rift-lint` flags the same keys.
//! The engine's list and the linter's are kept together by the sync test below, because the linter
//! works on raw JSON and cannot call the engine.

mod support;

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

/// One fixture per ignored key, each with the location the linter reports it at.
fn fixtures(port: u16) -> Vec<(&'static str, Value)> {
    let base = |extra: Value| {
        let mut imposter = json!({"port": port, "protocol": "http", "stubs": [
            {"responses": [{"is": {"statusCode": 200}}]}
        ]});
        for (k, v) in extra.as_object().expect("object") {
            imposter[k] = v.clone();
        }
        imposter
    };
    vec![
        (
            "_rift.metrics",
            base(json!({"_rift": {"metrics": {"enabled": true}}})),
        ),
        (
            "_rift.proxy",
            base(json!({"_rift": {"proxy": {"upstream": {"host": "x", "port": 1}}}})),
        ),
        ("recordMatches", base(json!({"recordMatches": true}))),
        (
            "stubs[0].responses[0]._rift",
            base(json!({"stubs": [{"responses": [{
                "inject": "function (config) { return {}; }",
                "_rift": {"templated": true}
            }]}]})),
        ),
        (
            "stubs[0].responses[0]._rift",
            base(json!({"stubs": [{"responses": [{
                "proxy": {"to": "http://127.0.0.1:1"},
                "_rift": {"templated": true}
            }]}]})),
        ),
        (
            "stubs[0].responses[0]._rift",
            base(json!({"stubs": [{"responses": [{
                "fault": "CONNECTION_RESET_BY_PEER",
                "_rift": {"templated": true}
            }]}]})),
        ),
        // Issue #1181: a behaviors block on a response no behavior runs on.
        (
            "stubs[0].responses[0]._behaviors",
            base(json!({"stubs": [{"responses": [{
                "proxy": {"to": "http://127.0.0.1:1"},
                "_behaviors": {"wait": 500}
            }]}]})),
        ),
        (
            "stubs[0].responses[0]._behaviors",
            base(json!({"stubs": [{"responses": [{
                "inject": "function (config) { return {}; }",
                "_behaviors": {"wait": 500}
            }]}]})),
        ),
        (
            "stubs[0].responses[0]._behaviors",
            base(json!({"stubs": [{"responses": [{
                "fault": "CONNECTION_RESET_BY_PEER",
                "_behaviors": {"wait": 500}
            }]}]})),
        ),
        (
            "stubs[0].responses[0]._behaviors",
            base(json!({"stubs": [{"responses": [{
                "_rift": {"script": {"engine": "rhai", "code": "fn respond(ctx) { http(200, \"x\") }"}},
                "_behaviors": {"wait": 500}
            }]}]})),
        ),
        (
            "stubs[0].responses[0].behaviors",
            base(json!({"stubs": [{"responses": [{
                "proxy": {"to": "http://127.0.0.1:1"},
                "behaviors": [{"wait": 500}]
            }]}]})),
        ),
    ]
}

async fn start_admin() -> (reqwest::Client, String, Arc<ImposterManager>) {
    start_admin_with(true).await
}

async fn start_admin_with(
    allow_injection: bool,
) -> (reqwest::Client, String, Arc<ImposterManager>) {
    let manager = Arc::new(ImposterManager::new());
    let admin_port = free_port();
    let server = AdminApiServer::new(
        format!("127.0.0.1:{admin_port}").parse().expect("addr"),
        manager.clone(),
        None,
    )
    .with_allow_injection(allow_injection);
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

/// Engine and linter agree on every ignored key: each fixture draws exactly one
/// `config_key_ignored` from the engine (on create and on GET) and exactly one W017 from the
/// linter, at the location the fixture names. The fixtures are the list both are held to — a key
/// added to either side needs a fixture here, or this test cannot see it.
#[tokio::test]
async fn the_engine_and_the_linter_report_the_same_ignored_keys() {
    let (client, admin, manager) = start_admin().await;
    for (location, imposter) in fixtures(free_port()) {
        let port = imposter["port"].as_u64().expect("port");
        let created = client
            .post(format!("{admin}/imposters"))
            .json(&imposter)
            .send()
            .await
            .expect("POST");
        assert_eq!(created.status().as_u16(), 201, "{location}");
        let created: Value = created.json().await.expect("json");
        let from_create = ignored_warnings(&created);
        assert_eq!(from_create.len(), 1, "{location}: {created}");
        let message = from_create[0]["message"].as_str().expect("message");
        let key = location.rsplit('.').next().expect("key");
        // The engine cannot tell which spelling a behaviors block was written in, so it names neither.
        let named = if key.ends_with("behaviors") {
            "A behaviors block"
        } else {
            key
        };
        assert!(message.contains(named), "{location}: {message}");
        if location.starts_with("stubs[0]") {
            assert_eq!(from_create[0]["stubIndex"], 0, "{location}");
        }

        let fetched: Value = client
            .get(format!("{admin}/imposters/{port}"))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json");
        assert_eq!(ignored_warnings(&fetched), from_create, "{location}");
        let _ = manager.delete_imposter(port as u16).await;

        let lint = rift_lint::lint_json(
            &imposter.to_string(),
            "fixture.json",
            &rift_lint::LintOptions::default(),
        );
        let w017: Vec<_> = lint.issues.iter().filter(|i| i.code == "W017").collect();
        assert_eq!(w017.len(), 1, "{location}: {:?}", lint.issues);
        assert_eq!(w017[0].location.as_deref(), Some(location));
    }
}

#[tokio::test]
async fn an_imposter_with_no_ignored_key_gets_no_such_warning() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    let created: Value = client
        .post(format!("{admin}/imposters"))
        .json(
            &json!({"port": port, "protocol": "http", "recordMatches": false,
            "stubs": [
                {"responses": [{"is": {"statusCode": 200}}]},
                // Behaviors run on an `is` response (and on the flat form), so they are not ignored.
                {"responses": [{"is": {"statusCode": 200}, "_behaviors": {"wait": 1}}]},
                {"responses": [{"statusCode": 200, "behaviors": [{"wait": 1}]}]},
                // An empty or null block on a proxy is nothing to report.
                {"responses": [{"proxy": {"to": "http://127.0.0.1:1"}, "_behaviors": {}}]},
                {"responses": [{"proxy": {"to": "http://127.0.0.1:1"}, "_behaviors": null}]}
            ]}),
        )
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("json");
    assert!(ignored_warnings(&created).is_empty(), "{created}");
    let _ = manager.delete_imposter(port).await;
}

/// A generated imposter can put `_rift` on every response; the report stays one entry per shape,
/// so it cannot outgrow the stub-analysis bound (#423).
#[tokio::test]
async fn many_ignored_blocks_collapse_into_one_warning_per_shape() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    let stubs: Vec<Value> = (0..150)
        .map(|_| json!({"responses": [{"proxy": {"to": "http://127.0.0.1:1"}, "_rift": {}}]}))
        .collect();
    let created: Value = client
        .post(format!("{admin}/imposters"))
        .json(&json!({"port": port, "protocol": "http", "stubs": stubs}))
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("json");
    let ignored = ignored_warnings(&created);
    assert_eq!(ignored.len(), 1, "{created}");
    let message = ignored[0]["message"].as_str().expect("message");
    assert!(message.contains("and 140 more"), "{message}");
    let _ = manager.delete_imposter(port).await;
}

/// The doors that never see an API response — `--configfile` here — still get a log line, from
/// the same list; and the two Mountebank flags that do nothing now say so at startup.
#[test]
fn the_binary_logs_ignored_keys_and_flags() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("imposters.json");
    let imposter_port = free_port();
    std::fs::write(
        &config,
        json!({"imposters": [{"port": imposter_port, "protocol": "http",
            "_rift": {"metrics": {"enabled": true}}, "stubs": []}]})
        .to_string(),
    )
    .expect("write");
    let log = dir.path().join("rift.log");
    let admin_port = free_port();
    let mut child = std::process::Command::new(support::server_bin())
        .args([
            "--port",
            &admin_port.to_string(),
            "--host",
            "127.0.0.1",
            "--metrics-port",
            "0",
            "--configfile",
            config.to_str().expect("utf8"),
            "--log",
            log.to_str().expect("utf8"),
            "--origin",
            "http://example.test",
            "--mock",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    let wanted = [
        "`_rift.metrics` has no effect",
        "--origin is accepted",
        "--mock is accepted",
    ];
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut text = String::new();
    while Instant::now() < deadline {
        text = std::fs::read_to_string(&log).unwrap_or_default();
        if wanted.iter().all(|w| text.contains(w)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    for w in wanted {
        assert!(text.contains(w), "missing {w:?} in: {text}");
    }
}

/// Issue #1181: the same one-entry-per-shape bound for behaviors, with the indices it names.
#[tokio::test]
async fn many_ignored_behaviors_collapse_into_one_warning_per_shape() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    let mut stubs: Vec<Value> = vec![json!({"responses": [{"is": {"statusCode": 200}}]})];
    stubs.extend((0..12).map(
        |_| json!({"responses": [{"proxy": {"to": "http://127.0.0.1:1"}, "_behaviors": {"wait": 1}}]}),
    ));
    let created: Value = client
        .post(format!("{admin}/imposters"))
        .json(&json!({"port": port, "protocol": "http", "stubs": stubs}))
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("json");
    let ignored = ignored_warnings(&created);
    assert_eq!(ignored.len(), 1, "{created}");
    assert_eq!(ignored[0]["stubIndex"], 1);
    assert_eq!(
        ignored[0]["message"],
        "A behaviors block on a `proxy` response has no effect: behaviors apply to `is` \
         responses only; Mountebank applies them, Rift does not yet (stubs 1, 2, 3, 4, 5, 6, 7, \
         8, 9, 10 and 2 more)"
    );
    let _ = manager.delete_imposter(port).await;
}

/// Issue #1181: a scripted behavior on a proxy response is a script surface even though Rift does
/// not run it yet — Mountebank does, so the gate must not be open the day Rift starts to.
#[tokio::test]
async fn a_scripted_behavior_on_a_proxy_response_needs_allow_injection() {
    let imposter = |port: u16, behaviors: Value| {
        json!({"port": port, "protocol": "http", "stubs": [{"responses": [{
            "proxy": {"to": "http://127.0.0.1:1"}, "_behaviors": behaviors
        }]}]})
    };
    let decorate = json!({"decorate": "function (request, response) { response.body = 'x'; }"});

    let (client, admin, manager) = start_admin_with(false).await;
    let port = free_port();
    let refused = client
        .post(format!("{admin}/imposters"))
        .json(&imposter(port, decorate.clone()))
        .send()
        .await
        .expect("POST");
    assert_eq!(refused.status().as_u16(), 400);
    let plain = client
        .post(format!("{admin}/imposters"))
        .json(&imposter(port, json!({"wait": 500})))
        .send()
        .await
        .expect("POST");
    assert_eq!(plain.status().as_u16(), 201);
    let _ = manager.delete_imposter(port).await;

    let (client, admin, manager) = start_admin_with(true).await;
    let port = free_port();
    let admitted = client
        .post(format!("{admin}/imposters"))
        .json(&imposter(port, decorate))
        .send()
        .await
        .expect("POST");
    assert_eq!(admitted.status().as_u16(), 201);
    let _ = manager.delete_imposter(port).await;
}

/// Issue #1181: each shape says what is true of it — Mountebank applies behaviors on `proxy` and
/// `inject`, ignores them on `fault`, and has no `_rift`-only response.
#[tokio::test]
async fn each_ignored_behaviors_shape_has_its_own_message() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    let with = |response: Value| {
        let mut response = response;
        response["_behaviors"] = json!({"wait": 1});
        json!({"responses": [response]})
    };
    let stubs = vec![
        with(json!({"inject": "function (config) { return {}; }"})),
        with(json!({"fault": "CONNECTION_RESET_BY_PEER"})),
        with(
            json!({"_rift": {"script": {"engine": "rhai", "code": "fn respond(ctx) { http(200, \"x\") }"}}}),
        ),
    ];
    let created: Value = client
        .post(format!("{admin}/imposters"))
        .json(&json!({"port": port, "protocol": "http", "stubs": stubs}))
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("json");
    let messages: Vec<Value> = ignored_warnings(&created)
        .into_iter()
        .map(|w| w["message"].clone())
        .collect();
    assert_eq!(
        messages,
        vec![
            json!(
                "A behaviors block on an `inject` response has no effect: behaviors apply to `is` \
                 responses only; Mountebank applies them, Rift does not yet (stubs 0)"
            ),
            json!(
                "A behaviors block on a `fault` response has no effect: behaviors apply to `is` \
                 responses only, as in Mountebank (stubs 1)"
            ),
            json!(
                "A behaviors block on a `_rift`-only response has no effect: behaviors apply to \
                 `is` responses only (stubs 2)"
            ),
        ]
    );
    let _ = manager.delete_imposter(port).await;
}
