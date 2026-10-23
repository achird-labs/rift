//! Issue #1654: a `lookup` behavior reads its CSV file under the server's body limits. A file
//! larger than `--max-body-size` is not read: a warning names the flag and the tokens are left in
//! place, and the server stays up. A file within the ceiling still serves its row.
//!
//! The server is the real binary, its ports chosen by the OS.

mod support;

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

/// A running server whose stdout and stderr go to `log`.
struct Server {
    _child: tokio::process::Child,
    admin: u16,
    log: PathBuf,
}

impl Server {
    async fn spawn(dir: &std::path::Path, args: &[&str]) -> Self {
        let log = dir.join("rift.log");
        let file = std::fs::File::create(&log).expect("create log");
        let child = tokio::process::Command::new(support::server_bin())
            .args(["--port", "0", "--host", "127.0.0.1", "--metrics-port", "0"])
            .args(args)
            .current_dir(dir)
            .env("NO_COLOR", "1")
            .stdout(file.try_clone().expect("clone log"))
            .stderr(file)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn rift server");
        for _ in 0..100 {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
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
            if let Some(admin) = port {
                return Self {
                    _child: child,
                    admin,
                    log,
                };
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "no admin port within 10s; output:\n{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
    }

    async fn create(&self, imposter: &Value) -> u16 {
        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/imposters", self.admin))
            .json(imposter)
            .send()
            .await
            .expect("post");
        let status = resp.status().as_u16();
        let body = resp.text().await.expect("body");
        assert_eq!(status, 201, "create failed: {body}");
        let body: Value = serde_json::from_str(&body).expect("create body");
        u16::try_from(body["port"].as_u64().expect("port")).expect("u16 port")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

/// An imposter whose body on `/big` and `/small` is the `name` column of the row `?id=` keys, read
/// from `big.csv` and `small.csv` under `--scripts-dir`.
fn imposter() -> Value {
    let stub = |route: &str, csv: &str| {
        json!({
            "predicates": [{ "equals": { "path": route } }],
            "responses": [{
                "is": { "body": "name=${row}[name]" },
                "_behaviors": { "lookup": {
                    "key": { "from": { "query": "id" }, "using": { "method": "regex", "selector": ".*" } },
                    "fromDataSource": { "csv": { "path": csv, "keyColumn": "id" } },
                    "into": "${row}"
                } }
            }]
        })
    };
    json!({ "protocol": "http", "stubs": [stub("/big", "big.csv"), stub("/small", "small.csv")] })
}

async fn get(port: u16, path: &str) -> (u16, String) {
    let resp = reqwest::get(format!("http://127.0.0.1:{port}{path}"))
        .await
        .expect("send");
    let status = resp.status().as_u16();
    (status, resp.text().await.expect("body"))
}

#[tokio::test]
async fn a_csv_over_the_body_ceiling_is_not_read_and_one_within_it_is() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical tempdir");
    let sd = root.join("sd");
    std::fs::create_dir_all(&sd).expect("mkdir");
    // 4 KiB: four times the ceiling.
    let mut big = String::from("id,name\n1,BIG_1654\n");
    while big.len() < 4096 {
        big.push_str("9,padding\n");
    }
    std::fs::write(sd.join("big.csv"), &big).expect("write big csv");
    std::fs::write(sd.join("small.csv"), "id,name\n1,SMALL_1654\n").expect("write small csv");

    let server = Server::spawn(
        &root,
        &[
            "--scripts-dir",
            sd.to_str().expect("utf-8 path"),
            "--max-body-size",
            "1024",
            "--body-memory-budget",
            "1024",
        ],
    )
    .await;
    let port = server.create(&imposter()).await;

    let (status, body) = get(port, "/big?id=1").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body, "name=${row}[name]",
        "a file over --max-body-size is not read, so its tokens stay"
    );
    assert!(
        server.log().contains("--max-body-size"),
        "the warning names the flag:\n{}",
        server.log()
    );

    let (status, body) = get(port, "/small?id=1").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body, "name=SMALL_1654",
        "a file within the ceiling serves its row"
    );

    // Still up, and the oversized file is still refused rather than cached.
    let (_, body) = get(port, "/big?id=1").await;
    assert_eq!(body, "name=${row}[name]");
}
