//! AC1 for issue #703: the static-stub serve fast path performs no per-request `String` allocation
//! for body or headers.
//!
//! A counting global allocator (the same technique the decision-cache uses for #650) measures the
//! number of allocations `PreparedResponse::serve` makes. The proof of "no per-request String
//! allocation" is structural: serve's allocation count is a small **constant** that does NOT grow
//! with the number of headers or the body size — a per-header/per-body `String` allocation would
//! make it scale. The legacy execute+build assembly (clone the header `HashMap`, re-parse each
//! header from a `String`, `Bytes::from(String)`) is measured alongside to show the win.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bytes::Bytes;
use http_body_util::Full;
use hyper::Response;
use rift_mock_core::extensions::apply_date_templates;
use rift_mock_core::imposter::{IsResponse, PreparedResponse, ResponseMode};

struct CountingAllocator;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Allocations made while running `f` (over `iters` iterations), counting only inside the window.
fn count_allocs(iters: usize, mut f: impl FnMut()) -> usize {
    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    for _ in 0..iters {
        f();
    }
    COUNTING.store(false, Ordering::Relaxed);
    ALLOCS.load(Ordering::Relaxed)
}

fn is_response(n_headers: usize) -> (IsResponse, Arc<str>) {
    let mut headers = rift_mock_core::imposter::Headers::new();
    for i in 0..n_headers {
        headers.insert(format!("X-Header-{i}"), vec![format!("value-{i}")]);
    }
    let body = serde_json::json!({"id": 1, "name": "resource", "items": [1, 2, 3]});
    let rendered: Arc<str> = Arc::from(serde_json::to_string(&body).unwrap().as_str());
    let is = IsResponse::new(200, headers, Some(body), ResponseMode::Text);
    (is, rendered)
}

fn legacy_serve(is: &IsResponse, rendered: &Arc<str>) -> Response<Full<Bytes>> {
    let mut headers = is.headers.clone();
    if !headers.contains_key("content-type") && !headers.contains_key("Content-Type") {
        headers.insert(
            "Content-Type".to_string(),
            vec!["application/json".to_string()],
        );
    }
    let body = rendered.to_string();
    let mut builder = Response::builder().status(is.status_code);
    for (k, values) in &headers {
        for v in values {
            builder = builder.header(k, v);
        }
    }
    builder = builder.header("x-rift-imposter", "true");
    let body_bytes = Bytes::from(apply_date_templates(&body));
    builder.body(Full::new(body_bytes)).unwrap()
}

#[test]
fn serve_allocations_are_constant_and_below_legacy() {
    let (small_is, small_rendered) = is_response(2);
    let (big_is, big_rendered) = is_response(20);
    let small = PreparedResponse::try_build(&small_is, Some(&small_rendered), None, false)
        .expect("static is prepared");
    let big = PreparedResponse::try_build(&big_is, Some(&big_rendered), None, false)
        .expect("static is prepared");

    let iters = 200;
    // Warm up any one-time lazy allocations outside the measured window.
    black_box(small.serve());
    black_box(big.serve());

    let small_serve = count_allocs(iters, || {
        black_box(black_box(&small).serve());
    });
    let big_serve = count_allocs(iters, || {
        black_box(black_box(&big).serve());
    });
    let big_legacy = count_allocs(iters, || {
        black_box(legacy_serve(black_box(&big_is), black_box(&big_rendered)));
    });

    // No per-header/per-body String allocation: serve's allocation count does not grow with header
    // count — the 2-header and 20-header stubs allocate the same amount per serve.
    assert_eq!(
        small_serve, big_serve,
        "serve allocations must be constant regardless of header count \
         (2-header: {small_serve}, 20-header: {big_serve} over {iters} iters)"
    );
    // And it is a small constant per call (the HeaderMap backing clone; no per-item String allocs).
    assert!(
        big_serve / iters <= 4,
        "serve should allocate a small constant per call, got {} per call",
        big_serve / iters
    );
    // The win: legacy allocates far more (a String per header + body String + Bytes-from-String).
    assert!(
        big_serve * 3 < big_legacy,
        "fast path must allocate far less than legacy (serve={big_serve}, legacy={big_legacy})"
    );
}
