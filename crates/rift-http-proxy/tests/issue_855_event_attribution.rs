//! Issue #855: change events say *what* changed but not *who* changed it, which makes them
//! unusable for the audit logging they were partly built for (issue #316). `EventContext` carries
//! the attribution on the **listener signature** rather than on `ImposterEvent`, so the enum — and
//! every downstream `match` on it — is untouched.
//!
//! The principal originates in `AuthzDecision::Allow { principal }` (issue #854) at the admin
//! listener and has to reach `ImposterManager::emit` deep in `rift-mock-core`, through manager
//! methods that take no principal. It travels as a `tokio` task-local, mirroring the existing
//! `with_annotation_scope` seam. That mechanism has one real boundary and these tests pin both
//! sides of it: inside a request task the principal is present; outside any request — a config-file
//! load, an embedder calling the manager directly — it is `None` rather than wrong.

use parking_lot::Mutex;
use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use rift_mock_core::extensions::authz::{AdminAuthorizer, AuthzDecision, AuthzRequest};
use rift_mock_core::imposter::{EventContext, ImposterEvent, ImposterEventListener};
use std::sync::Arc;

fn imposter_cfg(port: u16) -> ImposterConfig {
    serde_json::from_value(serde_json::json!({
        "port": port, "protocol": "http", "stubs": []
    }))
    .expect("test imposter config")
}

/// Records each event together with the attribution it arrived with, so assertions read as
/// "this change was attributed to this principal".
struct RecordingListener {
    seen: Mutex<Vec<(ImposterEvent, Option<String>)>>,
}

impl RecordingListener {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
        })
    }
    fn seen(&self) -> Vec<(ImposterEvent, Option<String>)> {
        self.seen.lock().clone()
    }
    /// The attribution recorded for the first event matching `pred`.
    fn principal_for(&self, pred: impl Fn(&ImposterEvent) -> bool) -> Option<String> {
        self.seen
            .lock()
            .iter()
            .find(|(e, _)| pred(e))
            .expect("no matching event was emitted")
            .1
            .clone()
    }
}

impl ImposterEventListener for RecordingListener {
    fn on_event(&self, event: &ImposterEvent, ctx: &EventContext) {
        self.seen
            .lock()
            .push((event.clone(), ctx.principal.clone()));
    }
}

struct FixedAuthorizer(AuthzDecision);

impl AdminAuthorizer for FixedAuthorizer {
    fn authorize(&self, _req: AuthzRequest<'_>) -> AuthzDecision {
        self.0.clone()
    }
}

/// Start an admin API on an ephemeral port over a manager carrying `listener`.
async fn start(
    authorizer: Option<AuthzDecision>,
) -> (
    String,
    Arc<RecordingListener>,
    rift_http_proxy::admin_api::RunningAdminApi,
) {
    let listener = RecordingListener::new();
    let manager = Arc::new(
        ImposterManager::new()
            .with_event_listener(Arc::clone(&listener) as Arc<dyn ImposterEventListener>),
    );
    let mut server = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None);
    if let Some(decision) = authorizer {
        server = server.with_admin_authorizer(Arc::new(FixedAuthorizer(decision)));
    }
    let running = server.bind().await.expect("admin API binds");
    let base = format!("http://{}", running.local_addr());
    (base, listener, running)
}

// AC3: the whole point — an authorizer that names a principal makes that name reach the change
// event, so an audit trail can record who created the imposter without correlating request logs
// out of band.
#[tokio::test]
async fn a_named_principal_reaches_the_change_event() {
    let (base, listener, running) = start(Some(AuthzDecision::Allow {
        principal: Some("alice".to_string()),
    }))
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/imposters"))
        .json(&serde_json::json!({"port": 18551, "protocol": "http", "stubs": []}))
        .send()
        .await
        .expect("create imposter");
    assert_eq!(resp.status(), 201);

    assert_eq!(
        listener.principal_for(|e| matches!(e, ImposterEvent::Created(18551))),
        Some("alice".to_string()),
        "the creating principal must be attributed to the Created event"
    );

    running.shutdown().await;
}

// AC4: the seam's stated premise — install no authorizer and nothing changes. This is what makes
// the feature safe to ship: an embedder who wants none of it sees identical behaviour and data.
#[tokio::test]
async fn no_authorizer_installed_leaves_the_principal_unset() {
    let (base, listener, running) = start(None).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/imposters"))
        .json(&serde_json::json!({"port": 18552, "protocol": "http", "stubs": []}))
        .send()
        .await
        .expect("create imposter");
    assert_eq!(resp.status(), 201);

    assert_eq!(
        listener.principal_for(|e| matches!(e, ImposterEvent::Created(18552))),
        None,
        "with no authorizer installed there is no principal to attribute"
    );

    running.shutdown().await;
}

// AC5: an authorizer may allow without identifying anyone — that is exactly what the built-in
// api-key gate does (`AuthzDecision::allow()`). Allowing must not invent an attribution.
#[tokio::test]
async fn an_allow_without_a_principal_leaves_it_unset() {
    let (base, listener, running) = start(Some(AuthzDecision::allow())).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/imposters"))
        .json(&serde_json::json!({"port": 18553, "protocol": "http", "stubs": []}))
        .send()
        .await
        .expect("create imposter");
    assert_eq!(resp.status(), 201);

    assert_eq!(
        listener.principal_for(|e| matches!(e, ImposterEvent::Created(18553))),
        None,
        "an Allow that names nobody must not manufacture a principal"
    );

    running.shutdown().await;
}

// AC7: the bound the issue states up front — `AllDeleted` carries no port, so a fleet-wide delete
// records the actor but not a per-resource target. Pinned deliberately: this is the documented
// shape, and a future change to it should have to edit this assertion consciously.
#[tokio::test]
async fn a_fleet_wide_delete_attributes_the_actor_but_names_no_port() {
    let (base, listener, running) = start(Some(AuthzDecision::Allow {
        principal: Some("carol".to_string()),
    }))
    .await;

    let client = reqwest::Client::new();
    client
        .post(format!("{base}/imposters"))
        .json(&serde_json::json!({"port": 18554, "protocol": "http", "stubs": []}))
        .send()
        .await
        .expect("create imposter");
    let resp = client
        .delete(format!("{base}/imposters"))
        .send()
        .await
        .expect("delete all");
    assert_eq!(resp.status(), 200);

    assert_eq!(
        listener.principal_for(|e| matches!(e, ImposterEvent::AllDeleted)),
        Some("carol".to_string()),
        "a fleet-wide delete must still record who did it"
    );

    running.shutdown().await;
}

// The four remaining event variants. `Created`/`AllDeleted` above went through one route each, so
// without these the majority of the enum — and the stub and enable/disable code paths — had no
// attribution coverage at all. They matter because the mechanism's stated failure mode is silent:
// an emit that ever moves into a spawned task attributes `None` with nothing to catch it, and
// `apply_config`'s per-port loop is exactly the kind of code someone parallelises later.

// `Deleted`, via a single-imposter delete.
#[tokio::test]
async fn a_single_imposter_delete_is_attributed() {
    let (base, listener, running) = start(Some(AuthzDecision::Allow {
        principal: Some("dave".to_string()),
    }))
    .await;
    let client = reqwest::Client::new();
    client
        .post(format!("{base}/imposters"))
        .json(&serde_json::json!({"port": 18556, "protocol": "http", "stubs": []}))
        .send()
        .await
        .expect("create imposter");
    let resp = client
        .delete(format!("{base}/imposters/18556"))
        .send()
        .await
        .expect("delete imposter");
    assert_eq!(resp.status(), 200);

    assert_eq!(
        listener.principal_for(|e| matches!(e, ImposterEvent::Deleted(18556))),
        Some("dave".to_string()),
        "deleting one imposter must record who deleted it"
    );
    running.shutdown().await;
}

// `StubsChanged`, via the stub-append route — a different handler and manager method from the
// imposter-level CRUD above.
#[tokio::test]
async fn a_stub_mutation_is_attributed() {
    let (base, listener, running) = start(Some(AuthzDecision::Allow {
        principal: Some("erin".to_string()),
    }))
    .await;
    let client = reqwest::Client::new();
    client
        .post(format!("{base}/imposters"))
        .json(&serde_json::json!({"port": 18557, "protocol": "http", "stubs": []}))
        .send()
        .await
        .expect("create imposter");
    let resp = client
        .post(format!("{base}/imposters/18557/stubs"))
        .json(&serde_json::json!({
            "stub": {
                "predicates": [{"equals": {"path": "/ping"}}],
                "responses": [{"is": {"statusCode": 200, "body": "hi"}}]
            }
        }))
        .send()
        .await
        .expect("add stub");
    assert!(resp.status().is_success(), "add stub: {}", resp.status());

    assert_eq!(
        listener.principal_for(|e| matches!(e, ImposterEvent::StubsChanged(18557))),
        Some("erin".to_string()),
        "a stub edit must record who made it"
    );
    running.shutdown().await;
}

// `EnabledChanged`, via the serve/pause toggle — its own manager method again.
#[tokio::test]
async fn an_enabled_toggle_is_attributed() {
    let (base, listener, running) = start(Some(AuthzDecision::Allow {
        principal: Some("frank".to_string()),
    }))
    .await;
    let client = reqwest::Client::new();
    client
        .post(format!("{base}/imposters"))
        .json(&serde_json::json!({"port": 18558, "protocol": "http", "stubs": []}))
        .send()
        .await
        .expect("create imposter");
    let resp = client
        .post(format!("{base}/imposters/18558/disable"))
        .send()
        .await
        .expect("disable imposter");
    assert!(resp.status().is_success(), "disable: {}", resp.status());

    assert_eq!(
        listener.principal_for(|e| matches!(
            e,
            ImposterEvent::EnabledChanged {
                port: 18558,
                enabled: false
            }
        )),
        Some("frank".to_string()),
        "pausing an imposter must record who paused it"
    );
    running.shutdown().await;
}

// Cross-request isolation. This is the one property here guaranteed only by an implementation
// detail — `tokio`'s task-local is scoped to the *future*, so concurrent requests cannot see each
// other's principal. Nothing in application logic enforces it, so a refactor that hoisted the
// principal onto the connection (a shared `Mutex<Option<String>>`, say) would leak one caller's
// identity into another's audit record and every single-request test above would still pass.
#[tokio::test]
async fn concurrent_requests_do_not_cross_attribute() {
    // Attribution echoes a per-request header, so the two in-flight requests carry different
    // principals through one server.
    struct HeaderAuthorizer;
    impl AdminAuthorizer for HeaderAuthorizer {
        fn authorize(&self, req: AuthzRequest<'_>) -> AuthzDecision {
            AuthzDecision::Allow {
                principal: req.credential.map(str::to_string),
            }
        }
    }

    let listener = RecordingListener::new();
    let manager = Arc::new(
        ImposterManager::new()
            .with_event_listener(Arc::clone(&listener) as Arc<dyn ImposterEventListener>),
    );
    let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None)
        .with_admin_authorizer(Arc::new(HeaderAuthorizer))
        .bind()
        .await
        .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());

    let client = reqwest::Client::new();
    let one = client
        .post(format!("{base}/imposters"))
        .header("authorization", "grace")
        .json(&serde_json::json!({"port": 18559, "protocol": "http", "stubs": []}))
        .send();
    let two = client
        .post(format!("{base}/imposters"))
        .header("authorization", "heidi")
        .json(&serde_json::json!({"port": 18560, "protocol": "http", "stubs": []}))
        .send();
    let (r1, r2) = tokio::join!(one, two);
    assert_eq!(r1.expect("create 18559").status(), 201);
    assert_eq!(r2.expect("create 18560").status(), 201);

    assert_eq!(
        listener.principal_for(|e| matches!(e, ImposterEvent::Created(18559))),
        Some("grace".to_string()),
        "each imposter must be attributed to the caller that created it"
    );
    assert_eq!(
        listener.principal_for(|e| matches!(e, ImposterEvent::Created(18560))),
        Some("heidi".to_string()),
        "a concurrent request must not inherit the other caller's principal"
    );

    running.shutdown().await;
}

// AC6: the task-local's boundary, asserted rather than assumed. An embedder driving the manager
// directly — or a config-file load at startup — runs outside any request scope. Reading the
// principal there must yield `None`, not panic and not leak an unrelated request's principal.
#[tokio::test]
async fn a_direct_manager_mutation_outside_any_request_has_no_principal() {
    let listener = RecordingListener::new();
    let manager = Arc::new(
        ImposterManager::new()
            .with_event_listener(Arc::clone(&listener) as Arc<dyn ImposterEventListener>),
    );

    manager
        .create_imposter(imposter_cfg(18555))
        .await
        .expect("create imposter directly");

    assert_eq!(
        listener.principal_for(|e| matches!(e, ImposterEvent::Created(18555))),
        None,
        "outside a request scope there is no principal; reading one must be None, not a panic"
    );
    assert_eq!(listener.seen().len(), 1);
}

// AC8: `ImposterEvent` is untouched. This match is exhaustive with NO wildcard arm, so adding,
// removing or reshaping a variant fails to compile here — which is the guarantee the issue makes
// ("existing matches keep compiling") expressed as a test rather than as prose.
#[test]
fn the_event_enum_is_unchanged() {
    fn describe(e: &ImposterEvent) -> &'static str {
        match e {
            ImposterEvent::Created(_) => "created",
            ImposterEvent::Replaced(_) => "replaced",
            ImposterEvent::StubsChanged(_) => "stubs-changed",
            ImposterEvent::EnabledChanged { .. } => "enabled-changed",
            ImposterEvent::Deleted(_) => "deleted",
            ImposterEvent::AllDeleted => "all-deleted",
        }
    }
    assert_eq!(describe(&ImposterEvent::Created(1)), "created");
    assert_eq!(describe(&ImposterEvent::AllDeleted), "all-deleted");
}
