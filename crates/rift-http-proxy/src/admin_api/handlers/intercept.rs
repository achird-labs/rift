//! Intercept runtime lifecycle + rule CRUD + CA/truststore export admin handlers (epic #394 slice
//! 4/5; runtime lifecycle issue #493).
//!
//! Everything here lives under `/intercept`. The lifecycle verbs (`POST`/`GET`/`DELETE /intercept`)
//! start, report, and stop the listener over the shared [`InterceptControl`] slot, so intercept can
//! be enabled at runtime on any server — not only one started with `--intercept-port`. The rule
//! CRUD + CA/truststore sub-routes operate on the running listener's [`InterceptState`] and keep
//! `404`-ing when no listener is running. All of this is only reachable when the server was built
//! `with_intercept(...)` — see `admin_api::router::route_request`.

use crate::admin_api::types::{collect_body, error_response, json_response};
use crate::intercept_control::{
    InterceptControl, InterceptStartError, InterceptStartOptions, InterceptStatus,
};
use crate::intercept_rules::{InterceptRule, InterceptState};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use rift_mock_core::proxy::truststore::{TrustStorePassword, ca_pem, export_jks, export_pkcs12};
use serde::Serialize;

const DEFAULT_TRUSTSTORE_PASSWORD: &str = "changeit";

/// Dispatch a `/intercept...` admin request. Returns `None` for any unmatched path/method so the
/// caller falls through to its normal `404` handling — including the rule/CA sub-routes when no
/// listener is running (`control.state()` is `None`).
pub async fn route(
    method: &Method,
    path: &str,
    query: Option<&str>,
    req: Request<Incoming>,
    control: &InterceptControl,
) -> Option<Response<Full<Bytes>>> {
    let rest = path.strip_prefix("/intercept")?;
    let resp = match (method, rest) {
        // Runtime lifecycle (issue #493) — operate on the shared slot, listener or not.
        (&Method::POST, "") => handle_start(req, control).await,
        (&Method::GET, "") => handle_status(control),
        (&Method::DELETE, "") => handle_stop(control).await,
        // Rule CRUD + CA/truststore — need a running listener's state. When none is running these
        // are a known route with an actionable body ("not running"), not the generic 404 an unknown
        // sub-path gets below — mirroring `GET /intercept`.
        (&Method::POST, "/rules") => match control.state() {
            Some(state) => handle_add_rules(req, &state).await,
            None => not_running(),
        },
        (&Method::GET, "/rules") => match control.state() {
            Some(state) => handle_list_rules(&state),
            None => not_running(),
        },
        (&Method::DELETE, "/rules") => match control.state() {
            Some(state) => handle_clear_rules(&state),
            None => not_running(),
        },
        (&Method::GET, "/ca.pem") => match control.state() {
            Some(state) => handle_ca_pem(&state),
            None => not_running(),
        },
        (&Method::GET, "/truststore.p12") => match control.state() {
            Some(state) => handle_truststore_p12(query, &state),
            None => not_running(),
        },
        (&Method::GET, "/truststore.jks") => match control.state() {
            Some(state) => handle_truststore_jks(query, &state),
            None => not_running(),
        },
        // Unmatched `/intercept...` path/method: let the caller apply its own 404 handling.
        _ => return None,
    };
    Some(resp)
}

/// The `404` an intercept sub-route returns when no listener is running — the same actionable body
/// `GET /intercept` uses, rather than a bare "Not Found".
fn not_running() -> Response<Full<Bytes>> {
    error_response(StatusCode::NOT_FOUND, "intercept listener not running")
}

/// `POST /intercept` — start the listener. Body is optional: absent, empty, or `{}` all mean
/// defaults (`127.0.0.1:0`, fresh in-memory CA). `201` with the [`InterceptStatus`] body on
/// success, `409` when already running, `400` for a bad body / options / CA / bind.
async fn handle_start(req: Request<Incoming>, control: &InterceptControl) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(e.status_code(), &e.to_string()),
    };
    start_from_bytes(&body, control).await
}

/// Parse start options from a (possibly empty) JSON body and drive `control.start`. Split out from
/// `handle_start` so the `201`/`409`/`400` mapping is unit-testable without a `Request<Incoming>`.
async fn start_from_bytes(body: &[u8], control: &InterceptControl) -> Response<Full<Bytes>> {
    let opts: InterceptStartOptions = if body.is_empty() {
        InterceptStartOptions::default()
    } else {
        match serde_json::from_slice(body) {
            Ok(o) => o,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid intercept start options: {e}"),
                );
            }
        }
    };
    match control.start(opts).await {
        Ok(addr) => json_response(StatusCode::CREATED, &InterceptStatus::from_addr(addr)),
        Err(e) => {
            let status = match e {
                InterceptStartError::AlreadyRunning => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            error_response(status, &e.to_string())
        }
    }
}

/// `GET /intercept` — `200` with the [`InterceptStatus`] of the running listener (whatever surface
/// started it), or `404` when none is running.
fn handle_status(control: &InterceptControl) -> Response<Full<Bytes>> {
    match control.status() {
        Some(addr) => json_response(StatusCode::OK, &InterceptStatus::from_addr(addr)),
        None => not_running(),
    }
}

/// `DELETE /intercept` — stop the listener and drop its rules + CA. Always `204`, idempotent: a
/// delete with nothing running is a successful no-op. A subsequent `POST` without CA paths mints a
/// fresh CA, so clients must re-export `/intercept/ca.pem` after a restart.
async fn handle_stop(control: &InterceptControl) -> Response<Full<Bytes>> {
    control.stop().await;
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Full::new(Bytes::new()))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build response",
            )
        })
}

/// A single rule or a batch — `POST /intercept/rules` accepts either shape.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum RuleOrRules {
    One(InterceptRule),
    Many(Vec<InterceptRule>),
}

/// `POST /intercept/rules` — add one rule (a bare `InterceptRule` object) or many (a JSON array).
async fn handle_add_rules(req: Request<Incoming>, state: &InterceptState) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(e.status_code(), &e.to_string()),
    };
    add_rules_from_bytes(&body, state)
}

/// Parse a rule (or array of rules) from a JSON body and add them to the store. Split out from
/// `handle_add_rules` so the parse/store path is unit-testable without a `Request<Incoming>`.
fn add_rules_from_bytes(body: &[u8], state: &InterceptState) -> Response<Full<Bytes>> {
    let parsed: RuleOrRules = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid intercept rule JSON: {e}"),
            );
        }
    };
    let added = match parsed {
        RuleOrRules::One(rule) => {
            if let Err(e) = state.rules.add(rule.clone()) {
                return error_response(StatusCode::TOO_MANY_REQUESTS, &e.to_string());
            }
            vec![rule]
        }
        RuleOrRules::Many(rules) => {
            if let Err(e) = state.rules.extend(rules.clone()) {
                return error_response(StatusCode::TOO_MANY_REQUESTS, &e.to_string());
            }
            rules
        }
    };
    json_response(StatusCode::CREATED, &added)
}

/// `GET /intercept/rules` — list all current rules.
fn handle_list_rules(state: &InterceptState) -> Response<Full<Bytes>> {
    json_response(StatusCode::OK, &state.rules.list())
}

#[derive(Debug, Serialize, serde::Deserialize)]
struct DeletedResponse {
    deleted: usize,
}

/// `DELETE /intercept/rules` — remove all rules, returning how many were removed.
fn handle_clear_rules(state: &InterceptState) -> Response<Full<Bytes>> {
    let deleted = state.rules.len();
    state.rules.clear();
    json_response(StatusCode::OK, &DeletedResponse { deleted })
}

/// `GET /intercept/ca.pem` — the intercept CA certificate, PEM-encoded.
fn handle_ca_pem(state: &InterceptState) -> Response<Full<Bytes>> {
    let pem = ca_pem(&state.ca);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-pem-file")
        .body(Full::new(Bytes::from(pem)))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build response",
            )
        })
}

/// Extract `password=` from a query string, defaulting to [`DEFAULT_TRUSTSTORE_PASSWORD`].
fn password_from_query(query: Option<&str>) -> String {
    query
        .and_then(|q| {
            q.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                (k == "password").then(|| {
                    urlencoding::decode(v)
                        .map(|d| d.into_owned())
                        .unwrap_or_else(|_| v.to_string())
                })
            })
        })
        .unwrap_or_else(|| DEFAULT_TRUSTSTORE_PASSWORD.to_string())
}

fn truststore_response(bytes: Vec<u8>, password: &str, filename: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header(
            "content-disposition",
            format!("attachment; filename=\"{filename}\""),
        )
        .header("x-truststore-password", password)
        .body(Full::new(Bytes::from(bytes)))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build response",
            )
        })
}

/// `GET /intercept/truststore.p12[?password=]` — PKCS#12 truststore containing the CA cert.
fn handle_truststore_p12(query: Option<&str>, state: &InterceptState) -> Response<Full<Bytes>> {
    let password = password_from_query(query);
    match export_pkcs12(&state.ca, &TrustStorePassword::new(password.clone())) {
        Ok(bytes) => truststore_response(bytes, &password, "rift-intercept-ca.p12"),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to export PKCS#12 truststore: {e}"),
        ),
    }
}

/// `GET /intercept/truststore.jks[?password=]` — JKS truststore containing the CA cert.
fn handle_truststore_jks(query: Option<&str>, state: &InterceptState) -> Response<Full<Bytes>> {
    let password = password_from_query(query);
    match export_jks(&state.ca, &TrustStorePassword::new(password.clone())) {
        Ok(bytes) => truststore_response(bytes, &password, "rift-intercept-ca.jks"),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to export JKS truststore: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intercept_rules::{InterceptAction, InterceptRules, ServeStub};
    use rift_mock_core::proxy::intercept_ca::CertificateAuthority;
    use std::sync::Arc;

    fn test_state() -> InterceptState {
        InterceptState {
            rules: InterceptRules::new(),
            ca: Arc::new(CertificateAuthority::generate().expect("generate CA")),
        }
    }

    #[test]
    fn ca_and_truststore_export_handlers() {
        let state = test_state();

        let ca_resp = handle_ca_pem(&state);
        assert_eq!(ca_resp.status(), StatusCode::OK);
        let ca_body = body_string(ca_resp);
        assert!(ca_body.starts_with("-----BEGIN CERTIFICATE-----"));

        let p12_resp = handle_truststore_p12(Some("password=hunter2"), &state);
        assert_eq!(p12_resp.status(), StatusCode::OK);
        assert_eq!(
            p12_resp
                .headers()
                .get("x-truststore-password")
                .unwrap()
                .to_str()
                .unwrap(),
            "hunter2"
        );
        // `p12` is not a direct dependency of this crate, so we assert non-empty bytes + the
        // password header rather than round-tripping the parser (rift-mock-core's own tests already
        // cover the PKCS#12 encoding itself).
        assert!(!body_bytes(p12_resp).is_empty());

        let jks_resp = handle_truststore_jks(None, &state);
        assert_eq!(jks_resp.status(), StatusCode::OK);
        assert_eq!(
            jks_resp
                .headers()
                .get("x-truststore-password")
                .unwrap()
                .to_str()
                .unwrap(),
            DEFAULT_TRUSTSTORE_PASSWORD
        );
        assert!(!body_bytes(jks_resp).is_empty());
    }

    #[test]
    fn rules_endpoints_list_and_clear() {
        let state = test_state();
        state
            .rules
            .add(InterceptRule {
                host: None,
                predicates: vec![],
                action: InterceptAction::Serve(ServeStub {
                    status_code: 200,
                    headers: Default::default(),
                    body: None,
                }),
            })
            .unwrap();

        let list_resp = handle_list_rules(&state);
        assert_eq!(list_resp.status(), StatusCode::OK);
        let listed: Vec<InterceptRule> = serde_json::from_str(&body_string(list_resp)).unwrap();
        assert_eq!(listed.len(), 1);

        let clear_resp = handle_clear_rules(&state);
        assert_eq!(clear_resp.status(), StatusCode::OK);
        let deleted: DeletedResponse = serde_json::from_str(&body_string(clear_resp)).unwrap();
        assert_eq!(deleted.deleted, 1);
        assert!(state.rules.is_empty());
    }

    #[test]
    fn password_from_query_defaults_and_decodes() {
        assert_eq!(password_from_query(None), DEFAULT_TRUSTSTORE_PASSWORD);
        assert_eq!(password_from_query(Some("password=abc")), "abc");
        assert_eq!(password_from_query(Some("other=1&password=a%20b")), "a b");
    }

    #[test]
    fn add_rules_from_bytes_stores_rule_and_rejects_bad_json() {
        let state = test_state();
        let json =
            br#"{"host":"cdn.example.com","action":{"serve":{"statusCode":418,"body":"brew"}}}"#;
        let resp = add_rules_from_bytes(json, &state);
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(state.rules.len(), 1);
        assert_eq!(
            state.rules.list()[0].host.as_deref(),
            Some("cdn.example.com")
        );

        let bad = add_rules_from_bytes(b"{not json", &state);
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        assert_eq!(state.rules.len(), 1, "a rejected body must not add a rule");
    }

    // Issue #554: once the store is at MAX_RULES, POST /intercept/rules is rejected with 429 for
    // both the single-rule and the batch shape, and stores nothing.
    #[test]
    fn add_rules_from_bytes_rejects_past_capacity_with_429() {
        use crate::intercept_rules::MAX_RULES;
        let state = test_state();
        let filler = InterceptRule {
            host: None,
            predicates: vec![],
            action: InterceptAction::Serve(ServeStub {
                status_code: 200,
                headers: Default::default(),
                body: None,
            }),
        };
        state
            .rules
            .extend(vec![filler; MAX_RULES])
            .expect("fill to the cap");

        let one = br#"{"action":{"serve":{"statusCode":200}}}"#;
        let resp = add_rules_from_bytes(one, &state);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            state.rules.len(),
            MAX_RULES,
            "a rejected add stores nothing"
        );

        let many = br#"[{"action":{"serve":{"statusCode":200}}}]"#;
        let resp = add_rules_from_bytes(many, &state);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            state.rules.len(),
            MAX_RULES,
            "a rejected batch stores nothing"
        );
    }

    // AC1/AC4: a rule created via the admin handler actually drives interception end-to-end.
    #[tokio::test]
    async fn rule_added_via_admin_handler_is_served_through_listener() {
        use crate::intercept::InterceptListener;
        use rift_mock_core::proxy::intercept_ca::SniCertResolver;

        let ca = CertificateAuthority::generate().expect("ca");
        let ca_pem = ca.ca_cert_pem().to_string();
        let ca = Arc::new(ca);
        let state = InterceptState {
            rules: InterceptRules::new(),
            ca: ca.clone(),
        };

        // Add the rule through the ADMIN handler path (not InterceptRules::add directly).
        let json = br#"{"host":"cdn.example.com","action":{"serve":{"statusCode":418,"body":"admin-brewed"}}}"#;
        assert_eq!(
            add_rules_from_bytes(json, &state).status(),
            StatusCode::CREATED
        );

        let resolver = Arc::new(SniCertResolver::new(ca));
        let listener = InterceptListener::bind(
            "127.0.0.1:0".parse().unwrap(),
            resolver,
            state.rules.clone(),
        )
        .await
        .expect("bind");
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(&proxy_url).unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
            .build()
            .unwrap();
        let resp = client
            .get("https://cdn.example.com/x")
            .send()
            .await
            .expect("intercepted");
        assert_eq!(resp.status(), 418);
        assert_eq!(resp.text().await.unwrap(), "admin-brewed");

        listener.shutdown().await;
    }

    // ── Runtime lifecycle handlers (issue #493) ────────────────────────────────────────────────

    // A #[tokio::test]-safe body reader (the sync `body_string` spins its own runtime, which panics
    // inside an async test).
    async fn read_body(resp: Response<Full<Bytes>>) -> String {
        use http_body_util::BodyExt;
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// The status body of a running listener — deserialized so the parts under test read cleanly.
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct StatusBody {
        intercept_port: u16,
        intercept_url: String,
    }

    #[tokio::test]
    async fn start_status_stop_handler_matrix() {
        let control = InterceptControl::default();

        // GET before start → 404.
        assert_eq!(handle_status(&control).status(), StatusCode::NOT_FOUND);

        // POST empty body → 201 with an OS-assigned port.
        let started = start_from_bytes(b"", &control).await;
        assert_eq!(started.status(), StatusCode::CREATED);
        let body: StatusBody = serde_json::from_str(&read_body(started).await).unwrap();
        assert!(body.intercept_port > 0);
        assert!(body.intercept_url.starts_with("http://"));

        // GET while running → 200, same port.
        let status = handle_status(&control);
        assert_eq!(status.status(), StatusCode::OK);
        let got: StatusBody = serde_json::from_str(&read_body(status).await).unwrap();
        assert_eq!(got.intercept_port, body.intercept_port);

        // POST while running → 409.
        assert_eq!(
            start_from_bytes(b"{}", &control).await.status(),
            StatusCode::CONFLICT
        );

        // DELETE → 204, then GET → 404 (idempotent second DELETE also 204).
        assert_eq!(handle_stop(&control).await.status(), StatusCode::NO_CONTENT);
        assert_eq!(handle_status(&control).status(), StatusCode::NOT_FOUND);
        assert_eq!(handle_stop(&control).await.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn start_rejects_unknown_field_and_bad_json() {
        let control = InterceptControl::default();
        assert_eq!(
            start_from_bytes(br#"{"caCertpath":"x"}"#, &control)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            start_from_bytes(b"{not json", &control).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert!(
            control.status().is_none(),
            "a rejected start must not leave a listener"
        );
    }

    #[tokio::test]
    async fn start_serialized_body_is_deserializable_status() {
        // AC8/parity: the 201 body round-trips through the same shape the FFI returns.
        let control = InterceptControl::default();
        let resp = start_from_bytes(b"", &control).await;
        let json = read_body(resp).await;
        assert!(json.contains("interceptPort"));
        assert!(json.contains("interceptUrl"));
        control.stop().await;
    }

    fn body_bytes(resp: Response<Full<Bytes>>) -> Vec<u8> {
        use http_body_util::BodyExt;
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(resp.into_body().collect())
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    fn body_string(resp: Response<Full<Bytes>>) -> String {
        String::from_utf8(body_bytes(resp)).unwrap()
    }
}
