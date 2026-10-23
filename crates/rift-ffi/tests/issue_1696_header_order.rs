//! Issue #1696: the C-ABI views and doors kept a stub's (and the journal's) header order only until
//! they passed through a `serde_json::Value`, whose objects are sorted (`preserve_order` is off
//! workspace-wide). Every assertion reads the returned string: parsing it would sort it again.

use rift_ffi::*;
use std::ffi::{CStr, CString, c_char};
use std::io::{Read, Write};
use std::time::Duration;

const WRITTEN: &str = r#""headers":{"Zeta":"1","Alpha":"2","Mid":"3"}"#;

const ORDERED_IMPOSTER: &str = r#"{"protocol": "http", "recordRequests": true, "stubs": [
    {"responses": [{"is": {"headers": {"Zeta": "1", "Alpha": "2", "Mid": "3"}, "body": "ok"}}]}
]}"#;

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

/// `GET /` with `headers` in this order over a raw socket; the response's header names among
/// `zeta`/`alpha`/`mid`, in wire order.
fn served_header_names(port: u16, headers: &[(&str, &str)]) -> Vec<String> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut request = "GET / HTTP/1.1\r\nHost: localhost\r\n".to_owned();
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("write");
    let mut text = String::new();
    stream.read_to_string(&mut text).expect("read");
    let (head, _) = text.split_once("\r\n\r\n").expect("a response head");
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.to_ascii_lowercase())
        .filter(|name| ["zeta", "alpha", "mid"].contains(&name.as_str()))
        .collect()
}

#[test]
fn get_and_list_imposters_keep_stub_header_order() {
    unsafe {
        let h = rift_start();
        let port = rift_create_imposter(h, cstr(ORDERED_IMPOSTER).as_ptr());
        assert_ne!(port, 0);
        let replayable = cstr(r#"{"replayable": true}"#);

        let detail = take_json(rift_get_imposter(h, port, std::ptr::null()));
        assert!(detail.contains(WRITTEN), "detail: {detail}");
        let export = take_json(rift_get_imposter(h, port, replayable.as_ptr()));
        assert!(export.contains(WRITTEN), "replayable: {export}");
        let listed = take_json(rift_list_imposters(h, replayable.as_ptr()));
        assert!(listed.contains(WRITTEN), "replayable list: {listed}");
        rift_stop(h);
    }
}

#[test]
fn the_detail_prints_recorded_request_headers_in_the_order_sent() {
    unsafe {
        let h = rift_start();
        let port = rift_create_imposter(h, cstr(ORDERED_IMPOSTER).as_ptr());
        assert_ne!(port, 0);
        served_header_names(port, &[("X-Zulu", "1"), ("X-Alpha", "2"), ("X-Mike", "3")]);

        let detail = take_json(rift_get_imposter(h, port, std::ptr::null()));
        assert!(
            detail.contains(r#""X-Zulu":"1","X-Alpha":"2","X-Mike":"3""#),
            "detail: {detail}"
        );
        rift_stop(h);
    }
}

#[test]
fn the_list_summary_keeps_the_default_response_header_order() {
    unsafe {
        let h = rift_start();
        let port = rift_create_imposter(
            h,
            cstr(r#"{"protocol": "http", "defaultResponse": {"headers": {"Zeta": "1", "Alpha": "2", "Mid": "3"}}, "stubs": []}"#)
                .as_ptr(),
        );
        assert_ne!(port, 0);
        let listed = take_json(rift_list_imposters(h, std::ptr::null()));
        assert!(listed.contains(WRITTEN), "summary: {listed}");
        let detail = take_json(rift_get_imposter(h, port, std::ptr::null()));
        assert!(detail.contains(WRITTEN), "detail: {detail}");
        rift_stop(h);
    }
}

#[test]
fn apply_config_keeps_header_order() {
    unsafe {
        let h = rift_start();
        let doc = format!(r#"{{"imposters": [{ORDERED_IMPOSTER}]}}"#);
        let report = take_json(rift_apply_config(h, cstr(&doc).as_ptr()));
        let report: serde_json::Value = serde_json::from_str(&report).expect("report");
        let port = report["created"][0].as_u64().expect("a created port");
        let port = u16::try_from(port).expect("a port");

        let export = take_json(rift_get_imposter(
            h,
            port,
            cstr(r#"{"replayable": true}"#).as_ptr(),
        ));
        assert!(export.contains(WRITTEN), "replayable: {export}");
        assert_eq!(served_header_names(port, &[]), ["zeta", "alpha", "mid"]);

        // The bare-array shape decodes in order too.
        let doc = format!("[{ORDERED_IMPOSTER}]");
        take_json(rift_apply_config(h, cstr(&doc).as_ptr()));
        let listed = take_json(rift_list_imposters(
            h,
            cstr(r#"{"replayable": true}"#).as_ptr(),
        ));
        assert!(listed.contains(WRITTEN), "replayable list: {listed}");
        rift_stop(h);
    }
}

#[test]
fn serve_admin_inline_config_keeps_header_order() {
    unsafe {
        let h = rift_start();
        let opts = format!(r#"{{"config": {{"imposters": [{ORDERED_IMPOSTER}]}}}}"#);
        take_json(rift_serve_admin(h, cstr(&opts).as_ptr()));
        let listed = take_json(rift_list_imposters(
            h,
            cstr(r#"{"replayable": true}"#).as_ptr(),
        ));
        assert!(listed.contains(WRITTEN), "replayable list: {listed}");
        rift_stop(h);
    }
}

#[test]
fn space_list_stubs_keeps_header_order() {
    unsafe {
        let h = rift_start();
        let port = rift_create_imposter(h, cstr(r#"{"protocol": "http", "stubs": []}"#).as_ptr());
        assert_ne!(port, 0);
        let space = cstr("s1");
        let stub = cstr(
            r#"{"responses": [{"is": {"headers": {"Zeta": "1", "Alpha": "2", "Mid": "3"}}}]}"#,
        );
        assert_eq!(
            rift_space_add_stub(h, port, space.as_ptr(), stub.as_ptr()),
            0
        );
        let listed = take_json(rift_space_list_stubs(h, port, space.as_ptr()));
        assert!(listed.contains(WRITTEN), "space stubs: {listed}");
        rift_stop(h);
    }
}
