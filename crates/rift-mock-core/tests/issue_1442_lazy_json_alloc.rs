//! Issue #1442: matching parses a request body as JSON only when a stub reads it.
//!
//! A counting global allocator measures the bytes matching allocates for a 4 MiB JSON body against
//! a path-only stub. Before the fix the body was parsed up front, and its `serde_json::Value` tree
//! (a `String` and map node per key, a `Value` per element) cost several times the body. Now nothing
//! reads the body, so matching allocates a small constant regardless of its size.
//!
//! Its own test binary, because the global allocator counts every thread in the process.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rift_mock_core::imposter::{Imposter, ImposterConfig};

struct CountingAllocator;
static BYTES: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn imposter(stubs: serde_json::Value) -> Imposter {
    let cfg: ImposterConfig = serde_json::from_value(
        serde_json::json!({ "port": 0, "protocol": "http", "stubs": stubs }),
    )
    .expect("valid imposter config");
    Imposter::new(cfg).expect("imposter")
}

/// A JSON array of small objects, about `bytes` long: the shape whose parse tree is largest
/// relative to its text.
fn json_body(bytes: usize) -> String {
    let item = r#"{"id":12345,"name":"abcdefgh"},"#;
    let mut body = String::with_capacity(bytes + 64);
    body.push('[');
    while body.len() < bytes {
        body.push_str(item);
    }
    body.pop();
    body.push(']');
    body
}

fn bytes_allocated_matching(imp: &Imposter, body: &str) -> usize {
    let headers: HashMap<String, String> = HashMap::new();
    BYTES.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let hit = imp
        .find_matching_stub_with_client("POST", "/a", &headers, None, Some(body), None, None)
        .expect("no backend error");
    COUNTING.store(false, Ordering::Relaxed);
    assert!(hit.is_some(), "the stub matches");
    BYTES.load(Ordering::Relaxed)
}

#[test]
fn a_body_no_stub_reads_is_not_parsed() {
    let body = json_body(4 * 1024 * 1024);
    let path_only = imposter(serde_json::json!([
        { "predicates": [{ "equals": { "path": "/a" } }],
          "responses": [{ "is": { "statusCode": 200 } }] }
    ]));
    let allocated = bytes_allocated_matching(&path_only, &body);
    assert!(
        allocated < body.len() / 4,
        "matching a path-only stub allocated {allocated} bytes for a {} byte body",
        body.len()
    );

    // The control: a stub that does read the body pays for the parse, so the bound above is
    // measuring the parse and not something else.
    let reads_body = imposter(serde_json::json!([
        { "predicates": [{ "exists": { "body": { "id": true } } }],
          "responses": [{ "is": { "statusCode": 200 } }] },
        { "predicates": [{ "equals": { "path": "/a" } }],
          "responses": [{ "is": { "statusCode": 200 } }] }
    ]));
    let parsed = bytes_allocated_matching(&reads_body, &body);
    assert!(
        parsed > body.len(),
        "a body predicate should parse the body ({parsed} bytes allocated)"
    );
}
