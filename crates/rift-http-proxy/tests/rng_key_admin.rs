//! Issue #1795: an `rngKey` the engine cannot use is refused at the admin API with `400 bad data`
//! naming the problem; a usable one is admitted.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};
use serde_json::{Value, json};

async fn post(admin: &str, body: &Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{admin}/imposters"))
        .json(body)
        .send()
        .await
        .expect("post");
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn imposter(engine: Value) -> Value {
    json!({ "protocol": "http", "_rift": { "scriptEngine": engine },
            "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }] })
}

#[tokio::test]
async fn a_bad_rng_key_is_400_and_a_good_one_is_admitted() {
    let cli = Cli::try_parse_from(["rift", "--host", "127.0.0.1", "--port", "0"]).expect("cli");
    let server = ServerBuilder::from_cli(cli).start().await.expect("server");
    let admin = format!("http://{}", server.admin_addr());
    for (engine, why) in [
        (
            json!({ "seed": 1, "rngKey": ["cookie:x"] }),
            "is not one of",
        ),
        (
            json!({ "rngKey": ["path"] }),
            "needs _rift.scriptEngine.seed",
        ),
        (json!({ "seed": 1, "rngKey": ["body:x"] }), "RFC 6901"),
    ] {
        let (status, body) = post(&admin, &imposter(engine.clone())).await;
        assert_eq!(status, 400, "{engine}: {body}");
        assert_eq!(body["errors"][0]["type"], "bad data", "{body}");
        assert!(body.to_string().contains(why), "{engine}: {body}");
    }
    let (status, body) = post(
        &admin,
        &imposter(json!({ "seed": 1, "rngKey": ["header:X-Rift-Rng-Key"] })),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    server.shutdown().await;
}
