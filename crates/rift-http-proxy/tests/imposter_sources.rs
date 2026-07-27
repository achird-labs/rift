//! U-12 imposter sources: the `ImposterSource` SPI, the `file:`/`https:` built-ins, `--imposters`,
//! and re-fetch on `POST /admin/reload`.
//!
//! Every test here asserts an exact observable — a counter, a surviving request count, an error
//! naming both offenders — rather than "it didn't blow up". The `https:` tests run against a
//! purpose-built counting server so that "fetched once", "a 304 re-applies nothing" and the
//! hygiene limits are *measured*, not narrated.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};
use rift_http_proxy::sources::{
    FileSource, HttpSource, ImposterSource, SourceRef, SourceRegistry, SourceSet,
};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

// ===== A tiny counting HTTP/1.1 origin =====

/// What the test origin should answer with.
#[derive(Clone)]
enum Reply {
    /// 200 with this body and `ETag`; answers 304 when the request carries a matching
    /// `If-None-Match`.
    Etagged { body: String, etag: String },
    /// 302 to an arbitrary `Location` — used to prove the redirect scheme check.
    Redirect { location: String },
    /// A body of `size` bytes, to prove the size cap.
    Oversized { size: usize },
    /// Sleep before answering, to prove the timeout.
    Slow { delay: Duration },
}

struct Origin {
    port: u16,
    hits: Arc<AtomicUsize>,
    /// Requests that arrived carrying `If-None-Match`.
    conditional_hits: Arc<AtomicUsize>,
    body: Arc<std::sync::Mutex<Reply>>,
}

impl Origin {
    fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("origin binds");
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let conditional_hits = Arc::new(AtomicUsize::new(0));
        let body = Arc::new(std::sync::Mutex::new(reply));

        let (h, ch, b) = (hits.clone(), conditional_hits.clone(), body.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (h, ch, b) = (h.clone(), ch.clone(), b.clone());
                std::thread::spawn(move || {
                    let _ = serve_one(stream, &h, &ch, &b);
                });
            }
        });
        Self {
            port,
            hits,
            conditional_hits,
            body,
        }
    }

    fn uri(&self) -> String {
        format!("http://127.0.0.1:{}/imposters.json", self.port)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn conditional_hits(&self) -> usize {
        self.conditional_hits.load(Ordering::SeqCst)
    }

    fn set(&self, reply: Reply) {
        *self.body.lock().unwrap() = reply;
    }
}

/// Serve exactly one request, then let the caller drop the socket.
///
/// Every response carries `Connection: close` (issue #872). The server genuinely does close after
/// one request, so saying so is the correct behaviour rather than a workaround: HTTP/1.1 defaults
/// to keep-alive, so without it `reqwest` pools the socket, reuses it for the next fetch, and
/// races the close — which surfaced as an intermittent transport error under full-suite load, not
/// as the parse error the 304 test is designed to catch.
fn serve_one(
    mut stream: TcpStream,
    hits: &AtomicUsize,
    conditional_hits: &AtomicUsize,
    reply: &std::sync::Mutex<Reply>,
) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..n]).to_string();
    hits.fetch_add(1, Ordering::SeqCst);

    let if_none_match = request
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("if-none-match:"))
        .map(|l| l.split_once(':').unwrap().1.trim().to_string());
    if if_none_match.is_some() {
        conditional_hits.fetch_add(1, Ordering::SeqCst);
    }

    let reply = reply.lock().unwrap().clone();
    match reply {
        Reply::Etagged { body, etag } => {
            if if_none_match.as_deref() == Some(etag.as_str()) {
                write!(
                    stream,
                    "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nContent-Length: 0\r\n\
                     Connection: close\r\n\r\n"
                )?;
            } else {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nETag: {etag}\r\nContent-Type: application/json\r\n\
                     Connection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )?;
            }
        }
        Reply::Redirect { location } => {
            write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            )?;
        }
        Reply::Oversized { size } => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\
                 Content-Length: {size}\r\n\r\n"
            )?;
            // Written in chunks so the reader can bail part-way; a broken pipe here is the
            // expected outcome, not a failure.
            let chunk = vec![b'x'; 64 * 1024];
            let mut written = 0;
            while written < size {
                let take = chunk.len().min(size - written);
                if stream.write_all(&chunk[..take]).is_err() {
                    break;
                }
                written += take;
            }
        }
        Reply::Slow { delay } => {
            std::thread::sleep(delay);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\n[]"
            )?;
        }
    }
    stream.flush()
}

// ===== Helpers =====

fn imposter_doc(port: u16, body: &str) -> String {
    format!(
        r#"{{"imposters":[{{"port":{port},"protocol":"http","recordRequests":true,
            "stubs":[{{"responses":[{{"is":{{"statusCode":200,"body":"{body}"}}}}]}}]}}]}}"#
    )
}

fn set_of(refs: &[&str], no_parse: bool) -> SourceSet {
    let mut registry = SourceRegistry::new();
    registry
        .register(Arc::new(FileSource::new(no_parse)))
        .unwrap();
    registry
        .register(Arc::new(HttpSource::new().unwrap()))
        .unwrap();
    SourceSet::new(refs.iter().map(|u| SourceRef::new(*u)).collect(), registry)
}

fn cli(args: &[&str]) -> Cli {
    let mut argv = vec!["rift"];
    argv.extend_from_slice(args);
    Cli::try_parse_from(argv).expect("cli parse")
}

// ===== Issue #872: the origin must announce that it closes after one request =====

/// Read one raw HTTP response from the origin, sending `request_headers` verbatim.
fn raw_request(port: u16, extra_headers: &str) -> String {
    use std::io::{Read, Write};
    let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect to origin");
    write!(
        sock,
        "GET /imposters.json HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra_headers}\r\n"
    )
    .expect("write request");
    let mut out = String::new();
    let _ = sock.read_to_string(&mut out);
    out
}

/// `serve_one` handles exactly one request and drops the socket. HTTP/1.1 defaults to keep-alive,
/// so without an explicit `Connection: close` a pooling client (reqwest) reuses the dead socket on
/// the next fetch and racily fails with a transport error.
///
/// Asserted on the header rather than by making two fetches and hoping: the race is timing- and
/// load-dependent, so a two-fetch test passes almost every time even when the header is missing —
/// which is exactly how this shipped.
#[test]
fn origin_announces_connection_close_on_a_200() {
    let origin = Origin::start(Reply::Etagged {
        body: imposter_doc(14930, "x"),
        etag: "\"v1\"".to_string(),
    });
    let resp = raw_request(origin.port, "");
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp:.80}");
    assert!(
        resp.to_ascii_lowercase().contains("connection: close"),
        "a 200 must announce the close; headers were: {resp:.200}"
    );
}

#[test]
fn origin_announces_connection_close_on_a_304() {
    // The 304 path is the one the flake actually hit: it is the second fetch in
    // `http_source_304_serves_cache_without_reparsing`, i.e. the one served over a pooled socket.
    let origin = Origin::start(Reply::Etagged {
        body: imposter_doc(14931, "x"),
        etag: "\"v1\"".to_string(),
    });
    let resp = raw_request(origin.port, "If-None-Match: \"v1\"\r\n");
    assert!(resp.starts_with("HTTP/1.1 304"), "got: {resp:.80}");
    assert!(
        resp.to_ascii_lowercase().contains("connection: close"),
        "a 304 must announce the close; headers were: {resp:.200}"
    );
}

// ===== AC3: a port claimed by two sources names both =====

#[tokio::test]
async fn merge_collision_names_both_sources() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.json");
    let b = dir.path().join("b.json");
    std::fs::write(&a, imposter_doc(21401, "from-a")).unwrap();
    std::fs::write(&b, imposter_doc(21401, "from-b")).unwrap();

    let set = set_of(
        &[
            &format!("file:{}", a.display()),
            &format!("file:{}", b.display()),
        ],
        false,
    );
    let err = set
        .fetch_all()
        .await
        .map(|_| ())
        .expect_err("two sources claiming one port must be refused, not silently merged");
    let msg = err.to_string();

    // Both sources named, and the contested port — an operator must be able to fix this from the
    // message alone. Naming only one (the "loser") is the failure mode this asserts against.
    assert!(
        msg.contains(&a.display().to_string()),
        "the error must name the first source: {msg}"
    );
    assert!(
        msg.contains(&b.display().to_string()),
        "the error must name the second source: {msg}"
    );
    assert!(msg.contains("21401"), "the error must name the port: {msg}");
}

#[tokio::test]
async fn distinct_ports_across_sources_merge() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.json");
    let b = dir.path().join("b.json");
    std::fs::write(&a, imposter_doc(21402, "from-a")).unwrap();
    std::fs::write(&b, imposter_doc(21403, "from-b")).unwrap();

    let merged = set_of(
        &[
            &format!("file:{}", a.display()),
            &format!("file:{}", b.display()),
        ],
        false,
    )
    .fetch_all()
    .await
    .expect("distinct ports merge");
    let mut ports: Vec<u16> = merged.imposters.iter().filter_map(|c| c.port).collect();
    ports.sort_unstable();
    assert_eq!(ports, vec![21402, 21403]);
}

// ===== AC2: an ETag 304 re-parses nothing and re-applies nothing =====

#[tokio::test]
async fn http_source_304_serves_cache_without_reparsing() {
    let origin = Origin::start(Reply::Etagged {
        body: imposter_doc(21404, "v1"),
        etag: "\"v1\"".to_string(),
    });
    let source = HttpSource::new().unwrap();
    let r = SourceRef::new(origin.uri());

    let first = source.fetch(&r).await.expect("first fetch succeeds");
    assert!(!first.unchanged, "a first fetch is always a change");
    assert_eq!(first.configs.len(), 1);
    assert_eq!(first.meta.version.as_deref(), Some("\"v1\""));
    assert_eq!(origin.hits(), 1);
    assert_eq!(
        origin.conditional_hits(),
        0,
        "nothing to be conditional about yet"
    );

    // Swap the body for something that would FAIL to parse. A 304 must never reach the parser,
    // so this is the strongest available proof that the second fetch did not re-parse: if it
    // did, the test fails with a parse error instead of passing.
    origin.set(Reply::Etagged {
        body: "{ this is not valid json".to_string(),
        etag: "\"v1\"".to_string(),
    });

    let second = source.fetch(&r).await.expect("a 304 is not an error");
    assert!(second.unchanged, "a 304 must report unchanged");
    assert_eq!(
        second.configs.len(),
        1,
        "the cached configs are served verbatim"
    );
    assert_eq!(second.configs[0].port, Some(21404));
    assert_eq!(origin.hits(), 2);
    assert_eq!(
        origin.conditional_hits(),
        1,
        "the second request must carry If-None-Match, or the ETag is doing nothing"
    );
}

#[tokio::test]
async fn changed_etag_refetches_and_reports_a_change() {
    let origin = Origin::start(Reply::Etagged {
        body: imposter_doc(21405, "v1"),
        etag: "\"v1\"".to_string(),
    });
    let source = HttpSource::new().unwrap();
    let r = SourceRef::new(origin.uri());

    source.fetch(&r).await.expect("first fetch");
    origin.set(Reply::Etagged {
        body: imposter_doc(21406, "v2"),
        etag: "\"v2\"".to_string(),
    });

    let second = source.fetch(&r).await.expect("second fetch");
    assert!(!second.unchanged, "a new ETag is a change");
    assert_eq!(second.configs[0].port, Some(21406));
    assert_eq!(second.meta.version.as_deref(), Some("\"v2\""));
}

// ===== AC5: https hygiene — cap, timeout, redirect scheme =====

#[tokio::test]
async fn http_source_refuses_an_oversized_body() {
    let origin = Origin::start(Reply::Oversized {
        size: rift_http_proxy::sources::MAX_BODY_BYTES + 4096,
    });
    let err = HttpSource::new()
        .unwrap()
        .fetch(&SourceRef::new(origin.uri()))
        .await
        .expect_err("a body past the cap must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("limit") || msg.contains("exceeds"),
        "the error must say the cap was hit: {msg}"
    );
}

#[tokio::test]
async fn http_source_enforces_its_timeout() {
    let origin = Origin::start(Reply::Slow {
        delay: Duration::from_secs(3),
    });
    let source = HttpSource::with_timeout(Duration::from_millis(150)).unwrap();
    let started = std::time::Instant::now();
    let err = source
        .fetch(&SourceRef::new(origin.uri()))
        .await
        .expect_err("a stalled origin must not hang the fetch");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the timeout must fire well before the origin answers; took {:?}",
        started.elapsed()
    );
    let _ = err;
}

/// Behaviour pin, not a proof of our own line: mutation testing showed reqwest refuses a
/// `file://` redirect on its own, so disabling our scheme check leaves this test green. It stays
/// because the *behaviour* is what the security claim rests on, and a future change of HTTP
/// client would silently drop it. The redirect guarantee that is genuinely ours is the chain cap
/// below — a custom `redirect::Policy` replaces reqwest's default limit rather than adding to it.
#[tokio::test]
async fn http_source_refuses_a_redirect_to_a_non_http_scheme() {
    let origin = Origin::start(Reply::Redirect {
        location: "file:///etc/passwd".to_string(),
    });
    let err = HttpSource::new()
        .unwrap()
        .fetch(&SourceRef::new(origin.uri()))
        .await
        .expect_err("a redirect off http(s) must be refused, never followed");
    let msg = err.to_string();
    assert!(
        !msg.contains("root:"),
        "the target must never be read: {msg}"
    );
}

/// A custom redirect policy REPLACES reqwest's default 10-hop limit, so without an explicit cap
/// a self-referential redirect loops until the request timeout — burning the origin, and the
/// fetch, for the whole budget. Asserted on the origin's own hit count, which is exact.
#[tokio::test]
async fn http_source_caps_the_redirect_chain() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("origin binds");
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            h.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            // Always redirect back to ourselves: a loop that only a hop cap can stop.
            let _ = write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/loop\r\n\
                 Content-Length: 0\r\n\r\n"
            );
            let _ = stream.flush();
        }
    });

    let source = HttpSource::with_timeout(Duration::from_secs(5)).unwrap();
    let err = source
        .fetch(&SourceRef::new(format!("http://127.0.0.1:{port}/loop")))
        .await
        .map(|_| ())
        .expect_err("a redirect loop must terminate as an error");
    let _ = err;

    let seen = hits.load(Ordering::SeqCst);
    assert!(
        seen <= 12,
        "the chain must be capped at ~10 hops; the origin saw {seen} requests"
    );
}

// ===== D3: a remote document may not read local files =====

#[tokio::test]
async fn remote_document_refuses_ejs_include() {
    let origin = Origin::start(Reply::Etagged {
        body: r#"{"imposters":[<% include '/etc/passwd' %>]}"#.to_string(),
        etag: "\"inc\"".to_string(),
    });
    let err = HttpSource::new()
        .unwrap()
        .fetch(&SourceRef::new(origin.uri()))
        .await
        .expect_err("a remote document must not include a local file");
    let msg = err.to_string();
    assert!(
        msg.contains("include"),
        "the error must name the refused tag: {msg}"
    );
    assert!(!msg.contains("root:"), "the file must never be read: {msg}");
}

#[tokio::test]
async fn remote_document_refuses_ejs_stringify() {
    let origin = Origin::start(Reply::Etagged {
        body: r#"{"imposters":[{"port":21407,"protocol":"http","stubs":[{"responses":[{"inject":"<%- stringify('/etc/passwd') %>"}]}]}]}"#.to_string(),
        etag: "\"str\"".to_string(),
    });
    let err = HttpSource::new()
        .unwrap()
        .fetch(&SourceRef::new(origin.uri()))
        .await
        .expect_err("a remote document must not stringify a local file");
    assert!(
        err.to_string().contains("stringify"),
        "the error must name the refused tag: {err}"
    );
}

#[tokio::test]
async fn remote_document_refuses_a_file_script_reference() {
    let origin = Origin::start(Reply::Etagged {
        body: r#"{"imposters":[{"port":21408,"protocol":"http","stubs":[{"responses":[
            {"_rift":{"script":{"file":"/etc/passwd"}}}]}]}]}"#
            .to_string(),
        etag: "\"ref\"".to_string(),
    });
    let err = HttpSource::new()
        .unwrap()
        .fetch(&SourceRef::new(origin.uri()))
        .await
        .expect_err("a remote document must not resolve a local `file:` script");
    assert!(
        !err.to_string().contains("root:"),
        "the file must never be read: {err}"
    );
}

/// The chosen half of the split: env substitution still runs for a remote document, because it is
/// deployment configuration the operator supplied to their own process.
#[tokio::test]
async fn remote_document_still_substitutes_env() {
    unsafe { std::env::set_var("RIFT_TEST_U12_BODY", "from-env") };
    let origin = Origin::start(Reply::Etagged {
        body: imposter_doc(21409, "<%= process.env.RIFT_TEST_U12_BODY %>"),
        etag: "\"env\"".to_string(),
    });
    let fetched = HttpSource::new()
        .unwrap()
        .fetch(&SourceRef::new(origin.uri()))
        .await
        .expect("env substitution is honoured for remote documents");
    let json = serde_json::to_string(&fetched.configs[0]).unwrap();
    assert!(
        json.contains("from-env"),
        "the env var must have been substituted: {json}"
    );
    unsafe { std::env::remove_var("RIFT_TEST_U12_BODY") };
}

// ===== AC1: reload across two sources is incremental =====

/// The #316 composition claim, tested rather than asserted: changing one source must not reset
/// the imposters that came from the *other* source. Runtime state (recorded requests) is the
/// probe, because it is exactly what a delete-and-recreate would destroy.
#[tokio::test]
async fn source_reload_is_incremental_across_sources() {
    let dir = tempfile::tempdir().unwrap();
    let changing = dir.path().join("changing.json");
    let stable = dir.path().join("stable.json");
    std::fs::write(&changing, imposter_doc(21410, "v1")).unwrap();
    std::fs::write(&stable, imposter_doc(21411, "stable")).unwrap();

    let server = ServerBuilder::from_cli(cli(&[
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--imposters",
        &format!("file:{},file:{}", changing.display(), stable.display()),
    ]))
    .start()
    .await
    .expect("server starts from two file sources");
    let admin = server.admin_addr();
    let client = reqwest::Client::new();

    // Both sources loaded.
    for port in [21410u16, 21411] {
        let body = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect("imposter is serving")
            .text()
            .await
            .unwrap();
        assert!(!body.is_empty(), "imposter {port} answered");
    }

    // Give the stable imposter runtime state to lose.
    let recorded_before = requests_for(&client, admin.port(), 21411).await;
    assert_eq!(
        recorded_before, 1,
        "the probe request must have been recorded, or this test proves nothing"
    );

    // Change ONLY the first source.
    std::fs::write(&changing, imposter_doc(21410, "v2")).unwrap();
    let reload = client
        .post(format!("http://127.0.0.1:{}/admin/reload", admin.port()))
        .send()
        .await
        .expect("reload responds");
    assert!(
        reload.status().is_success(),
        "reload failed: {}",
        reload.text().await.unwrap_or_default()
    );

    // The changed source took effect...
    let changed = client
        .get("http://127.0.0.1:21410/")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(changed, "v2", "the changed source must have been applied");

    // ...and the untouched source's imposter kept its runtime state. A delete-and-recreate
    // reload — the mutant this test exists to catch — resets this to 0.
    let recorded_after = requests_for(&client, admin.port(), 21411).await;
    assert_eq!(
        recorded_after, recorded_before,
        "the sibling source's imposter must not have been recreated"
    );

    server.shutdown().await;
}

async fn requests_for(client: &reqwest::Client, admin_port: u16, imposter: u16) -> usize {
    let body: serde_json::Value = client
        .get(format!(
            "http://127.0.0.1:{admin_port}/imposters/{imposter}"
        ))
        .send()
        .await
        .expect("admin responds")
        .json()
        .await
        .expect("imposter body is json");
    body.get("requests")
        .and_then(|r| r.as_array())
        .map(Vec::len)
        .unwrap_or(0)
}

// ===== `--configfile` is sugar, not a second code path =====

#[tokio::test]
async fn configfile_and_imposters_are_mutually_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("c.json");
    std::fs::write(&f, imposter_doc(21412, "x")).unwrap();

    let err = ServerBuilder::from_cli(cli(&[
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--configfile",
        f.to_str().unwrap(),
        "--imposters",
        &format!("file:{}", f.display()),
    ]))
    .start()
    .await
    .map(|_| ())
    .expect_err("passing both spellings of one thing must be refused");
    assert!(
        err.to_string().contains("--imposters") && err.to_string().contains("--configfile"),
        "the error must name both flags: {err}"
    );
}

/// An embedder's source is reachable through the same dispatch as the built-ins, and a scheme it
/// shares with a built-in is a startup error rather than a silent shadow.
#[test]
fn registry_refuses_an_embedder_shadowing_a_builtin() {
    struct Shadow;
    impl ImposterSource for Shadow {
        fn schemes(&self) -> &'static [&'static str] {
            &["https"]
        }
        fn fetch<'a>(
            &'a self,
            _r: &'a SourceRef,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = anyhow::Result<rift_http_proxy::sources::FetchedImposters>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async { anyhow::bail!("unreachable") })
        }
    }

    let mut registry = SourceRegistry::new();
    registry
        .register(Arc::new(HttpSource::new().unwrap()))
        .unwrap();
    let err = registry
        .register(Arc::new(Shadow))
        .expect_err("shadowing a built-in scheme must be refused");
    assert!(
        err.to_string().contains("https"),
        "the error must name the contested scheme: {err}"
    );
}
