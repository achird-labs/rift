//! Issue #1160: `POST /admin/reload` parsed and validated the config file's `routes` block and then
//! dropped it — the front door kept the old table and the response said nothing. It now swaps the
//! reloaded table in, as a restart would; and a `routes` block with no front door to serve it is
//! reported rather than silently discarded.

use std::net::TcpListener;
use std::path::Path;

use clap::Parser;
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// One imposter answering `pong` on `/ping`; `routes` is written verbatim when given.
fn write_config(path: &Path, imposter_port: u16, routes: Option<&str>) {
    let routes = routes.map_or(String::new(), |r| {
        format!(r#","routes":{{"routes":[{r}]}}"#)
    });
    std::fs::write(
        path,
        format!(
            r#"{{"imposters":[{{"port":{imposter_port},"protocol":"http","stubs":[
                {{"predicates":[{{"equals":{{"path":"/ping"}}}}],
                  "responses":[{{"is":{{"statusCode":200,"body":"pong"}}}}]}}]}}]{routes}}}"#
        ),
    )
    .expect("write config");
}

fn route(id: &str, host: &str, port: u16) -> String {
    format!(r#"{{"id":"{id}","match":{{"host":"{host}"}},"target":{{"port":{port}}}}}"#)
}

/// Write a config whose imposter listens on a free port, with `routes` built for that port, and
/// start a server on it — retrying on a fresh port when the imposter did not get its port.
///
/// `free_port` releases the port before the server binds it, so a test running in parallel can
/// take it in between; the boot then serves without the imposter (a bind failure there is logged,
/// not fatal) and the first reload fails retrying the bind (`Address already in use`).
async fn start_with_imposter(
    config: &Path,
    front_door: bool,
    routes: impl Fn(u16) -> Option<String>,
) -> (RunningServer, u16) {
    for _ in 0..5 {
        let imposter = free_port();
        write_config(config, imposter, routes(imposter).as_deref());
        let server = start(config, front_door).await;
        let bound = reqwest::get(format!(
            "http://{}/imposters/{imposter}",
            server.admin_addr()
        ))
        .await
        .is_ok_and(|r| r.status().is_success());
        if bound {
            return (server, imposter);
        }
        server.shutdown().await;
    }
    panic!("no free port for the imposter after 5 attempts");
}

async fn start(config: &Path, front_door: bool) -> RunningServer {
    let mut args = vec![
        "rift".to_owned(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        "0".into(),
        "--metrics-port".into(),
        "0".into(),
        "--configfile".into(),
        config.to_str().expect("utf8").into(),
    ];
    if front_door {
        args.extend(["--front-door".into(), "127.0.0.1:0".into()]);
    }
    ServerBuilder::from_cli(Cli::try_parse_from(args).expect("cli"))
        .start()
        .await
        .expect("start")
}

async fn reload(server: &RunningServer) -> (u16, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", server.admin_addr()))
        .send()
        .await
        .expect("reload");
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or_default())
}

/// Status and the front door's no-route marker for a request to `host`.
async fn through_front_door(server: &RunningServer, host: &str) -> (u16, Option<String>) {
    let addr = server.front_door_addr().expect("front door");
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/ping"))
        .header("Host", host)
        .send()
        .await
        .expect("front door request");
    let marker = response
        .headers()
        .get("x-rift-front-door")
        .map(|v| v.to_str().expect("ascii").to_owned());
    (response.status().as_u16(), marker)
}

const ROUTED: (u16, Option<String>) = (200, None);

fn no_route() -> (u16, Option<String>) {
    (404, Some("no-route".to_owned()))
}

#[tokio::test]
async fn a_reload_swaps_in_the_edited_route_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("imposters.json");
    let (server, imposter) =
        start_with_imposter(&config, true, |p| Some(route("a", "a.test", p))).await;
    assert_eq!(through_front_door(&server, "a.test").await, ROUTED);

    // Only `routes` changes: the imposters are byte-identical, so the diff has nothing to do.
    write_config(&config, imposter, Some(&route("b", "b.test", imposter)));
    let (status, body) = reload(&server).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("warnings").is_none(), "{body}");
    assert_eq!(through_front_door(&server, "b.test").await, ROUTED);
    assert_eq!(through_front_door(&server, "a.test").await, no_route());

    // No block at all reloads to the empty table, exactly what a restart would give.
    write_config(&config, imposter, None);
    let (status, body) = reload(&server).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(through_front_door(&server, "b.test").await, no_route());

    server.shutdown().await;
}

#[tokio::test]
async fn an_invalid_route_table_is_refused_and_the_old_one_keeps_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("imposters.json");
    let (server, imposter) =
        start_with_imposter(&config, true, |p| Some(route("a", "a.test", p))).await;

    let duplicate = format!(
        "{},{}",
        route("dup", "a.test", imposter),
        route("dup", "b.test", imposter)
    );
    write_config(&config, imposter, Some(&duplicate));
    let (status, body) = reload(&server).await;
    assert_eq!(status, 500, "{body}");
    assert_eq!(through_front_door(&server, "a.test").await, ROUTED);

    server.shutdown().await;
}

#[tokio::test]
async fn a_routes_block_without_a_front_door_is_reported_not_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("imposters.json");
    let (server, imposter) =
        start_with_imposter(&config, false, |p| Some(route("a", "a.test", p))).await;

    let (status, body) = reload(&server).await;
    assert_eq!(status, 200, "{body}");
    let warnings = body["warnings"].as_array().expect("warnings present");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().is_some_and(|w| w.contains("--front-door"))),
        "{body}"
    );

    // And a config without `routes` says nothing about it.
    write_config(&config, imposter, None);
    let (_, body) = reload(&server).await;
    assert!(body.get("warnings").is_none(), "{body}");

    server.shutdown().await;
}
