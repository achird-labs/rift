//! Issue #1506: startup-source behaviour that only the real binary shows, ported from the deleted
//! rift-conformance specs (`DataDirPersistenceSpec`, `MbEjsStringify89Spec`).

mod support;

fn free_port() -> u16 {
    std::net::TcpListener::bind("0.0.0.0:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

async fn get(url: &str) -> (u16, String) {
    let response = reqwest::get(url).await.expect(url);
    (
        response.status().as_u16(),
        response.text().await.expect("body"),
    )
}

/// The port of the only imposter listed by the admin API.
async fn only_imposter_port(admin: u16) -> u16 {
    let (status, listed) = get(&format!("http://127.0.0.1:{admin}/imposters")).await;
    assert_eq!(status, 200, "{listed}");
    let listed: serde_json::Value = serde_json::from_str(&listed).expect("json");
    let imposters = listed["imposters"].as_array().expect("imposters array");
    assert_eq!(imposters.len(), 1, "got: {listed}");
    u16::try_from(imposters[0]["port"].as_u64().expect("port")).expect("u16 port")
}

/// The port of the imposter listed besides `known`.
async fn only_other_port(admin: u16, known: u16) -> u16 {
    let (_, listed) = get(&format!("http://127.0.0.1:{admin}/imposters")).await;
    let listed: serde_json::Value = serde_json::from_str(&listed).expect("json");
    let ports: Vec<u16> = listed["imposters"]
        .as_array()
        .expect("imposters array")
        .iter()
        .map(|i| u16::try_from(i["port"].as_u64().expect("port")).expect("u16 port"))
        .filter(|port| *port != known)
        .collect();
    assert_eq!(ports.len(), 1, "got: {listed}");
    ports[0]
}

/// Spawn the server with `--port 0` and its stdout and stderr appended to `log`; returns it with
/// the admin port read from the log. Startup output (the skipped-file summary) is what the caller
/// asserts on, which `support::spawn_server` discards.
async fn spawn_logging(args: &[&str], log: &std::path::Path) -> (tokio::process::Child, u16) {
    let file = std::fs::File::create(log).expect("create log");
    let child = tokio::process::Command::new(support::server_bin())
        .args(["--port", "0", "--host", "127.0.0.1"])
        .args(args)
        .env("NO_COLOR", "1")
        .stdout(file.try_clone().expect("clone log"))
        .stderr(file)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn rift server");
    for _ in 0..100 {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        let port = text
            .lines()
            .filter_map(|l| l.split("listening on http://").nth(1))
            .filter_map(|rest| {
                rest.split_whitespace()
                    .next()?
                    .rsplit(':')
                    .next()?
                    .parse()
                    .ok()
            })
            .next();
        if let Some(port) = port {
            return (child, port);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!(
        "no admin port within 10s; output:\n{}",
        std::fs::read_to_string(log).unwrap_or_default()
    );
}

fn imposter(port: Option<u16>, body: &str) -> serde_json::Value {
    let mut imposter = serde_json::json!({
        "protocol": "http",
        "stubs": [{"responses": [{"is": {"statusCode": 200, "body": body}}]}]
    });
    if let Some(port) = port {
        imposter["port"] = port.into();
    }
    imposter
}

// `--configfile` imposters are never persisted, and win over a datadir file for the same port.
#[tokio::test]
async fn configfile_wins_over_a_datadir_file_for_the_same_port_and_is_never_persisted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let datadir = dir.path().join("data");
    std::fs::create_dir(&datadir).expect("datadir");
    let shared = free_port();
    let datadir_file = datadir.join(format!("{shared}.json"));
    std::fs::write(
        &datadir_file,
        imposter(Some(shared), "from-datadir").to_string(),
    )
    .expect("write datadir file");
    let configfile = dir.path().join("config.json");
    std::fs::write(
        &configfile,
        serde_json::json!([
            imposter(Some(shared), "from-config"),
            imposter(None, "config-only")
        ])
        .to_string(),
    )
    .expect("write configfile");

    let log = dir.path().join("rift.log");
    let (mut child, admin) = spawn_logging(
        &[
            "--datadir",
            datadir.to_str().expect("utf-8"),
            "--configfile",
            configfile.to_str().expect("utf-8"),
        ],
        &log,
    )
    .await;

    assert_eq!(
        get(&format!("http://127.0.0.1:{shared}/")).await,
        (200, "from-config".to_string())
    );
    let config_only = only_other_port(admin, shared).await;
    assert_eq!(
        get(&format!("http://127.0.0.1:{config_only}/")).await,
        (200, "config-only".to_string())
    );

    let files: Vec<String> = std::fs::read_dir(&datadir)
        .expect("read datadir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files, vec![format!("{shared}.json")], "nothing persisted");
    let kept = std::fs::read_to_string(&datadir_file).expect("datadir file");
    assert!(kept.contains("from-datadir"), "loser untouched: {kept}");

    let output = std::fs::read_to_string(&log).expect("log");
    assert!(
        output.contains(&format!("{shared}.json")) && output.contains("imposter creation failed"),
        "startup output names the skipped datadir file: {output}"
    );
    child.kill().await.expect("stop server");
}

// A `--configfile` EJS `stringify` directive inlines the referenced file as a JSON string.
#[tokio::test]
async fn configfile_stringify_directive_inlines_the_referenced_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("greeting.txt"), "hello \"quoted\"\nline2").expect("write");
    let configfile = dir.path().join("config.ejs");
    std::fs::write(
        &configfile,
        r#"{"imposters": [{"protocol": "http",
            "stubs": [{"responses": [{"is": {"body": "<%- stringify('greeting.txt') %>"}}]}]}]}"#,
    )
    .expect("write configfile");

    let (mut child, admin) = support::spawn_server(&[
        "--host",
        "127.0.0.1",
        "--configfile",
        configfile.to_str().expect("utf-8"),
    ])
    .await;
    let port = only_imposter_port(admin).await;
    assert_eq!(
        get(&format!("http://127.0.0.1:{port}/")).await,
        (200, "hello \"quoted\"\nline2".to_string())
    );
    child.kill().await.expect("stop server");
}
