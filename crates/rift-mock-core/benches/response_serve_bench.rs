//! Criterion bench for the static-response serve path (issue #703).
//!
//! Compares the prepared fast path (`PreparedResponse::serve` — a status copy plus refcounted
//! `HeaderMap`/`Bytes` clones) against the legacy assembly the handler used for every static `is`
//! response (clone the `HashMap<String,Vec<String>>` headers, re-parse each header from strings via
//! `Response::builder().header(..)`, `apply_date_templates` scan, `Bytes::from(String)`). Both
//! produce the same `Response` (proven byte-identical by the `prepared_response_tests` differential
//! test); this measures how much per-request work the fast path removes.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use http_body_util::Full;
use hyper::Response;
use rift_mock_core::extensions::apply_date_templates;
use rift_mock_core::imposter::{IsResponse, PreparedResponse, ResponseMode};
use std::hint::black_box;

/// A representative static JSON stub: a handful of headers and a small object body — the dominant
/// mock workload.
fn fixture() -> (IsResponse, Arc<str>) {
    let mut headers: HashMap<String, Vec<String>> = HashMap::new();
    headers.insert("X-Trace-Id".to_string(), vec!["abc-123".to_string()]);
    headers.insert("Cache-Control".to_string(), vec!["no-store".to_string()]);
    let body = serde_json::json!({"id": 42, "name": "resource", "items": [1, 2, 3]});
    let rendered: Arc<str> = Arc::from(serde_json::to_string(&body).unwrap().as_str());
    let is = IsResponse {
        status_code: 200,
        headers,
        body: Some(body),
        mode: ResponseMode::Text,
    };
    (is, rendered)
}

/// The legacy handler assembly for a static `is` response, replicated here as the comparison
/// baseline (clone headers, re-parse them, date-scan the body, allocate `Bytes` from a `String`).
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

fn bench_response_serve(c: &mut Criterion) {
    let (is, rendered) = fixture();
    let prepared =
        PreparedResponse::try_build(&is, Some(&rendered), None, false).expect("static is prepared");

    let mut group = c.benchmark_group("response_serve");
    group.bench_function("prepared_fast_path", |b| {
        b.iter(|| black_box(black_box(&prepared).serve()))
    });
    group.bench_function("legacy_execute_build", |b| {
        b.iter(|| black_box(legacy_serve(black_box(&is), black_box(&rendered))))
    });
    group.finish();
}

criterion_group!(benches, bench_response_serve);
criterion_main!(benches);
