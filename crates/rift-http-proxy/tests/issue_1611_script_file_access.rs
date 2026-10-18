//! Issue #1611: a script's `require()` and Rhai `import` reach only the files their imposter's door
//! allows, and a refusal never says anything about a file outside that root.
//!
//! An imposter created over the admin API (or from `--datadir`, or an `https:` source) may
//! `require()` only modules under `--scripts-dir`, and nothing at all without one. Every
//! Mountebank-style hook run gets its own `require`, bound to its own imposter, so no run sees
//! another imposter's module cache. Rhai `import` is off on every door. A local `--configfile`
//! keeps the unconfined `require` its author already had (#305).
//!
//! Every file read here is a throwaway canary in a temp dir. The server is the real binary, so its
//! WARN log can be read back for the decorate path.
#![cfg(feature = "javascript")]

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// The canary tree: `sd/` is `--scripts-dir`, `outside/` holds the files no admin-API script may
/// read, and `cwd/` is the server's working directory.
struct Tree {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // Canonical, so an absolute path written into a script is the one the server sees.
        let root = dir.path().canonicalize().expect("canonical tempdir");
        for sub in ["sd/nested", "outside", "cwd"] {
            std::fs::create_dir_all(root.join(sub)).expect("mkdir");
        }
        let files: &[(&str, &str)] = &[
            ("sd/mod.js", "module.exports = 'IN_ROOT_1611';"),
            ("sd/flag.js", "module.exports = true;"),
            // A nested require resolves from the requiring module's own directory, as in Node.
            (
                "sd/nested/a.js",
                "module.exports = 'A+' + require('./b.js');",
            ),
            ("sd/nested/b.js", "module.exports = 'B_NESTED_1611';"),
            (
                "sd/nested/escape.js",
                "module.exports = require('../../outside/mod.js');",
            ),
            ("outside/mod.js", "module.exports = 'CANARY_OUTSIDE_1611';"),
            (
                "outside/secret.txt",
                "CANARY_TXT_1611 CANARY_TXT_1611 CANARY_TXT_1611\n",
            ),
            (
                "outside/dotenv.env",
                "API_KEY=canaryvalue1611\nDB_PASS=\"canary_quoted_1611\"\n",
            ),
            (
                "outside/leak.rhai",
                "export const X = \"CANARY_RHAI_1611\";\n",
            ),
            (
                "outside/notrhai.rhai",
                "let x = 1;\nAPI_KEY_RHAI_1611 = \"zz\";\n",
            ),
            (
                "sd/inroot.rhai",
                "export const X = \"IN_ROOT_RHAI_1611\";\n",
            ),
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

    fn sd(&self) -> String {
        self.path("sd")
    }
}

/// The same file named relative to `from`: as many `..` as `from` is deep, then the absolute path.
fn relative_from(from: &Path, to: &str) -> String {
    let ups = from.components().count().saturating_sub(1);
    format!("{}{}", "../".repeat(ups), to.trim_start_matches('/'))
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
            .env_remove("RIFT_STRICT_BEHAVIORS")
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

    async fn create(&self, imposter: Value) -> u16 {
        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/imposters", self.admin))
            .json(&imposter)
            .send()
            .await
            .expect("create");
        let status = resp.status();
        let body: Value = resp.json().await.expect("create body");
        assert_eq!(status, 201, "create failed: {body}");
        u16::try_from(body["port"].as_u64().expect("port")).expect("u16 port")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

/// The admin-API door: `--allowInjection --scripts-dir <sd>`.
async fn confined(tree: &Tree) -> Server {
    Server::spawn(tree, &["--allow-injection", "--scripts-dir", &tree.sd()]).await
}

async fn get(port: u16, path: &str) -> (u16, reqwest::header::HeaderMap, String) {
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("send");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    (status, headers, resp.text().await.expect("body"))
}

/// A string as a JavaScript string literal.
fn js(s: &str) -> String {
    serde_json::to_string(s).expect("json string")
}

/// A stub on `path` whose decorate requires `module`, reporting `OK:<exports>` or `ERR:<error>`.
fn decorate_reporting(path: &str, module: &str) -> Value {
    json!({
        "predicates": [{ "equals": { "path": path } }],
        "responses": [{
            "is": { "body": "orig" },
            "behaviors": [{ "decorate": format!(
                "config => {{ try {{ config.response.body = 'OK:' + JSON.stringify(require({})); }} \
                 catch (e) {{ config.response.body = 'ERR:' + String(e); }} }}",
                js(module)
            ) }]
        }]
    })
}

/// A stub on `path` whose decorate requires `module` with no `try`, so the error is the pipeline's.
fn decorate_throwing(path: &str, module: &str) -> Value {
    json!({
        "predicates": [{ "equals": { "path": path } }],
        "responses": [{
            "is": { "body": "orig" },
            "behaviors": [{ "decorate": format!(
                "config => {{ config.response.body = 'OK:' + JSON.stringify(require({})); }}",
                js(module)
            ) }]
        }]
    })
}

/// A stub on `path` whose inject requires `module` with no `try`.
fn inject_throwing(path: &str, module: &str) -> Value {
    json!({
        "predicates": [{ "equals": { "path": path } }],
        "responses": [{ "inject": format!(
            "config => ({{ statusCode: 200, body: 'OK:' + JSON.stringify(require({})) }})",
            js(module)
        ) }]
    })
}

/// A stub on `path` whose inject requires `module`, reporting `OK:<exports>` or `ERR:<error>`.
fn inject_reporting(path: &str, module: &str) -> Value {
    json!({
        "predicates": [{ "equals": { "path": path } }],
        "responses": [{ "inject": format!(
            "config => {{ try {{ return {{ body: 'OK:' + JSON.stringify(require({})) }}; }} \
             catch (e) {{ return {{ body: 'ERR:' + String(e) }}; }} }}",
            js(module)
        ) }]
    })
}

fn imposter(stubs: Vec<Value>) -> Value {
    json!({ "protocol": "http", "stubs": stubs })
}

const CANARIES: &[&str] = &[
    "CANARY_OUTSIDE_1611",
    "CANARY_TXT_1611",
    "canaryvalue1611",
    "canary_quoted_1611",
    "CANARY_RHAI_1611",
    "API_KEY_RHAI_1611",
];

fn assert_no_canary(what: &str, text: &str) {
    for canary in CANARIES {
        assert!(
            !text.contains(canary),
            "{what} carries the outside file's text {canary:?}: {text}"
        );
    }
}

#[tokio::test]
async fn an_admin_require_reads_only_under_scripts_dir() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    let outside = tree.path("outside/mod.js");
    let from_cwd = relative_from(&tree.root.join("cwd"), &outside);
    let port = server
        .create(imposter(vec![
            decorate_reporting("/in-abs", &tree.path("sd/mod.js")),
            decorate_reporting("/out-abs", &outside),
            decorate_reporting("/out-rel", &from_cwd),
            decorate_reporting("/out-dotdot", "../outside/mod.js"),
            decorate_reporting("/hosts", "/etc/hosts"),
        ]))
        .await;

    let (_, _, body) = get(port, "/in-abs").await;
    assert_eq!(body, "OK:\"IN_ROOT_1611\"");
    for path in ["/out-abs", "/out-rel", "/out-dotdot", "/hosts"] {
        let (_, _, body) = get(port, path).await;
        assert!(
            body.starts_with("ERR:") && body.contains("outside --scripts-dir"),
            "{path}: a module outside the root must be refused with the fixed message, got {body}"
        );
        assert_no_canary(path, &body);
    }
}

#[tokio::test]
async fn relative_requires_resolve_from_the_root_and_then_from_the_module() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    let port = server
        .create(imposter(vec![
            decorate_reporting("/top", "mod.js"),
            decorate_reporting("/nested", "nested/a.js"),
            decorate_reporting("/nested-escape", "nested/escape.js"),
        ]))
        .await;

    assert_eq!(get(port, "/top").await.2, "OK:\"IN_ROOT_1611\"");
    assert_eq!(get(port, "/nested").await.2, "OK:\"A+B_NESTED_1611\"");
    let (_, _, body) = get(port, "/nested-escape").await;
    assert!(
        body.contains("outside --scripts-dir"),
        "a module may not require its way out of the root: {body}"
    );
    assert_no_canary("/nested-escape", &body);
}

/// Run a decorate `require` on every MB pool worker, so a worker that later runs an inject has
/// already had a `require` installed (the stale global of probe 3, before the fix).
async fn warm_every_worker(server: &Server, tree: &Tree) {
    let warm = server
        .create(imposter(vec![decorate_reporting(
            "/",
            &tree.path("sd/mod.js"),
        )]))
        .await;
    for _ in 0..64 {
        get(warm, "/").await;
    }
}

#[tokio::test]
async fn an_inject_error_never_echoes_an_outside_file() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    warm_every_worker(&server, &tree).await;
    let port = server
        .create(imposter(vec![
            // A ReferenceError naming the file's value, and a SyntaxError naming its first token.
            inject_throwing("/dotenv", &tree.path("outside/dotenv.env")),
            inject_throwing("/syntax", &tree.path("outside/secret.txt")),
            inject_throwing("/module", &tree.path("outside/mod.js")),
        ]))
        .await;

    for path in ["/dotenv", "/syntax", "/module"] {
        let (status, _, body) = get(port, path).await;
        assert_no_canary(path, &body);
        assert_eq!(status, 400, "{path}: {body}");
        assert!(
            body.contains("outside --scripts-dir"),
            "{path}: the 400 names the fixed refusal: {body}"
        );
    }
}

#[tokio::test]
async fn a_refusal_is_the_same_for_a_missing_and_an_existing_outside_file() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    let present = tree.path("outside/mod.js");
    let missing = tree.path("outside/no-such-module.js");
    let port = server
        .create(imposter(vec![
            decorate_reporting("/present", &present),
            decorate_reporting("/missing", &missing),
        ]))
        .await;

    let (_, _, present_body) = get(port, "/present").await;
    let (_, _, missing_body) = get(port, "/missing").await;
    assert_eq!(
        present_body.replace(&present, "<p>"),
        missing_body.replace(&missing, "<p>"),
        "whether an outside file exists must not show in the refusal"
    );
    for body in [&present_body, &missing_body] {
        assert!(
            !body.contains("No such file") && !body.contains("os error"),
            "{body}"
        );
    }
}

#[tokio::test]
async fn a_failed_decorate_logs_no_file_content() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    let port = server
        .create(imposter(vec![
            decorate_throwing("/dotenv", &tree.path("outside/dotenv.env")),
            decorate_throwing("/syntax", &tree.path("outside/secret.txt")),
        ]))
        .await;

    for path in ["/dotenv", "/syntax"] {
        let (_, headers, body) = get(port, path).await;
        assert_eq!(
            headers
                .get("x-rift-decorate-error")
                .map(|v| v.to_str().unwrap_or_default()),
            Some("true"),
            "{path}: {body}"
        );
        assert_eq!(body, "orig", "{path}: the stub's own body is served");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = server.log();
    assert_no_canary("the server log", &log);
    assert!(
        log.contains("outside --scripts-dir"),
        "the WARN line names the refusal:\n{log}"
    );
}

#[tokio::test]
async fn rhai_import_is_refused_on_the_admin_door() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    let script = |path: &str, module: &str| {
        json!({
            "predicates": [{ "equals": { "path": path } }],
            "responses": [{ "_rift": { "script": { "engine": "rhai", "code": format!(
                "import {} as m; fn respond(ctx) {{ http(200, m::X) }}",
                js(module)
            ) } } }]
        })
    };
    let port = server
        .create(imposter(vec![
            script("/leak", &tree.path("outside/leak")),
            script("/notrhai", &tree.path("outside/notrhai")),
            // Off everywhere, not only outside the root.
            script("/inroot", &tree.path("sd/inroot")),
        ]))
        .await;

    for path in ["/leak", "/notrhai", "/inroot"] {
        let (status, _, body) = get(port, path).await;
        assert_ne!(status, 200, "{path}: an import must not load: {body}");
        assert!(
            !body.contains("IN_ROOT_RHAI_1611"),
            "{path}: no module loads: {body}"
        );
        assert_no_canary(path, &body);
    }
    assert_no_canary("the server log", &server.log());
}

#[tokio::test]
async fn every_hook_gets_a_require_bound_to_its_own_imposter() {
    let tree = Tree::new();
    let server = confined(&tree).await;
    // Imposter A loads a module through a decorate, on every pool worker.
    let a_only = tree.path("sd/a_only.js");
    std::fs::write(&a_only, "module.exports = 'A_PRIVATE_EXPORT';").expect("write a_only");
    let a = server
        .create(imposter(vec![decorate_reporting("/", &a_only)]))
        .await;
    for _ in 0..64 {
        assert_eq!(get(a, "/").await.2, "OK:\"A_PRIVATE_EXPORT\"");
    }
    std::fs::remove_file(&a_only).expect("remove a_only");

    // Imposter B asks for the same path: it gets its own require and a fresh cache, never A's.
    let b = server
        .create(imposter(vec![
            inject_reporting("/inject", &a_only),
            json!({
                "predicates": [{ "inject": format!(
                    "config => require({}) === true",
                    js(&tree.path("sd/flag.js"))
                ) }],
                "responses": [{ "is": { "body": "predicate saw require" } }]
            }),
        ]))
        .await;
    for _ in 0..32 {
        let (_, _, body) = get(b, "/inject").await;
        assert!(
            body.starts_with("ERR:") && !body.contains("A_PRIVATE_EXPORT"),
            "imposter B must not see imposter A's cached module: {body}"
        );
    }
    // A predicate inject has a require too, as in Mountebank.
    let (status, _, body) = get(b, "/predicate").await;
    assert_eq!((status, body.as_str()), (200, "predicate saw require"));
}

#[tokio::test]
async fn without_scripts_dir_every_admin_require_is_refused() {
    let tree = Tree::new();
    let server = Server::spawn(&tree, &["--allow-injection"]).await;
    let port = server
        .create(imposter(vec![
            decorate_reporting("/in", &tree.path("sd/mod.js")),
            decorate_reporting("/out", &tree.path("outside/mod.js")),
        ]))
        .await;
    for path in ["/in", "/out"] {
        let (_, _, body) = get(port, path).await;
        assert!(
            body.starts_with("ERR:") && body.contains("no --scripts-dir is configured"),
            "{path}: {body}"
        );
        assert_no_canary(path, &body);
    }
}

#[tokio::test]
async fn a_local_configfile_keeps_its_unconfined_require_and_loses_rhai_import() {
    let tree = Tree::new();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let mut config = imposter(vec![
        decorate_reporting("/require", &tree.path("outside/mod.js")),
        json!({
            "predicates": [{ "equals": { "path": "/import" } }],
            "responses": [{ "_rift": { "script": { "engine": "rhai", "code": format!(
                "import {} as m; fn respond(ctx) {{ http(200, m::X) }}",
                js(&tree.path("outside/leak"))
            ) } } }]
        }),
    ]);
    config["port"] = port.into();
    let configfile = tree.root.join("imposters.json");
    std::fs::write(
        &configfile,
        serde_json::to_string(&json!({ "imposters": [config] })).expect("json"),
    )
    .expect("write configfile");
    let _server = Server::spawn(
        &tree,
        &[
            "--allow-injection",
            "--configfile",
            configfile.to_str().expect("utf-8"),
        ],
    )
    .await;

    // #305's users wrote absolute paths into their own config file; that keeps working.
    assert_eq!(get(port, "/require").await.2, "OK:\"CANARY_OUTSIDE_1611\"");
    let (status, _, body) = get(port, "/import").await;
    assert_ne!(status, 200, "Rhai import is off on every door: {body}");
    assert!(!body.contains("CANARY_RHAI_1611"), "{body}");
}

// A stub written over the admin API into a `--configfile` imposter makes the imposter the admin
// door's: the posted stub never runs with the config file's unconfined `require`.
#[tokio::test]
async fn an_admin_stub_write_confines_a_configfile_imposter() {
    let tree = Tree::new();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let mut config = imposter(vec![decorate_reporting(
        "/own",
        &tree.path("outside/mod.js"),
    )]);
    config["port"] = port.into();
    let configfile = tree.root.join("imposters.json");
    std::fs::write(
        &configfile,
        serde_json::to_string(&json!({ "imposters": [config] })).expect("json"),
    )
    .expect("write configfile");
    let server = Server::spawn(
        &tree,
        &[
            "--allow-injection",
            "--scripts-dir",
            &tree.sd(),
            "--configfile",
            configfile.to_str().expect("utf-8"),
        ],
    )
    .await;
    assert_eq!(get(port, "/own").await.2, "OK:\"CANARY_OUTSIDE_1611\"");

    let resp = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{}/imposters/{port}/stubs",
            server.admin
        ))
        .json(&json!({ "stub": decorate_reporting("/posted", &tree.path("outside/mod.js")) }))
        .send()
        .await
        .expect("add stub");
    assert_eq!(
        resp.status(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );

    for path in ["/posted", "/own"] {
        let (_, _, body) = get(port, path).await;
        assert_no_canary(path, &body);
        assert!(body.contains("outside --scripts-dir"), "{path}: {body}");
    }
}
