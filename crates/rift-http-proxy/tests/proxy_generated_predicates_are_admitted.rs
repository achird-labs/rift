//! Issue #1353: a proxy records only predicates the engine would admit. A generated predicate that
//! did not decode was dropped with a warn, and when every one was dropped the recording became a
//! match-all stub that answered every later request with the first response. An unknown
//! `predicateOperator` is now refused at the config door; anything a generator produces that does
//! not decode or validate is a generation failure (#498): nothing recorded, the response forwarded
//! with `x-rift-generator-error: invalid-predicate`.

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

struct Admin {
    client: reqwest::Client,
    url: String,
    manager: Arc<ImposterManager>,
}

impl Admin {
    async fn start() -> Self {
        let manager = Arc::new(ImposterManager::new());
        let running =
            AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
                .bind()
                .await
                .expect("admin API binds");
        Admin {
            client: reqwest::Client::new(),
            url: format!("http://{}", running.local_addr()),
            manager,
        }
    }

    async fn post(&self, imposter: &Value) -> (u16, String) {
        let response = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(imposter)
            .send()
            .await
            .expect("POST");
        let status = response.status().as_u16();
        (status, response.text().await.expect("body"))
    }

    async fn upstream(&self) -> u16 {
        let (status, body) = self
            .post(&json!({"protocol": "http", "recordRequests": true,
                "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "up"}}]}]}))
            .await;
        assert_eq!(status, 201, "{body}");
        port_of(&body)
    }

    async fn printed(&self, port: u16, query: &str) -> Value {
        self.client
            .get(format!("{}/imposters/{port}{query}", self.url))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json")
    }

    async fn upstream_hits(&self, port: u16) -> usize {
        self.printed(port, "").await["requests"]
            .as_array()
            .expect("requests")
            .len()
    }

    async fn stubs(&self, port: u16) -> Vec<Value> {
        self.printed(port, "").await["stubs"]
            .as_array()
            .expect("stubs")
            .clone()
    }

    async fn drop_all(&self, ports: &[u16]) {
        for port in ports {
            let _ = self.manager.delete_imposter(*port).await;
        }
    }
}

/// The port the engine assigned, from a 201 body.
fn port_of(body: &str) -> u16 {
    let created: Value = serde_json::from_str(body).expect("json");
    created["port"].as_u64().expect("assigned port") as u16
}

fn proxy(upstream: u16, generators: Value) -> Value {
    json!({"protocol": "http", "stubs": [{"responses": [{"proxy": {
        "to": format!("http://127.0.0.1:{upstream}"),
        "mode": "proxyOnce",
        "predicateGenerators": generators
    }}]}]})
}

async fn get(admin: &Admin, port: u16, path: &str) -> (Option<String>, String) {
    let response = admin
        .client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("request");
    let header = response
        .headers()
        .get("x-rift-generator-error")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (header, response.text().await.expect("body"))
}

#[tokio::test]
async fn an_unknown_predicate_operator_is_refused_at_create() {
    let admin = Admin::start().await;
    let (status, body) = admin
        .post(&proxy(
            1,
            json!([{"matches": {"path": true}, "predicateOperator": "bogus"}]),
        ))
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains(
            "predicateGenerators[0].predicateOperator `bogus` is not a predicate operator"
        ),
        "{body}"
    );
}

/// Issue #1413: a generator entry the recorder cannot read used to be skipped, so a generator list
/// nobody could read recorded a match-all stub. Each shape is refused at create, as Mountebank's
/// dry run refuses most of them.
#[tokio::test]
async fn an_unreadable_generator_entry_is_refused_at_create() {
    let admin = Admin::start().await;
    let cases = [
        (json!(["x"]), "predicateGenerators[0] must be an object"),
        (
            json!([{"matches": {"path": true}}, {}]),
            "predicateGenerators[1] needs `matches` or `inject`",
        ),
        (
            json!([{"inject": 42}]),
            "predicateGenerators[0].inject must be a function string",
        ),
        (
            json!([{"matches": true}]),
            "predicateGenerators[0].matches must be an object of request fields",
        ),
        (
            json!([{"matches": {"path": true}, "caseSensitive": "yes"}]),
            "predicateGenerators[0].caseSensitive must be a boolean",
        ),
        (
            json!([{"matches": {"path": true}, "except": 5}]),
            "predicateGenerators[0].except must be a string",
        ),
        (
            json!([{"matches": {"pathh": true}}]),
            "predicateGenerators[0].matches field `pathh` is not a request field",
        ),
    ];
    for (generators, message) in cases {
        let (status, body) = admin.post(&proxy(1, generators.clone())).await;
        assert_eq!(status, 400, "{generators}: {body}");
        assert!(body.contains(message), "{generators}: {body}");
    }
}

#[tokio::test]
async fn an_inject_generator_returning_a_non_predicate_records_nothing() {
    let admin = Admin::start().await;
    let upstream = admin.upstream().await;
    // Created through the manager: the admin API refuses `inject` without `--allowInjection`.
    let config = serde_json::from_value(proxy(
        upstream,
        json!([{"inject":
            "function(config) { return [{ equal: { path: config.request.path } }]; }"}]),
    ))
    .expect("imposter config");
    let port = admin
        .manager
        .create_imposter(config)
        .await
        .expect("create proxy");

    let (header, body) = get(&admin, port, "/a").await;
    assert_eq!(header.as_deref(), Some("invalid-predicate"));
    assert_eq!(body, "up");
    let (_, body) = get(&admin, port, "/b").await;
    assert_eq!(
        body, "up",
        "/b is forwarded, not answered by /a's recording"
    );
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    assert_eq!(admin.stubs(port).await.len(), 1, "no stub was recorded");
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn a_matches_operator_over_a_non_regex_path_records_nothing() {
    let admin = Admin::start().await;
    let upstream = admin.upstream().await;
    let (status, body) = admin
        .post(&proxy(
            upstream,
            json!([{"matches": {"path": true}, "predicateOperator": "matches"}]),
        ))
        .await;
    assert_eq!(status, 201, "{body}");
    let port = port_of(&body);

    let (header, body) = get(&admin, port, "/a(b").await;
    assert_eq!(header.as_deref(), Some("invalid-predicate"));
    assert_eq!(body, "up");
    assert_eq!(admin.stubs(port).await.len(), 1, "no stub was recorded");

    // The export is still one the engine admits.
    let mut export = admin.printed(port, "?replayable=true").await;
    admin.drop_all(&[port]).await;
    // Re-imported on a fresh port: the freed one may already be another test's.
    export.as_object_mut().expect("object").remove("port");
    let (status, body) = admin.post(&export).await;
    assert_eq!(status, 201, "the export re-imports: {body}");
    admin.drop_all(&[port_of(&body), upstream]).await;
}

#[tokio::test]
async fn every_valid_operator_still_records() {
    let admin = Admin::start().await;
    let upstream = admin.upstream().await;
    for operator in [
        "equals",
        "deepEquals",
        "contains",
        "startsWith",
        "endsWith",
        "matches",
        "exists",
    ] {
        let (status, body) = admin
            .post(&proxy(
                upstream,
                json!([{"matches": {"path": true}, "predicateOperator": operator}]),
            ))
            .await;
        assert_eq!(status, 201, "{operator}: {body}");
        let port = port_of(&body);
        let (header, _) = get(&admin, port, "/rec").await;
        assert_eq!(header, None, "{operator}");
        let stubs = admin.stubs(port).await;
        assert_eq!(
            stubs.len(),
            2,
            "{operator}: one recording in front of the proxy"
        );
        let predicates = stubs[0]["predicates"].as_array().expect("predicates");
        assert_eq!(predicates.len(), 1, "{operator}: {predicates:?}");
        assert!(
            predicates[0].get(operator).is_some(),
            "{operator}: {predicates:?}"
        );
        admin.drop_all(&[port]).await;
    }
    admin.drop_all(&[upstream]).await;
}

/// Issue #1446: `ip` and `form` are recorded from the request, so a recording replays to the
/// same client and form instead of failing the generation.
#[tokio::test]
async fn an_ip_and_form_generator_records_a_stub_that_replays() {
    let admin = Admin::start().await;
    let upstream = admin.upstream().await;
    let (status, body) = admin
        .post(&proxy(
            upstream,
            json!([{"matches": {"ip": true, "form": true}}]),
        ))
        .await;
    assert_eq!(status, 201, "{body}");
    let port = port_of(&body);

    let send = |form: &'static str| {
        admin
            .client
            .post(format!("http://127.0.0.1:{port}/submit"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form)
            .send()
    };
    for form in ["a=1", "a=1", "a=2", "a=2"] {
        let response = send(form).await.expect("request");
        assert!(
            response.headers().get("x-rift-generator-error").is_none(),
            "{form}: the generation must succeed"
        );
    }
    assert_eq!(
        admin.upstream_hits(upstream).await,
        2,
        "each form reaches the upstream once, then replays"
    );
    let stubs = admin.stubs(port).await;
    let recorded: Vec<&Value> = stubs
        .iter()
        .filter_map(|stub| stub["predicates"].as_array())
        .flatten()
        .collect();
    assert!(
        recorded
            .iter()
            .any(|p| p["deepEquals"]["form"] == json!({"a": "2"})),
        "{stubs:?}"
    );
    assert!(
        recorded
            .iter()
            .any(|p| p["deepEquals"]["ip"] == json!("127.0.0.1")),
        "{stubs:?}"
    );
    admin.drop_all(&[port, upstream]).await;
}
