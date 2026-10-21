//! Issue #1617: a `lookup` behavior reads its CSV file only where its imposter's door allows, which
//! is a deliberate departure from Mountebank, where it reads any path.
//!
//! An imposter written over the admin API (or from `--datadir`, or an `https:` source) reads CSV
//! files only under `--scripts-dir`, a relative path from there, and none without it. The admin API
//! refuses such a path when the imposter is written; an imposter that reaches the engine another way
//! (a stored `--datadir` file) loads, and its lookup is refused each time it runs. A local
//! `--configfile` keeps reading any path. A refusal never depends on what exists outside the root.
//!
//! Every file read here is a throwaway canary in a temp dir. The server is the real binary.

mod support;

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

/// The canary tree: `sd/` is `--scripts-dir`, `outside/` holds files no admin-API imposter may
/// read, and `cwd/` is the server's working directory.
struct Tree {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

const INSIDE: &str = "id,name\n1,IN_ROOT_1617\n";
const CANARY: &str = "CANARY_OUTSIDE_1617";

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // Canonical, so an absolute path written into a config is the one the server sees.
        let root = dir.path().canonicalize().expect("canonical tempdir");
        for sub in ["sd/nested", "outside", "cwd", "datadir"] {
            std::fs::create_dir_all(root.join(sub)).expect("mkdir");
        }
        let files: &[(&str, &str)] = &[
            ("sd/data.csv", INSIDE),
            ("sd/nested/data.csv", INSIDE),
            ("outside/data.csv", "id,name\n1,CANARY_OUTSIDE_1617\n"),
            // What a relative path resolved to before #1617: the working directory.
            ("cwd/data.csv", "id,name\n1,CWD_1617\n"),
        ];
        for (path, content) in files {
            std::fs::write(root.join(path), content).expect("write canary");
        }
        Self { _dir: dir, root }
    }

    fn path(&self, rel: &str) -> String {
        self.root
            .join(rel)
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    #[cfg(unix)]
    fn symlink(&self, link: &str, target: &str) {
        let link = self.root.join(link);
        // Replacing a link is how a file under the root changes under a running imposter.
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(self.root.join(target), link).expect("symlink");
    }
}

/// A running server whose stdout and stderr go to `log`.
struct Server {
    _child: tokio::process::Child,
    admin: u16,
    log: PathBuf,
}

impl Server {
    async fn spawn(tree: &Tree, args: &[&str]) -> Self {
        let log = tree.root.join(format!(
            "rift-{}.log",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let file = std::fs::File::create(&log).expect("create log");
        let child = tokio::process::Command::new(support::server_bin())
            .args(["--port", "0", "--host", "127.0.0.1", "--metrics-port", "0"])
            .args(args)
            .current_dir(tree.root.join("cwd"))
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

    async fn post(&self, path: &str, body: &Value) -> (u16, String) {
        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}{path}", self.admin))
            .json(body)
            .send()
            .await
            .expect("post");
        let status = resp.status().as_u16();
        (status, resp.text().await.expect("body"))
    }

    async fn create(&self, imposter: &Value) -> u16 {
        let (status, body) = self.post("/imposters", imposter).await;
        assert_eq!(status, 201, "create failed: {body}");
        let body: Value = serde_json::from_str(&body).expect("create body");
        u16::try_from(body["port"].as_u64().expect("port")).expect("u16 port")
    }

    async fn ports(&self) -> Vec<u16> {
        let body: Value = reqwest::get(format!("http://127.0.0.1:{}/imposters", self.admin))
            .await
            .expect("list")
            .json()
            .await
            .expect("list body");
        body["imposters"]
            .as_array()
            .expect("imposters")
            .iter()
            .map(|i| u16::try_from(i["port"].as_u64().expect("port")).expect("u16"))
            .collect()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

async fn confined(tree: &Tree) -> Server {
    Server::spawn(tree, &["--scripts-dir", &tree.path("sd")]).await
}

/// A stub on `/l` whose body is the `name` column of the row `?id=` keys, read from `csv` with
/// `delimiter`.
fn lookup_stub(route: &str, csv: &str, delimiter: Option<&str>) -> Value {
    let mut source = json!({ "path": csv, "keyColumn": "id" });
    if let Some(delimiter) = delimiter {
        source["delimiter"] = json!(delimiter);
    }
    json!({
        "predicates": [{ "equals": { "path": route } }],
        "responses": [{
            "is": { "body": "name=${row}[name]" },
            "_behaviors": { "lookup": {
                "key": { "from": { "query": "id" }, "using": { "method": "regex", "selector": ".*" } },
                "fromDataSource": { "csv": source },
                "into": "${row}"
            } }
        }]
    })
}

fn imposter(stubs: Vec<Value>) -> Value {
    json!({ "protocol": "http", "stubs": stubs })
}

async fn get(port: u16, path: &str) -> String {
    reqwest::get(format!("http://127.0.0.1:{port}{path}"))
        .await
        .expect("send")
        .text()
        .await
        .expect("body")
}

/// The admin API answers `400` with `message` and nothing of any file.
fn assert_refused(status: u16, body: &str, message: &str) {
    assert_eq!(status, 400, "the admin API must refuse it: {body}");
    assert!(body.contains(message), "names the refusal: {body}");
    assert!(!body.contains(CANARY), "never echoes the file: {body}");
}

#[tokio::test]
async fn an_absolute_path_outside_scripts_dir_is_refused() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    let outside = tree.path("outside/data.csv");
    let (status, body) = server
        .post(
            "/imposters",
            &imposter(vec![lookup_stub("/l", &outside, None)]),
        )
        .await;
    assert_refused(
        status,
        &body,
        &format!("lookup CSV path '{outside}' is outside --scripts-dir"),
    );
}

#[tokio::test]
async fn a_dotdot_escape_is_refused() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    for path in ["../outside/data.csv", "nested/../../outside/data.csv"] {
        let (status, body) = server
            .post("/imposters", &imposter(vec![lookup_stub("/l", path, None)]))
            .await;
        assert_refused(
            status,
            &body,
            &format!("lookup CSV path '{path}' is outside --scripts-dir"),
        );
    }
}

/// The answer for a path outside the root is the same whether or not anything is there.
#[tokio::test]
async fn a_refusal_is_the_same_for_a_missing_and_an_existing_outside_file() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    let mut answers = Vec::new();
    for path in ["outside/data.csv", "outside/missing.csv"] {
        let path = tree.path(path);
        let (status, body) = server
            .post(
                "/imposters",
                &imposter(vec![lookup_stub("/l", &path, None)]),
            )
            .await;
        answers.push((status, body.replace(&path, "<path>")));
    }
    assert_eq!(answers[0], answers[1]);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_root_is_refused() {
    let tree = Tree::new();
    tree.symlink("sd/link.csv", "outside/data.csv");
    tree.symlink("sd/dirlink", "outside");
    let server = confined(&tree).await;
    for path in ["link.csv", "dirlink/data.csv"] {
        let (status, body) = server
            .post("/imposters", &imposter(vec![lookup_stub("/l", path, None)]))
            .await;
        assert_refused(
            status,
            &body,
            &format!("lookup CSV path '{path}' is outside --scripts-dir"),
        );
    }
}

/// A link that pointed inside the root when the imposter was written, and out of it later, is
/// refused when the lookup runs: the path is resolved again on every use.
#[cfg(unix)]
#[tokio::test]
async fn a_link_moved_out_of_the_root_after_admission_is_refused_when_read() {
    let tree = Tree::new();
    tree.symlink("sd/swap.csv", "sd/data.csv");
    let server = confined(&tree).await;
    let port = server
        .create(&imposter(vec![lookup_stub(
            "/l",
            &tree.path("sd/swap.csv"),
            None,
        )]))
        .await;

    tree.symlink("sd/swap.csv", "outside/data.csv");
    let body = get(port, "/l?id=1").await;
    assert_eq!(body, "name=${row}[name]", "the lookup reads nothing");
    assert!(
        server.log().contains("lookup CSV path '")
            && server.log().contains("is outside --scripts-dir"),
        "the refusal is logged: {}",
        server.log()
    );
    assert!(
        !server.log().contains(CANARY),
        "the log carries no file content"
    );
}

#[tokio::test]
async fn a_relative_path_resolves_from_the_root_and_serves() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    let port = server
        .create(&imposter(vec![
            lookup_stub("/top", "data.csv", None),
            lookup_stub("/nested", "nested/./data.csv", None),
            lookup_stub("/abs", &tree.path("sd/data.csv"), None),
        ]))
        .await;
    for route in ["/top", "/nested", "/abs"] {
        assert_eq!(
            get(port, &format!("{route}?id=1")).await,
            "name=IN_ROOT_1617",
            "{route} reads the file under --scripts-dir"
        );
    }
}

/// A file that does not exist yet is admitted, as in Mountebank, and is read once it does.
#[tokio::test]
async fn a_missing_file_inside_the_root_is_admitted() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    let port = server
        .create(&imposter(vec![lookup_stub("/l", "later.csv", None)]))
        .await;
    assert_eq!(get(port, "/l?id=1").await, "name=${row}[name]");
    std::fs::write(tree.root.join("sd/later.csv"), INSIDE).expect("write later");
    assert_eq!(get(port, "/l?id=1").await, "name=IN_ROOT_1617");
}

/// The documented default without `--scripts-dir`: an admin-API lookup reads nothing.
#[tokio::test]
async fn without_scripts_dir_an_admin_lookup_is_refused() {
    let tree = Tree::new();
    let server = Server::spawn(&tree, &[]).await;
    for path in ["data.csv", &tree.path("outside/data.csv")] {
        let (status, body) = server
            .post("/imposters", &imposter(vec![lookup_stub("/l", path, None)]))
            .await;
        assert_refused(
            status,
            &body,
            &format!("lookup CSV path '{path}' cannot be read: no --scripts-dir is configured"),
        );
    }
}

/// Every admin write door refuses it, not only `POST /imposters`.
#[tokio::test]
async fn every_admin_write_door_refuses_an_outside_path() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    let port = server.create(&imposter(vec![])).await;
    let bad = lookup_stub("/l", "../outside/data.csv", None);
    let message = "lookup CSV path '../outside/data.csv' is outside --scripts-dir";

    let (status, body) = server
        .post(
            &format!("/imposters/{port}/stubs"),
            &json!({ "stub": bad.clone() }),
        )
        .await;
    assert_refused(status, &body, message);

    for (method, path, body) in [
        (
            reqwest::Method::PUT,
            format!("/imposters/{port}/stubs"),
            json!({ "stubs": [bad.clone()] }),
        ),
        (
            reqwest::Method::PUT,
            "/imposters".to_string(),
            json!({ "imposters": [imposter(vec![bad.clone()])] }),
        ),
    ] {
        let resp = reqwest::Client::new()
            .request(method, format!("http://127.0.0.1:{}{path}", server.admin))
            .json(&body)
            .send()
            .await
            .expect("send");
        let status = resp.status().as_u16();
        assert_refused(status, &resp.text().await.expect("body"), message);
    }
    assert_eq!(server.ports().await, vec![port], "nothing was replaced");
}

/// A stored `--datadir` file is never refused for it (its bytes were admitted once and must stay
/// loadable); its lookup is refused when it runs.
#[tokio::test]
async fn a_stored_datadir_lookup_outside_the_root_loads_and_reads_nothing() {
    let tree = Tree::new();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe")
        .local_addr()
        .expect("addr")
        .port();
    let mut stored = imposter(vec![lookup_stub(
        "/l",
        &tree.path("outside/data.csv"),
        None,
    )]);
    stored["port"] = json!(port);
    std::fs::write(
        tree.root.join(format!("datadir/{port}.json")),
        stored.to_string(),
    )
    .expect("write datadir file");

    let server = Server::spawn(
        &tree,
        &[
            "--datadir",
            &tree.path("datadir"),
            "--scripts-dir",
            &tree.path("sd"),
        ],
    )
    .await;
    assert_eq!(
        server.ports().await,
        vec![port],
        "the stored imposter loads"
    );
    assert_eq!(get(port, "/l?id=1").await, "name=${row}[name]");
    assert!(
        server.log().contains("is outside --scripts-dir"),
        "{}",
        server.log()
    );
}

/// A local `--configfile` imposter keeps reading any path, a relative one from the working
/// directory, as before.
#[tokio::test]
async fn a_configfile_lookup_reads_any_path() {
    let tree = Tree::new();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe")
        .local_addr()
        .expect("addr")
        .port();
    let mut config = imposter(vec![
        lookup_stub("/abs", &tree.path("outside/data.csv"), None),
        lookup_stub("/rel", "data.csv", None),
    ]);
    config["port"] = json!(port);
    let configfile = tree.root.join("imposters.json");
    std::fs::write(&configfile, json!({ "imposters": [config] }).to_string())
        .expect("write configfile");
    let _server = Server::spawn(
        &tree,
        &["--configfile", configfile.to_str().expect("utf-8")],
    )
    .await;
    assert_eq!(get(port, "/abs?id=1").await, format!("name={CANARY}"));
    assert_eq!(get(port, "/rel?id=1").await, "name=CWD_1617");
}

/// Two lookups reading one file with different delimiters each get their own parse (#1566's
/// fold-in): the cache is keyed by the canonical path and the delimiter, so the first parse is not
/// served to the second.
#[tokio::test]
async fn one_file_read_with_two_delimiters_is_parsed_twice() {
    let tree = Tree::new();
    std::fs::write(tree.root.join("sd/mixed.csv"), "k;v\n1;semi\n").expect("write mixed");
    let server = confined(&tree).await;
    // Absolute, so the engines before #1617 read it too and only the cache key is under test.
    let mixed = tree.path("sd/mixed.csv");
    let stub = |route: &str, key_column: &str, delimiter: &str, body: &str| {
        json!({
            "predicates": [{ "equals": { "path": route } }],
            "responses": [{
                "is": { "body": body },
                "_behaviors": { "lookup": {
                    "key": { "from": { "query": "id" }, "using": { "method": "regex", "selector": ".*" } },
                    "fromDataSource": { "csv": {
                        "path": mixed, "keyColumn": key_column, "delimiter": delimiter
                    } },
                    "into": "${row}"
                } }
            }]
        })
    };
    let port = server
        .create(&imposter(vec![
            // `,`: one column, `k;v`, holding `1;semi`.
            stub("/comma", "k;v", ",", "c=${row}[k;v]"),
            // `;`: columns `k` and `v`.
            stub("/semi", "k", ";", "s=${row}[v]"),
        ]))
        .await;
    assert_eq!(get(port, "/comma?id=1%3Bsemi").await, "c=1;semi");
    assert_eq!(get(port, "/semi?id=1").await, "s=semi");
}
