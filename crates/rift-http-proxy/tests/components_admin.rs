//! Issue #1757: `POST/GET/DELETE /components`, the `_rift.script.component` reference, the dangling
//! check at the admin doors, `--strict-wasm` and the datadir copy.
#![cfg(feature = "wasm")]

use clap::Parser;
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use serde_json::{Value, json};
use std::path::Path;

#[path = "../../rift-mock-core/test-support/wasm_fixtures.rs"]
mod wasm_fixtures;

async fn start_with(extra: &[&str]) -> (RunningServer, String) {
    let mut args = vec!["rift", "--host", "127.0.0.1", "--port", "0"];
    args.extend_from_slice(extra);
    let cli = Cli::try_parse_from(args).expect("cli");
    let server = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("server starts");
    let admin = format!("http://{}", server.admin_addr());
    (server, admin)
}

async fn start() -> (RunningServer, String) {
    start_with(&[]).await
}

fn sha_of(bytes: &[u8]) -> String {
    rift_http_proxy::imposter::components::ComponentId::of(bytes).hex()
}

async fn upload_as(admin: &str, content_type: &str, bytes: Vec<u8>) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{admin}/components"))
        .header("content-type", content_type)
        .body(bytes)
        .send()
        .await
        .expect("upload");
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn upload(admin: &str, bytes: Vec<u8>) -> (u16, Value) {
    upload_as(admin, "application/wasm", bytes).await
}

async fn send(method: reqwest::Method, url: &str, body: Option<&Value>) -> (u16, Value) {
    let mut req = reqwest::Client::new().request(method, url);
    if let Some(body) = body {
        req = req.json(body);
    }
    let resp = req.send().await.expect("admin request");
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn imposter_using(sha: &str) -> Value {
    json!({ "protocol": "http", "port": 0, "stubs": [{ "responses": [
        { "_rift": { "script": { "component": format!("sha256:{sha}") } } }] }] })
}

async fn create_imposter(admin: &str, config: &Value) -> (u16, Value) {
    send(
        reqwest::Method::POST,
        &format!("{admin}/imposters"),
        Some(config),
    )
    .await
}

fn port_of(body: &Value) -> u16 {
    u16::try_from(body["port"].as_u64().expect("port")).expect("port fits")
}

#[tokio::test]
async fn upload_is_201_then_200_with_the_same_body() {
    let (server, admin) = start().await;
    let bytes = wasm_fixtures::respond_ok();
    let sha = sha_of(&bytes);

    let (status, first) = upload(&admin, bytes.clone()).await;
    assert_eq!(status, 201, "{first}");
    assert_eq!(first["sha256"], sha);
    assert_eq!(first["size"], bytes.len());
    assert_eq!(first["world"], "rift:mock/mock");
    assert!(first["imports"].is_array());

    let (status, again) = upload(&admin, bytes).await;
    assert_eq!(status, 200);
    assert_eq!(again, first);
    server.shutdown().await;
}

#[tokio::test]
async fn upload_refusals_are_415_413_and_400() {
    let (server, admin) = start_with(&["--max-body-size", "4096"]).await;

    let (status, body) = upload_as(&admin, "application/json", wasm_fixtures::respond_ok()).await;
    assert_eq!(status, 415, "{body}");
    assert_eq!(body["errors"][0]["code"], "415");

    let resp = reqwest::Client::new()
        .post(format!("{admin}/components"))
        .body(wasm_fixtures::respond_ok())
        .send()
        .await
        .expect("upload");
    assert_eq!(resp.status().as_u16(), 415, "no Content-Type at all");

    let (status, body) = upload(&admin, vec![0u8; 8192]).await;
    assert_eq!(status, 413, "{body}");

    let (status, body) = upload(&admin, wasm_fixtures::core_module()).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errors"][0]["type"], "bad data");
    server.shutdown().await;
}

// A component has a cap of its own, below the admin body limit: the registry keeps it for the
// process's life.
#[tokio::test]
async fn a_component_past_its_own_cap_is_413() {
    let (server, admin) = start().await;
    let (status, body) = upload(&admin, vec![0u8; (64 << 20) + 1]).await;
    assert_eq!(status, 413, "{body}");
    assert!(
        body.to_string().contains("a component may be at most"),
        "{body}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn an_imposter_serves_an_uploaded_component() {
    let (server, admin) = start().await;
    let sha = sha_of(&wasm_fixtures::respond_ok());
    upload(&admin, wasm_fixtures::respond_ok()).await;

    let (status, body) = create_imposter(&admin, &imposter_using(&sha)).await;
    assert_eq!(status, 201, "{body}");
    let served = reqwest::get(format!("http://127.0.0.1:{}/anything", port_of(&body)))
        .await
        .expect("request");
    assert_eq!(served.status().as_u16(), 200);
    server.shutdown().await;
}

#[tokio::test]
async fn a_dangling_component_is_refused_at_every_admin_door() {
    let (server, admin) = start().await;
    let sha = "b".repeat(64);
    let dangling = json!({ "responses": [{ "_rift": { "script": {
        "component": format!("sha256:{sha}") } } }] });
    let want = format!("unknown component sha256:{sha}; upload it with POST /components");

    let (status, body) = create_imposter(&admin, &imposter_using(&sha)).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errors"][0]["message"], want);
    assert_eq!(body["errors"][0]["type"], "bad data");

    let (status, body) = send(
        reqwest::Method::PUT,
        &format!("{admin}/imposters"),
        Some(&json!({ "imposters": [imposter_using(&sha)] })),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // A registry entry of a whole imposter counts too.
    let (status, body) = create_imposter(
        &admin,
        &json!({ "protocol": "http", "port": 0, "_rift": { "scripts": {
            "s": { "component": format!("sha256:{sha}") } } }, "stubs": [] }),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // A malformed reference is refused with the parse error.
    let (status, body) = create_imposter(
        &admin,
        &json!({ "protocol": "http", "port": 0, "stubs": [{ "responses": [
            { "_rift": { "script": { "component": "sha256:nothex" } } }] }] }),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // The stub doors, against a plain imposter.
    let (status, body) = create_imposter(
        &admin,
        &json!({ "protocol": "http", "port": 0, "stubs": [] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let port = port_of(&body);
    let base = format!("{admin}/imposters/{port}");
    for (method, path, payload) in [
        (reqwest::Method::POST, "/stubs", json!({ "stub": dangling })),
        (
            reqwest::Method::PUT,
            "/stubs",
            json!({ "stubs": [dangling] }),
        ),
        (reqwest::Method::POST, "/spaces/f1/stubs", dangling.clone()),
    ] {
        let (status, body) = send(method.clone(), &format!("{base}{path}"), Some(&payload)).await;
        assert_eq!(status, 400, "{method} {path}: {body}");
        assert_eq!(body["errors"][0]["message"], want, "{method} {path}");
    }
    let (status, body) = send(
        reqwest::Method::POST,
        &format!("{base}/stubs"),
        Some(&json!({ "stub": { "responses": [{ "is": { "statusCode": 201 } }] } })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    for path in ["/stubs/0", "/stubs/by-id/missing"] {
        let (status, body) = send(
            reqwest::Method::PUT,
            &format!("{base}{path}"),
            Some(&dangling),
        )
        .await;
        assert_eq!(status, 400, "PUT {path}: {body}");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn delete_is_409_while_referenced_then_204() {
    let (server, admin) = start().await;
    let sha = sha_of(&wasm_fixtures::respond_ok());
    upload(&admin, wasm_fixtures::respond_ok()).await;
    let (_, body) = create_imposter(&admin, &imposter_using(&sha)).await;
    let port = port_of(&body);
    let url = format!("{admin}/components/{sha}");

    let (status, body) = send(reqwest::Method::DELETE, &url, None).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["errors"][0]["code"], "409");
    assert_eq!(
        body["errors"][0]["message"],
        format!("component is referenced by imposters {port}")
    );

    let (_, listed) = send(reqwest::Method::GET, &url, None).await;
    assert_eq!(listed["referencedBy"], json!([port]));

    let (status, _) = send(
        reqwest::Method::DELETE,
        &format!("{admin}/imposters/{port}"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = send(reqwest::Method::DELETE, &url, None).await;
    assert_eq!(status, 204);
    let (status, _) = send(reqwest::Method::DELETE, &url, None).await;
    assert_eq!(status, 404);
    server.shutdown().await;
}

#[tokio::test]
async fn list_and_get() {
    let (server, admin) = start().await;
    let (status, empty) = send(reqwest::Method::GET, &format!("{admin}/components"), None).await;
    assert_eq!(status, 200);
    assert_eq!(empty, json!({ "components": [] }));

    let bytes = wasm_fixtures::respond_ok();
    let sha = sha_of(&bytes);
    upload(&admin, bytes).await;

    let (_, listed) = send(reqwest::Method::GET, &format!("{admin}/components/"), None).await;
    let one = &listed["components"][0];
    assert_eq!(one["sha256"], sha);
    assert_eq!(one["referencedBy"], json!([]));
    assert!(
        one["uploadedAt"].as_str().is_some_and(|t| t.ends_with('Z')),
        "{one}"
    );

    let (status, got) = send(
        reqwest::Method::GET,
        &format!("{admin}/components/{sha}"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(&got, one);

    let (status, _) = send(
        reqwest::Method::GET,
        &format!("{admin}/components/{}", "c".repeat(64)),
        None,
    )
    .await;
    assert_eq!(status, 404);
    for bad in ["nothex", "sha256:aaaa", &"A".repeat(64)] {
        let (status, body) = send(
            reqwest::Method::GET,
            &format!("{admin}/components/{bad}"),
            None,
        )
        .await;
        assert_eq!(status, 400, "{bad}: {body}");
    }
    server.shutdown().await;
}

fn trust_file(dir: &Path, shas: &[String]) -> String {
    let path = dir.join("trusted.txt");
    let mut text = String::from("# trusted components\n");
    for sha in shas {
        text.push_str(&format!("{sha}  # one\n"));
    }
    std::fs::write(&path, text).expect("write trust list");
    path.to_str().expect("utf-8").to_owned()
}

#[tokio::test]
async fn strict_wasm_refuses_what_it_does_not_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let listed = wasm_fixtures::respond_ok();
    let trusted = trust_file(dir.path(), &[sha_of(&listed)]);
    let (server, admin) = start_with(&["--strict-wasm", &trusted]).await;

    let (status, body) = upload(&admin, wasm_fixtures::respond_bare(1)).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("--strict-wasm")),
        "{body}"
    );

    let (status, body) = create_imposter(
        &admin,
        &json!({ "protocol": "http", "port": 0, "stubs": [{ "responses": [{ "_rift": { "script": {
            "engine": "wasm", "code": wasm_fixtures::b64(&wasm_fixtures::respond_bare(1)) } } }] }] }),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    let (status, _) = upload(&admin, listed.clone()).await;
    assert_eq!(status, 201);
    let (status, body) = create_imposter(&admin, &imposter_using(&sha_of(&listed))).await;
    assert_eq!(status, 201, "{body}");
    server.shutdown().await;
}

#[tokio::test]
async fn strict_wasm_file_errors_abort_startup_naming_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("nope.txt");
    let bad = dir.path().join("bad.txt");
    std::fs::write(&bad, "not-a-sha\n").expect("write");
    for path in [missing, bad] {
        let cli = Cli::try_parse_from([
            "rift",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--strict-wasm",
            path.to_str().expect("utf-8"),
        ])
        .expect("cli");
        let err = ServerBuilder::from_cli(cli)
            .start()
            .await
            .err()
            .expect("startup aborts");
        let message = format!("{err:#}");
        assert!(message.contains(path.to_str().expect("utf-8")), "{message}");
    }
}

#[tokio::test]
async fn a_datadir_keeps_components_across_a_restart_and_skips_a_tampered_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let datadir = dir.path().to_str().expect("utf-8").to_owned();
    let kept = wasm_fixtures::respond_ok();
    let sha = sha_of(&kept);

    let (server, admin) = start_with(&["--datadir", &datadir]).await;
    assert_eq!(upload(&admin, kept.clone()).await.0, 201);
    let file = dir.path().join("components").join(format!("{sha}.wasm"));
    assert_eq!(std::fs::read(&file).expect("component file"), kept);
    server.shutdown().await;

    // A file whose bytes do not hash to its name.
    let forged = dir
        .path()
        .join("components")
        .join(format!("{}.wasm", "d".repeat(64)));
    std::fs::write(&forged, &kept).expect("write forged");

    let (server, admin) = start_with(&["--datadir", &datadir]).await;
    let (_, listed) = send(reqwest::Method::GET, &format!("{admin}/components"), None).await;
    let shas: Vec<&str> = listed["components"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["sha256"].as_str())
        .collect();
    assert_eq!(shas, [sha.as_str()], "the forged file is not loaded");

    // Delete removes the file too.
    let (status, _) = send(
        reqwest::Method::DELETE,
        &format!("{admin}/components/{sha}"),
        None,
    )
    .await;
    assert_eq!(status, 204);
    assert!(!file.exists());
    server.shutdown().await;
}
