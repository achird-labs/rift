//! A `rift:mock` component (issue #1751): each request increments `count` in the imposter's flow
//! store and answers it as JSON. With `knobs.limit` set, a count above the limit answers 409. Each
//! request prints one line to stdout, which Rift logs like `ctx.logger` output.
//!
//! Build with `cargo build --release --target wasm32-wasip2`, then point a `_rift.script` at
//! `target/wasm32-wasip2/release/counter_twin.wasm` (see `imposter.json`).

wit_bindgen::generate!({
    path: "../../../crates/rift-mock-core/wit",
    world: "mock",
});

use exports::rift::mock::handler::Guest;
use rift::mock::state;
use rift::mock::types::{Decision, Header, Request, Response};
use std::sync::OnceLock;

/// `knobs.limit`, set by `init`. Rift makes a fresh instance for every request, so `init` runs once
/// per instance.
static LIMIT: OnceLock<Option<i64>> = OnceLock::new();

struct CounterTwin;

fn json(status: u16, body: String) -> Decision {
    Decision::Respond(Response {
        status,
        headers: vec![Header {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
        body: body.into_bytes(),
    })
}

impl Guest for CounterTwin {
    fn init(knobs: String) -> Result<(), String> {
        let knobs: serde_json::Value =
            serde_json::from_str(&knobs).map_err(|e| format!("knobs are not JSON: {e}"))?;
        let limit = knobs.get("limit").and_then(serde_json::Value::as_i64);
        LIMIT
            .set(limit)
            .map_err(|_| "init ran twice on one instance".to_string())
    }

    fn respond(_req: Request) -> Result<Decision, String> {
        let count = state::incr_by("count", 1, None)?;
        println!("count is now {count}");
        if let Some(limit) = LIMIT.get().copied().flatten()
            && count > limit
        {
            return Ok(json(
                409,
                format!(r#"{{"error":"above limit","count":{count},"limit":{limit}}}"#),
            ));
        }
        Ok(json(200, format!(r#"{{"count":{count}}}"#)))
    }
}

export!(CounterTwin);
