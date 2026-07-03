//! Issue #204 gate: drive the full lifecycle across the C-ABI from a Rust integration test —
//! start → create imposter (JSON) → replace stubs (JSON) → serve+record → recorded (JSON) →
//! free → delete_all → stop — plus the null-pointer and error-sentinel paths. (Double-free /
//! use-after-free are undefined behaviour, so they belong under Miri/ASan, not a normal test.)

use rift_ffi::*;
use std::ffi::{CStr, CString, c_char};

fn cstr(s: &str) -> CString {
    CString::new(s).expect("no interior NUL in test input")
}

unsafe fn take_json(p: *mut c_char) -> String {
    unsafe {
        assert!(!p.is_null(), "expected JSON, got null");
        let s = CStr::from_ptr(p).to_str().expect("utf8").to_owned();
        rift_free(p);
        s
    }
}

#[test]
fn ffi_round_trip() {
    unsafe {
        let h = rift_start();
        assert!(!h.is_null(), "rift_start returned null");

        // create imposter from JSON config; returns its port
        let config =
            cstr(r#"{ "port": 19990, "protocol": "http", "recordRequests": true, "stubs": [] }"#);
        let port = rift_create_imposter(h, config.as_ptr());
        assert_eq!(port, 19990);

        // replace stubs from a JSON array
        let stubs = cstr(
            r#"[{ "predicates": [{ "equals": { "path": "/hello" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "world" } }] }]"#,
        );
        assert_eq!(rift_replace_stubs(h, port, stubs.as_ptr()), 0);

        // the embedded mock serves on its port — drive it like any HTTP service
        let rt = tokio::runtime::Runtime::new().unwrap();
        let body = rt.block_on(async {
            let r = reqwest::get("http://127.0.0.1:19990/hello")
                .await
                .expect("get");
            assert_eq!(r.status(), 200);
            r.text().await.expect("text")
        });
        assert_eq!(body, "world");

        // recorded requests come back as JSON; caller frees the buffer
        let recorded = take_json(rift_recorded(h, port));
        let parsed: serde_json::Value = serde_json::from_str(&recorded).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["path"], "/hello");

        // teardown removes all imposters — verified by the effect, not just the return code:
        // the port is gone, so recorded now returns null.
        assert_eq!(rift_delete_all(h), 0);
        assert!(
            rift_recorded(h, port).is_null(),
            "delete_all must remove the imposter (recorded → null)"
        );

        // stop drops the handle + runtime
        rift_stop(h);
    }
}

#[test]
fn ffi_null_and_error_paths() {
    unsafe {
        // null handle / null json never abort — they return error sentinels
        assert_eq!(
            rift_create_imposter(std::ptr::null_mut(), std::ptr::null()),
            0
        );
        assert_eq!(
            rift_replace_stubs(std::ptr::null_mut(), 1, std::ptr::null()),
            -1
        );
        assert_eq!(rift_delete_all(std::ptr::null_mut()), -1);
        assert!(rift_recorded(std::ptr::null_mut(), 1).is_null());
        // freeing null and stopping null are no-ops
        rift_free(std::ptr::null_mut());
        rift_stop(std::ptr::null_mut());

        // a live handle with malformed JSON / unknown port also yields sentinels
        let h = rift_start();
        let bad = cstr("not json");
        assert_eq!(rift_create_imposter(h, bad.as_ptr()), 0);
        assert_eq!(rift_replace_stubs(h, 12345, bad.as_ptr()), -1);
        // valid stubs but an unknown port exercises the manager's own NotFound error path
        let valid = cstr(r#"[{ "predicates": [], "responses": [] }]"#);
        assert_eq!(
            rift_replace_stubs(h, 12345, valid.as_ptr()),
            -1,
            "unknown port → -1"
        );
        assert!(rift_recorded(h, 12345).is_null(), "unknown port → null");
        rift_stop(h);
    }
}

// ===========================================================================
// Issue #343: C-ABI v2 — in-process admin plane, apply_config, delete_imposter,
// build_info, last_error.
// ===========================================================================

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("tokio runtime")
}

/// Call `rift_serve_admin`, assert it succeeded, and parse the JSON (frees the buffer).
unsafe fn serve_admin(h: *mut RiftHandle, opts: &str) -> serde_json::Value {
    unsafe {
        let c = cstr(opts);
        let json = take_json(rift_serve_admin(h, c.as_ptr()));
        serde_json::from_str(&json).expect("serve_admin returns JSON")
    }
}

// AC1: rift_serve_admin spawns the real admin API over the handle's manager — an FFI-created
// imposter is visible via GET /imposters, the admin control plane creates imposters that serve
// data-plane traffic, and scenario endpoints respond.
#[test]
fn ffi_serve_admin_exposes_manager_and_control_plane() {
    unsafe {
        let h = rift_start();
        let cfg = cstr(
            r#"{"port":19991,"protocol":"http","stubs":[{"predicates":[{"equals":{"path":"/ping"}}],"responses":[{"is":{"statusCode":200,"body":"pong"}}]}]}"#,
        );
        assert_eq!(rift_create_imposter(h, cfg.as_ptr()), 19991);

        let info = serve_admin(h, "{}");
        assert!(info["adminPort"].as_u64().expect("adminPort") > 0);
        let admin_url = info["adminUrl"].as_str().expect("adminUrl").to_string();

        rt().block_on(async {
            // The FFI-created imposter is visible over the real admin API.
            let imps: serde_json::Value = reqwest::get(format!("{admin_url}/imposters"))
                .await
                .expect("admin /imposters reachable")
                .json()
                .await
                .expect("json");
            let ports: Vec<u64> = imps["imposters"]
                .as_array()
                .expect("imposters array")
                .iter()
                .filter_map(|i| i["port"].as_u64())
                .collect();
            assert!(
                ports.contains(&19991),
                "FFI imposter visible via admin /imposters"
            );

            // Control plane: create an imposter over admin; it serves data-plane traffic.
            let create = reqwest::Client::new()
                .post(format!("{admin_url}/imposters"))
                .json(&serde_json::json!({
                    "port":19992,"protocol":"http",
                    "stubs":[{"responses":[{"is":{"statusCode":201,"body":"made"}}]}]
                }))
                .send()
                .await
                .expect("admin create imposter");
            assert!(
                create.status().is_success(),
                "admin created imposter over the embedded plane"
            );

            let served = reqwest::get("http://127.0.0.1:19992/anything")
                .await
                .expect("admin-created imposter reachable");
            assert_eq!(served.status(), 201);

            // Scenario endpoint responds (spot-check the wider control-plane surface).
            let scen = reqwest::get(format!("{admin_url}/imposters/19991/scenarios"))
                .await
                .expect("scenarios endpoint reachable");
            assert_eq!(
                scen.status(),
                200,
                "scenario endpoint responds over the embedded admin"
            );
        });

        rift_stop(h);
    }
}

// AC2: rift_apply_config returns the reload report field names; failed is [{port,error}]; an
// up-front validation failure returns NULL, sets last_error, and mutates nothing.
#[test]
fn ffi_apply_config_report_fields_and_validation() {
    unsafe {
        let h = rift_start();

        let cfg = cstr(r#"{"imposters":[{"port":19993,"protocol":"http","stubs":[]}]}"#);
        let report = take_json(rift_apply_config(h, cfg.as_ptr()));
        let v: serde_json::Value = serde_json::from_str(&report).expect("report json");
        for k in ["created", "replaced", "stubPatched", "deleted", "failed"] {
            assert!(
                v.get(k).is_some(),
                "apply report has field `{k}` (reload parity)"
            );
        }
        assert!(
            v["created"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_u64() == Some(19993)),
            "19993 reported as created"
        );

        // Duplicate explicit ports parse but fail up-front validation → NULL + last_error, and
        // nothing is mutated (port 19994 is left free to bind afterward).
        let dup = cstr(
            r#"{"imposters":[{"port":19994,"protocol":"http","stubs":[]},{"port":19994,"protocol":"http","stubs":[]}]}"#,
        );
        assert!(
            rift_apply_config(h, dup.as_ptr()).is_null(),
            "invalid config set → NULL"
        );
        let err = rift_last_error();
        assert!(!err.is_null(), "validation failure records last_error");
        rift_free(err);
        assert_eq!(
            rift_create_imposter(
                h,
                cstr(r#"{"port":19994,"protocol":"http","stubs":[]}"#).as_ptr()
            ),
            19994,
            "nothing was mutated — 19994 is still free to bind"
        );

        rift_stop(h);
    }
}

// AC3: rift_delete_imposter frees the port (immediate re-create on the same port succeeds);
// deleting an unknown port returns -1.
#[test]
fn ffi_delete_imposter_frees_port() {
    unsafe {
        let h = rift_start();
        let cfg = cstr(r#"{"port":19995,"protocol":"http","stubs":[]}"#);
        assert_eq!(rift_create_imposter(h, cfg.as_ptr()), 19995);

        assert_eq!(rift_delete_imposter(h, 19995), 0, "delete ok");
        assert_eq!(
            rift_create_imposter(h, cfg.as_ptr()),
            19995,
            "port freed — re-create on the same port succeeds"
        );

        assert_eq!(rift_delete_imposter(h, 6553), -1, "unknown port → -1");
        rift_stop(h);
    }
}

// AC4: rift_build_info parses; version == CARGO_PKG_VERSION; features list matches the enabled
// feature set; commit/builtAt are present (string or null).
#[test]
fn ffi_build_info_reports_version_and_features() {
    unsafe {
        let p = rift_build_info();
        assert!(!p.is_null(), "build_info returns a static string");
        // Static string — NOT freed.
        let s = CStr::from_ptr(p).to_str().expect("utf8").to_owned();
        let v: serde_json::Value = serde_json::from_str(&s).expect("build_info json");

        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert!(
            v.get("commit").is_some(),
            "commit field present (string or null)"
        );
        assert!(
            v.get("builtAt").is_some(),
            "builtAt field present (string or null)"
        );

        let features: Vec<&str> = v["features"]
            .as_array()
            .expect("features array")
            .iter()
            .map(|f| f.as_str().expect("feature str"))
            .collect();
        if cfg!(feature = "javascript") {
            assert!(
                features.contains(&"javascript"),
                "javascript feature reported"
            );
        }
        if cfg!(feature = "lua") {
            assert!(features.contains(&"lua"), "lua feature reported");
        }
        if cfg!(feature = "redis-backend") {
            assert!(
                features.contains(&"redis-backend"),
                "redis-backend feature reported"
            );
        }
    }
}

// AC5: rift_last_error is set on failure, cleared by the next successful call, cleared by reading
// it, and confined to the calling thread.
#[test]
fn ffi_last_error_set_cleared_and_reading_clears() {
    unsafe {
        let h = rift_start();

        // Failure sets it.
        assert!(rift_apply_config(h, cstr("{ not json").as_ptr()).is_null());
        let first = rift_last_error();
        assert!(!first.is_null(), "failure sets last_error");
        rift_free(first);
        // Reading cleared it.
        assert!(rift_last_error().is_null(), "reading last_error clears it");

        // Failure sets it again; a subsequent successful call clears it.
        assert!(rift_apply_config(h, cstr("{ not json").as_ptr()).is_null());
        assert_eq!(
            rift_delete_all(h),
            0,
            "a successful call clears last_error on entry"
        );
        assert!(
            rift_last_error().is_null(),
            "next successful call cleared last_error"
        );

        // Thread-confined: set on this thread, another thread sees none.
        assert!(rift_apply_config(h, cstr("{ not json").as_ptr()).is_null());
        let other = std::thread::spawn(|| rift_last_error().is_null())
            .join()
            .expect("thread");
        assert!(
            other,
            "last_error is thread-local — not visible on another thread"
        );
        let mine = rift_last_error();
        assert!(!mine.is_null(), "this thread's last_error is still set");
        rift_free(mine);

        rift_stop(h);
    }
}

// AC6: an HTTPS imposter created via FFI works without the host pre-installing a rustls provider
// — rift_start installs the ring default provider.
#[test]
fn ffi_https_imposter_without_host_provider() {
    unsafe {
        // NB: no rustls provider is installed by this test; rift_start must do it.
        let h = rift_start();
        let cfg = cstr(
            r#"{"port":19996,"protocol":"https","stubs":[{"responses":[{"is":{"statusCode":200,"body":"secure"}}]}]}"#,
        );
        assert_eq!(
            rift_create_imposter(h, cfg.as_ptr()),
            19996,
            "HTTPS imposter binds (self-signed) — provider is installed"
        );

        let body = rt().block_on(async {
            let client = reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .expect("tls client");
            let r = client
                .get("https://127.0.0.1:19996/anything")
                .send()
                .await
                .expect("https reachable");
            assert_eq!(r.status(), 200);
            r.text().await.expect("body")
        });
        assert_eq!(body, "secure");

        rift_stop(h);
    }
}

// AC7: one admin plane per handle — a second rift_serve_admin fails (NULL) with a last_error.
#[test]
fn ffi_one_admin_plane_per_handle() {
    unsafe {
        let h = rift_start();
        let _ = serve_admin(h, "{}");

        let second = rift_serve_admin(h, cstr("{}").as_ptr());
        assert!(
            second.is_null(),
            "second serve_admin on the same handle fails"
        );
        let err = rift_last_error();
        assert!(!err.is_null(), "already-serving failure records last_error");
        rift_free(err);

        rift_stop(h);
    }
}

// AC1 (metricsPort option): serve_admin with metricsPort:0 binds a metrics server and reports its
// assigned port; that port serves /metrics. Without the option, metricsPort is null.
#[test]
fn ffi_serve_admin_with_metrics_port() {
    unsafe {
        let h = rift_start();
        let info = serve_admin(h, r#"{"metricsPort":0}"#);
        let mp = info["metricsPort"].as_u64().expect("metricsPort reported") as u16;
        assert_ne!(mp, 0, "metrics bound to an assigned port");

        let ok = rt().block_on(async {
            reqwest::get(format!("http://127.0.0.1:{mp}/metrics"))
                .await
                .expect("metrics reachable")
                .status()
                == 200
        });
        assert!(ok, "metrics server serves /metrics");
        rift_stop(h);

        // No metricsPort → null in the response.
        let h2 = rift_start();
        let info = serve_admin(h2, "{}");
        assert!(info["metricsPort"].is_null(), "no metricsPort → null");
        rift_stop(h2);
    }
}

/// True once `url`'s host:port refuses TCP connections (listener gone), polled up to ~2s.
async fn admin_refused(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..40 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Err(_)) => return true,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    false
}

// AC8b: rift_stop actually shuts the admin/metrics listeners down (both ports refuse afterward).
#[test]
fn ffi_stop_shuts_down_admin_and_metrics() {
    unsafe {
        let h = rift_start();
        let info = serve_admin(h, r#"{"metricsPort":0}"#);
        let admin_port = info["adminPort"].as_u64().expect("adminPort") as u16;
        let metrics_port = info["metricsPort"].as_u64().expect("metricsPort") as u16;
        rt().block_on(async {
            // Both are up while serving.
            assert_eq!(
                reqwest::get(format!("http://127.0.0.1:{admin_port}/health"))
                    .await
                    .expect("admin up")
                    .status(),
                200
            );
        });

        rift_stop(h);

        rt().block_on(async {
            assert!(
                admin_refused(admin_port).await,
                "admin port closed by rift_stop"
            );
            assert!(
                admin_refused(metrics_port).await,
                "metrics port closed by rift_stop"
            );
        });
    }
}

// AC1 (apiKey option) + the security contract: a string apiKey gates the admin control plane; a
// wrong-typed apiKey is a hard error (NULL + last_error), never a silent unauthenticated plane.
#[test]
fn ffi_serve_admin_apikey_gates_and_rejects_wrong_type() {
    unsafe {
        // Wrong-type apiKey must fail loudly, not silently disable auth.
        let h = rift_start();
        assert!(
            rift_serve_admin(h, cstr(r#"{"apiKey":12345}"#).as_ptr()).is_null(),
            "non-string apiKey is rejected, not silently unauthenticated"
        );
        let err = rift_last_error();
        assert!(!err.is_null(), "wrong-type apiKey records last_error");
        rift_free(err);

        // A string apiKey gates the admin API.
        let info = serve_admin(h, r#"{"apiKey":"secret"}"#);
        let admin_url = info["adminUrl"].as_str().expect("adminUrl").to_string();
        rt().block_on(async {
            let unauthed = reqwest::get(format!("{admin_url}/imposters"))
                .await
                .expect("reachable");
            assert_eq!(unauthed.status(), 401, "admin requires the apiKey");

            let authed = reqwest::Client::new()
                .get(format!("{admin_url}/imposters"))
                .header("authorization", "secret")
                .send()
                .await
                .expect("reachable");
            assert_eq!(authed.status(), 200, "correct apiKey is accepted");
        });
        rift_stop(h);
    }
}

// AC1 (configFile option): serve_admin loads imposters from a config file and wires it as the
// reload source; the loaded imposter is visible over the admin API.
#[test]
fn ffi_serve_admin_loads_config_file() {
    unsafe {
        let path = std::env::temp_dir().join(format!("rift_ffi_cfg_{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"imposters":[{"port":19998,"protocol":"http","stubs":[]}]}"#,
        )
        .expect("write config file");

        let h = rift_start();
        let opts = format!(
            r#"{{"configFile":{}}}"#,
            serde_json::json!(path.to_str().unwrap())
        );
        let info = serve_admin(h, &opts);
        let admin_url = info["adminUrl"].as_str().expect("adminUrl").to_string();

        rt().block_on(async {
            let imps: serde_json::Value = reqwest::get(format!("{admin_url}/imposters"))
                .await
                .expect("reachable")
                .json()
                .await
                .expect("json");
            let ports: Vec<u64> = imps["imposters"]
                .as_array()
                .expect("array")
                .iter()
                .filter_map(|i| i["port"].as_u64())
                .collect();
            assert!(
                ports.contains(&19998),
                "configFile imposter loaded and visible"
            );
        });
        rift_stop(h);
        let _ = std::fs::remove_file(&path);
    }
}

// AC2 (per-port failure shape): a config whose port is already occupied applies with that port in
// `failed` as {port, error} — nothing up-front-invalid, so the report (not NULL) comes back.
#[test]
fn ffi_apply_config_reports_per_port_failure() {
    unsafe {
        // Occupy 0.0.0.0:<port> — the wildcard address imposters bind — so the manager's bind for
        // that port deterministically collides during apply.
        let occupied = std::net::TcpListener::bind("0.0.0.0:0").expect("occupy port");
        let busy = occupied.local_addr().expect("addr").port();

        let h = rift_start();
        let cfg = cstr(&format!(
            r#"{{"imposters":[{{"port":{busy},"protocol":"http","stubs":[]}}]}}"#
        ));
        let report = take_json(rift_apply_config(h, cfg.as_ptr()));
        let v: serde_json::Value = serde_json::from_str(&report).expect("report json");

        let failed = v["failed"].as_array().expect("failed array");
        assert_eq!(failed.len(), 1, "the occupied port failed to bind");
        assert_eq!(
            failed[0]["port"].as_u64(),
            Some(u64::from(busy)),
            "failed entry carries the port"
        );
        assert!(
            failed[0]["error"].as_str().is_some_and(|s| !s.is_empty()),
            "failed entry carries a non-empty error string"
        );
        rift_stop(h);
    }
}
