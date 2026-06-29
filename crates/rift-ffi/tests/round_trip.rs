//! Issue #204 gate: drive the full lifecycle across the C-ABI from a Rust integration test —
//! start → create imposter (JSON) → replace stubs (JSON) → serve+record → recorded (JSON) →
//! free → delete_all → stop — plus the null-pointer and error-sentinel paths. (Double-free /
//! use-after-free are undefined behaviour, so they belong under Miri/ASan, not a normal test.)

use rift_ffi::*;
use std::ffi::{c_char, CStr, CString};

fn cstr(s: &str) -> CString {
    CString::new(s).expect("no interior NUL in test input")
}

unsafe fn take_json(p: *mut c_char) -> String {
    assert!(!p.is_null(), "expected JSON, got null");
    let s = CStr::from_ptr(p).to_str().expect("utf8").to_owned();
    rift_free(p);
    s
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
