//! Issue #1754: `GET`/`PUT`/`DELETE /imposters/:port/clock` against a live admin listener.

use clap::Parser;
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use serde_json::{Value, json};

async fn start() -> (RunningServer, String) {
    let cli = Cli::try_parse_from(["rift", "--host", "127.0.0.1", "--port", "0"]).expect("cli");
    let server = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("server starts");
    let admin = format!("http://{}", server.admin_addr());
    (server, admin)
}

async fn send(method: reqwest::Method, url: &str, body: Option<&Value>) -> (u16, Value) {
    let mut req = reqwest::Client::new().request(method, url);
    if let Some(body) = body {
        req = req.json(body);
    }
    let resp = req.send().await.expect("admin request");
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

async fn create_imposter(admin: &str) -> u16 {
    let (status, body) = send(
        reqwest::Method::POST,
        &format!("{admin}/imposters"),
        Some(&json!({ "protocol": "http", "port": 0,
            "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }] })),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    u16::try_from(body["port"].as_u64().expect("port")).expect("port fits")
}

#[tokio::test]
async fn freeze_advance_and_reset_round_trip() {
    let (server, admin) = start().await;
    let port = create_imposter(&admin).await;
    let url = format!("{admin}/imposters/{port}/clock");

    let (status, body) = send(reqwest::Method::GET, &url, None).await;
    assert_eq!(status, 200);
    assert_eq!(body["mode"], "wall");
    assert!(body["nowMs"].as_i64().is_some());

    let (status, body) = send(
        reqwest::Method::PUT,
        &url,
        Some(&json!({ "nowMs": 1_700_000_000_000_i64 })),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({ "mode": "frozen", "nowMs": 1_700_000_000_000_i64 })
    );

    let (_, body) = send(reqwest::Method::GET, &url, None).await;
    assert_eq!(
        body,
        json!({ "mode": "frozen", "nowMs": 1_700_000_000_000_i64 })
    );

    let (status, body) = send(
        reqwest::Method::PUT,
        &url,
        Some(&json!({ "advanceMs": 86_400_000 })),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({ "mode": "frozen", "nowMs": 1_700_086_400_000_i64 })
    );

    let (status, body) = send(reqwest::Method::DELETE, &url, None).await;
    assert_eq!(status, 200);
    assert_eq!(body["mode"], "wall");
    let (_, body) = send(reqwest::Method::GET, &url, None).await;
    assert_eq!(body["mode"], "wall");

    server.shutdown().await;
}

#[tokio::test]
async fn malformed_bodies_are_bad_data() {
    let (server, admin) = start().await;
    let port = create_imposter(&admin).await;
    let url = format!("{admin}/imposters/{port}/clock");

    let bodies = [
        json!({ "nowMs": 1, "advanceMs": 1 }),
        json!({}),
        json!({ "advanceMs": -1 }),
        json!({ "nowMs": "x" }),
        json!({ "nowMs": 1.5 }),
        json!({ "nowMs": 1, "other": 2 }),
        json!([1]),
        json!({ "advanceMs": i64::MAX }),
        // Past year 9999, or before year 1 (issue #1754): every platform's clock holds that range.
        json!({ "nowMs": 253_402_300_800_000_i64 }),
        json!({ "nowMs": -62_135_596_800_001_i64 }),
        json!({ "nowMs": i64::MAX }),
        json!({ "advanceMs": 2_000 }),
    ];
    // Freeze near the top so the last body overflows rather than succeeding from the wall clock.
    let (status, _) = send(
        reqwest::Method::PUT,
        &url,
        Some(&json!({ "nowMs": 253_402_300_799_000_i64 })),
    )
    .await;
    assert_eq!(status, 200);
    for body in bodies {
        let (status, resp) = send(reqwest::Method::PUT, &url, Some(&body)).await;
        assert_eq!(status, 400, "{body}: {resp}");
        assert_eq!(resp["errors"][0]["type"], "bad data", "{body}: {resp}");
    }

    let raw = reqwest::Client::new()
        .put(&url)
        .body("not json")
        .send()
        .await
        .expect("request");
    assert_eq!(raw.status().as_u16(), 400);
    let resp: Value = raw.json().await.expect("json");
    assert_eq!(resp["errors"][0]["type"], "bad data");

    server.shutdown().await;
}

#[tokio::test]
async fn an_absent_imposter_is_404_on_every_verb() {
    let (server, admin) = start().await;
    let url = format!("{admin}/imposters/59999/clock");
    for (method, body) in [
        (reqwest::Method::GET, None),
        (reqwest::Method::PUT, Some(json!({ "nowMs": 1 }))),
        (reqwest::Method::DELETE, None),
    ] {
        let (status, _) = send(method.clone(), &url, body.as_ref()).await;
        assert_eq!(status, 404, "{method}");
    }
    server.shutdown().await;
}
