//! The `rift:mock/state` conformance guest (issue #1797).
//!
//! A host passes `script.json` as the knobs (`init`); every `respond` then runs the script's calls in
//! order against the host's `state` and answers `200` when each result matched, else `422`. The body
//! is a JSON report either way: `{"contract", "steps", "failures": [{"step", "note", "call",
//! "expected", "actual"}]}`.
//!
//! The script names the request's own flow by omitting `flow`, so run it on a store with no state
//! for that flow, nor for any flow the script names.
//!
//! Build: `cargo build --release --target wasm32-wasip2` in this directory.

wit_bindgen::generate!({
    path: "../rift-mock.wit",
    world: "mock",
});

use exports::rift::mock::handler::Guest;
use rift::mock::state::{self, CasOutcome};
use rift::mock::types::{Decision, Header, Request, Response};
use serde_json::{Value, json};
use std::sync::OnceLock;

static SCRIPT: OnceLock<Value> = OnceLock::new();

struct Conformance;

/// A JSON text a host returned, as the script spells a present value: `{"json": <value>}`. Text
/// that is not JSON is a failure in itself, kept visible as `{"notJson": <text>}`.
fn present(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => json!({ "json": v }),
        Err(_) => json!({ "notJson": text }),
    }
}

fn ok(v: Value) -> Value {
    json!({ "ok": v })
}

fn err(msg: String) -> Value {
    json!({ "err": msg })
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("script: missing string argument {name:?}"))
}

fn int_arg(args: &Value, name: &str) -> Result<i64, String> {
    args.get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("script: missing integer argument {name:?}"))
}

/// One call, its outcome in the script's terms.
fn run(call: &str, args: &Value) -> Result<Value, String> {
    let flow = args.get("flow").and_then(Value::as_str);
    Ok(match call {
        "get" => match state::get(str_arg(args, "key")?, flow) {
            Ok(None) => ok(Value::Null),
            Ok(Some(text)) => ok(present(&text)),
            Err(e) => err(e),
        },
        "set" => match state::set(str_arg(args, "key")?, str_arg(args, "value")?, flow) {
            Ok(()) => ok(Value::Null),
            Err(e) => err(e),
        },
        "incr-by" => match state::incr_by(str_arg(args, "key")?, int_arg(args, "by")?, flow) {
            Ok(n) => ok(json!(n)),
            Err(e) => err(e),
        },
        "exists" => match state::exists(str_arg(args, "key")?, flow) {
            Ok(b) => ok(json!(b)),
            Err(e) => err(e),
        },
        "delete" => match state::delete(str_arg(args, "key")?, flow) {
            Ok(()) => ok(Value::Null),
            Err(e) => err(e),
        },
        "cas" => {
            let expected = args.get("expected").and_then(Value::as_str);
            match state::cas(str_arg(args, "key")?, expected, str_arg(args, "new")?, flow) {
                Ok(CasOutcome::Applied) => ok(json!("applied")),
                Ok(CasOutcome::Conflict(None)) => ok(json!({ "conflict": null })),
                Ok(CasOutcome::Conflict(Some(text))) => ok(json!({ "conflict": present(&text) })),
                Err(e) => err(e),
            }
        }
        "ttl" => match state::ttl(int_arg(args, "seconds")?, flow) {
            Ok(()) => ok(Value::Null),
            Err(e) => err(e),
        },
        "key-ttl" => match state::key_ttl(str_arg(args, "key")?, int_arg(args, "seconds")?, flow) {
            Ok(b) => ok(json!(b)),
            Err(e) => err(e),
        },
        "clear" => match state::clear(flow) {
            Ok(()) => ok(Value::Null),
            Err(e) => err(e),
        },
        other => return Err(format!("script: unknown call {other:?}")),
    })
}

/// Whether `actual` meets `expected`: equal, except that an expected `{"err": "<prefix>"}` matches
/// any error message starting with that prefix (the part after it is the backend's own wording).
fn meets(expected: &Value, actual: &Value) -> bool {
    match (
        expected.get("err").and_then(Value::as_str),
        actual.get("err"),
    ) {
        (Some(prefix), Some(Value::String(msg))) => msg.starts_with(prefix),
        (Some(_), _) => false,
        (None, _) => expected == actual,
    }
}

fn reply(status: u16, report: &Value) -> Decision {
    Decision::Respond(Response {
        status,
        headers: vec![Header {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
        body: report.to_string().into_bytes(),
    })
}

impl Guest for Conformance {
    fn init(knobs: String) -> Result<(), String> {
        let script: Value =
            serde_json::from_str(&knobs).map_err(|e| format!("knobs are not JSON: {e}"))?;
        if !script.get("steps").is_some_and(Value::is_array) {
            return Err("knobs are not a conformance script: no \"steps\" array".into());
        }
        SCRIPT
            .set(script)
            .map_err(|_| "init ran twice on one instance".to_string())
    }

    fn respond(_req: Request) -> Result<Decision, String> {
        let script = SCRIPT.get().ok_or("respond before init")?;
        let steps = script["steps"].as_array().map_or(&[][..], Vec::as_slice);
        let mut failures = Vec::new();
        for (i, step) in steps.iter().enumerate() {
            let call = step.get("call").and_then(Value::as_str).unwrap_or("");
            let args = step.get("args").cloned().unwrap_or_else(|| json!({}));
            let expected = step.get("expect").cloned().unwrap_or(Value::Null);
            let actual = run(call, &args)?;
            if !meets(&expected, &actual) {
                failures.push(json!({
                    "step": i,
                    "note": step.get("note"),
                    "call": call,
                    "args": args,
                    "expected": expected,
                    "actual": actual,
                }));
            }
        }
        let status = if failures.is_empty() { 200 } else { 422 };
        let report = json!({
            "contract": script.get("contract"),
            "steps": steps.len(),
            "failures": failures,
        });
        Ok(reply(status, &report))
    }
}

export!(Conformance);
