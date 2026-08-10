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
    /// Promise `promised` bytes, write only `sent`, then close — the client's body read fails
    /// mid-stream. Distinct from `Oversized`, which fails because the body is too *large*: this
    /// one fails in the transport, which is the path that loses the source URI (issue #953).
    Truncated { promised: usize, sent: usize },
    /// Write the headers and `sent` body bytes, then stall without closing. Unlike `Slow`, which
    /// stalls *before* the response exists and so times out in `send()`, this reaches the client's
    /// body loop first — the only way to exercise a body-phase timeout.
    StallMidBody { sent: usize, stall: Duration },
}

struct Origin {
    port: u16,
    hits: Arc<AtomicUsize>,
    /// Requests that arrived carrying `If-None-Match`.
    conditional_hits: Arc<AtomicUsize>,
    body: Arc<std::sync::Mutex<Reply>>,
    /// Errors the origin hit that are *not* a client hanging up on purpose (issue #935). Each
    /// connection is served on a detached thread, so a panic there would never reach the test
    /// harness — the fault has to be carried back here to be assertable.
    faults: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Origin {
    fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("origin binds");
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let conditional_hits = Arc::new(AtomicUsize::new(0));
        let body = Arc::new(std::sync::Mutex::new(reply));
        let faults = Arc::new(std::sync::Mutex::new(Vec::new()));

        let (h, ch, b, f) = (
            hits.clone(),
            conditional_hits.clone(),
            body.clone(),
            faults.clone(),
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (h, ch, b, f) = (h.clone(), ch.clone(), b.clone(), f.clone());
                std::thread::spawn(move || {
                    serve_one(stream, &h, &ch, &b, &f);
                });
            }
        });
        Self {
            port,
            hits,
            conditional_hits,
            body,
            faults,
        }
    }

    /// Origin-side failures that were not an expected client disconnect. Empty is the only
    /// healthy value; a test asserting on a counter should check this too, or it will read a
    /// server-side fault as a protocol result.
    fn faults(&self) -> Vec<String> {
        self.faults.lock().unwrap().clone()
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

/// Serve exactly one request, then let the socket drop — classifying anything that went wrong on
/// the way into "the client hung up, which some tests do on purpose" versus "a real fault the
/// suite must be able to see" (issue #935).
fn serve_one(
    mut stream: TcpStream,
    hits: &AtomicUsize,
    conditional_hits: &AtomicUsize,
    reply: &std::sync::Mutex<Reply>,
    faults: &std::sync::Mutex<Vec<String>>,
) {
    if let Err(e) = respond(&mut stream, hits, conditional_hits, reply) {
        // A client that hangs up part-way through is this fixture's contract, not a fault:
        // `Oversized` exists to make the reader bail mid-body, and `Slow` deliberately outlives
        // its client's timeout. Every *other* error is a real failure, and blanket-discarding the
        // lot is what let a failed read masquerade as a protocol result (issue #935). Recorded
        // here, while `stream` is still open, so the client cannot observe EOF and assert before
        // the fault lands.
        if !matches!(
            e.kind(),
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
        ) {
            faults.lock().unwrap().push(format!("{:?}: {e}", e.kind()));
        }
    }
}

/// Read one request and write its answer.
///
/// Every response carries `Connection: close` (issue #872). The server genuinely does close after
/// one request, so saying so is the correct behaviour rather than a workaround: HTTP/1.1 defaults
/// to keep-alive, so without it `reqwest` pools the socket, reuses it for the next fetch, and
/// races the close — which surfaced as an intermittent transport error under full-suite load, not
/// as the parse error the 304 test is designed to catch.
fn respond(
    stream: &mut TcpStream,
    hits: &AtomicUsize,
    conditional_hits: &AtomicUsize,
    reply: &std::sync::Mutex<Reply>,
) -> std::io::Result<()> {
    let request = read_request_head(stream)?;
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
            stream
                .write_all(etagged_response(&body, &etag, if_none_match.as_deref()).as_bytes())?;
        }
        Reply::Redirect { location } => {
            stream.write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
                     Connection: close\r\n\r\n"
                )
                .as_bytes(),
            )?;
        }
        Reply::Oversized { size } => {
            stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\
                     Content-Length: {size}\r\n\r\n"
                )
                .as_bytes(),
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
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\n[]",
            )?;
        }
        Reply::Truncated { promised, sent } => {
            stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\
                     Content-Length: {promised}\r\n\r\n"
                )
                .as_bytes(),
            )?;
            stream.write_all(&vec![b'x'; sent])?;
            // Returning `Ok` on a body we deliberately cut short is the point: the short write is
            // this fixture's contract, so it must not land in `faults` (issue #935).
        }
        Reply::StallMidBody { sent, stall } => {
            // One byte more than we will ever send, so the client stays in its body loop waiting
            // for a remainder that never arrives.
            stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\
                     Content-Length: {}\r\n\r\n",
                    sent + 1
                )
                .as_bytes(),
            )?;
            stream.write_all(&vec![b'x'; sent])?;
            stream.flush()?;
            std::thread::sleep(stall);
        }
    }
    stream.flush()
}

/// Hard cap on the request head the fixture will buffer — reads are clamped to what is left, so
/// `head` never exceeds it.
const MAX_REQUEST_HEAD: usize = 8192;

/// Bounds a peer that connects and then stalls. The old single `read` could only ever block once;
/// looping to the terminator makes an indefinite wedge reachable, so the wait is capped and the
/// timeout surfaces as a recorded fault rather than a hung suite.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Read the request head until its `\r\n\r\n` terminator rather than taking whatever one packet
/// happened to carry (issue #935). A single `read` was the root of the fixture's load-dependent
/// failures twice over: it could miss a trailing `If-None-Match` — so the origin answered 200
/// where the test expected 304 — and it left unread bytes in the receive queue, which makes the
/// OS answer the socket's close with RST instead of FIN and truncate the response the client was
/// still reading.
fn read_request_head(stream: &mut TcpStream) -> std::io::Result<String> {
    stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT))?;
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    // Rescans the whole buffer, so a terminator split across two reads is still found.
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        // Both exits below are errors rather than "serve what we have": every caller in this file
        // sends a complete, terminated head, so a short one is always anomalous. Answering it
        // anyway would parse no `If-None-Match` and serve 200 where the test expects 304 — the
        // very wrong-but-plausible answer #935 exists to stop.
        let room = MAX_REQUEST_HEAD - head.len();
        if room == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "request head reached {MAX_REQUEST_HEAD} bytes with no CRLFCRLF terminator"
                ),
            ));
        }
        let take = room.min(chunk.len());
        let n = stream.read(&mut chunk[..take])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "peer closed after {} byte(s), before the request head terminated",
                    head.len()
                ),
            ));
        }
        head.extend_from_slice(&chunk[..n]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// Render an `Etagged` reply into a single buffer (issue #935). Returning the whole response
/// rather than writing it piecemeal is what lets `serve_one` put it on the wire with one
/// `write_all`: a `write!` per format fragment is a syscall per fragment, and a peer that reads
/// between two of them sees a truncated response.
fn etagged_response(body: &str, etag: &str, if_none_match: Option<&str>) -> String {
    if if_none_match == Some(etag) {
        format!(
            "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nContent-Length: 0\r\n\
             Connection: close\r\n\r\n"
        )
    } else {
        format!(
            "HTTP/1.1 200 OK\r\nETag: {etag}\r\nContent-Type: application/json\r\n\
             Connection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }
}

/// Why a response read failed, in the terms that tell a truncated read apart from a genuine
/// protocol mismatch (issue #935): how much arrived, what it was, and the underlying error. The
/// partial is `{:?}`-escaped so its CR/LF cannot mangle the panic output it lands in.
fn read_failure_message(read: usize, partial: &str, error: &std::io::Error) -> String {
    format!(
        "reading the origin's response failed after {read} byte(s): {error}; \
         partial response: {partial:?}"
    )
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
    let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect to origin");
    // One `write_all` rather than a `write!` per format fragment (issue #935): each fragment is
    // its own syscall, and a request that reaches the origin in pieces is exactly what let it
    // answer before the whole head had arrived.
    let request = format!("GET /imposters.json HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra_headers}\r\n");
    sock.write_all(request.as_bytes()).expect("write request");
    read_response(&mut sock)
}

/// Read the whole response, failing loudly if the socket dies part-way (issue #935). The discarded
/// error this replaces turned a truncated read into a silent wrong answer: the assertion compared
/// against a partial response and reported it as a protocol regression, which is how the same
/// failure got misdiagnosed twice.
fn read_response(sock: &mut impl Read) -> String {
    let mut out = String::new();
    if let Err(e) = sock.read_to_string(&mut out) {
        panic!("{}", read_failure_message(out.len(), &out, &e));
    }
    out
}

// ===== Issue #935: the fixture must survive fragmented I/O, and say so when it doesn't =====

/// AC2 — the regression gate for the actual flake. `write!` on an unbuffered `TcpStream` emits one
/// syscall per format fragment, so under full-workspace load the origin's request read used to
/// return before `If-None-Match` arrived: it answered 200, and the 304 test reported that as a
/// protocol regression. Splitting the request explicitly makes that race deterministic — this
/// fails against a single-`read` `serve_one` every time, rather than once in a hundred runs.
///
/// It also covers the second half of the chain: leaving request bytes unread is what made the OS
/// send RST instead of FIN and truncate the response mid-header.
#[test]
fn serve_one_reads_a_request_split_across_delayed_writes() {
    let origin = Origin::start(Reply::Etagged {
        body: imposter_doc(14932, "x"),
        etag: "\"v1\"".to_string(),
    });

    let mut sock = TcpStream::connect(("127.0.0.1", origin.port)).expect("connect to origin");
    sock.write_all(b"GET /imposters.json HTTP/1.1\r\nHost: 127.0.0.1\r\n")
        .expect("write the first fragment");
    sock.flush().expect("flush the first fragment");
    // Wider than any scheduler gap the real flake needed, so the origin is guaranteed to have
    // woken on a partial head if it only reads once.
    std::thread::sleep(Duration::from_millis(50));
    sock.write_all(b"If-None-Match: \"v1\"\r\n\r\n")
        .expect("write the trailing fragment");
    sock.flush().expect("flush the trailing fragment");

    let mut out = String::new();
    sock.read_to_string(&mut out).expect("read the response");
    assert!(
        out.starts_with("HTTP/1.1 304"),
        "the origin must wait for the whole request head before answering; got: {out:.120}"
    );
    assert_eq!(
        origin.conditional_hits(),
        1,
        "the If-None-Match arriving in a later packet must still be seen"
    );
    assert!(
        origin.faults().is_empty(),
        "a fragmented request is normal traffic, not an origin fault: {:?}",
        origin.faults()
    );
}

/// Delivers `pending`, then fails the way a peer's RST does. `read_to_string` keeps the bytes it
/// already decoded and returns the error, so this reproduces the exact state the old `let _ =`
/// discarded — deterministically, without having to win a race against a real socket.
struct ResetAfter {
    pending: &'static [u8],
}

impl Read for ResetAfter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ));
        }
        let n = self.pending.len().min(buf.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending = &self.pending[n..];
        Ok(n)
    }
}

/// The `n == 0` guard in `read_request_head` is load-bearing twice over: without it a peer that
/// closes mid-head spins the loop forever (`read` keeps returning `Ok(0)`, and a read *timeout*
/// never fires on a repeating instant-EOF), and before it the fixture answered the truncated head
/// anyway — parsing no `If-None-Match` and serving 200 where the caller expected 304. This pins
/// the branch, and with it that a genuine origin fault reaches the test rather than dying in the
/// detached thread that produced it.
#[test]
fn a_request_head_that_never_terminates_is_recorded_as_a_fault() {
    let origin = Origin::start(Reply::Etagged {
        body: imposter_doc(14935, "x"),
        etag: "\"v1\"".to_string(),
    });

    {
        let mut sock = TcpStream::connect(("127.0.0.1", origin.port)).expect("connect to origin");
        sock.write_all(b"GET /imposters.json HTTP/1.1\r\nHost: 127.0.0.1\r\n")
            .expect("write a head with no terminator");
        sock.flush().expect("flush the partial head");
    } // dropped without the terminating CRLFCRLF, so the origin reads EOF mid-head

    // Nothing is left for this client to read, so there is no EOF to synchronise on the way the
    // other tests do — poll the sink instead, bounded so a regression fails rather than hangs.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while origin.faults().is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let faults = origin.faults();
    assert_eq!(
        faults.len(),
        1,
        "the unterminated head is recorded exactly once: {faults:?}"
    );
    assert!(
        faults[0].contains("UnexpectedEof"),
        "classified as an early close: {faults:?}"
    );
    assert!(
        faults[0].contains("before the request head terminated"),
        "and says what was incomplete: {faults:?}"
    );
    assert_eq!(
        origin.hits(),
        0,
        "a request whose head never arrived is not a served request"
    );
}

/// AC1 — a truncated read must fail, naming all three facts that separate "the socket died
/// mid-response" from "the origin sent the wrong thing". This is the gate on the *behaviour*: a
/// `read_response` that goes back to discarding the error returns the partial and fails here.
/// (The panic it provokes is expected and prints to stderr during a passing run.)
#[test]
fn read_response_panics_instead_of_returning_a_truncated_response() {
    const PARTIAL: &str = "HTTP/1.1 200 OK\r\nETag: ";
    let panicked = std::panic::catch_unwind(|| {
        read_response(&mut ResetAfter {
            pending: PARTIAL.as_bytes(),
        })
    })
    .expect_err("a reset mid-response must panic, not hand back the partial read");

    let msg = panicked
        .downcast_ref::<String>()
        .expect("the panic payload is the formatted failure message");
    assert!(
        msg.contains(&format!("{} byte", PARTIAL.len())),
        "names how much arrived: {msg}"
    );
    assert!(
        msg.contains("connection reset by peer"),
        "names the underlying error: {msg}"
    );
    assert!(
        msg.contains(r#""HTTP/1.1 200 OK\r\nETag: ""#),
        "shows the partial response with escapes, so CR/LF cannot mangle the panic output: {msg}"
    );
}

/// AC3 — rendering each response into one buffer is what makes "a single `write_all`" true by
/// construction; testing the renderer directly also pins the bytes the 200/304 tests depend on.
#[test]
fn etagged_response_answers_304_only_on_a_matching_if_none_match() {
    let body = imposter_doc(14933, "x");
    let matched = etagged_response(&body, "\"v1\"", Some("\"v1\""));
    assert!(matched.starts_with("HTTP/1.1 304 Not Modified\r\n"));
    assert!(matched.contains("Content-Length: 0\r\n"));
    assert!(matched.ends_with("\r\n\r\n"), "a 304 carries no body");

    for stale in [Some("\"v0\""), None] {
        let full = etagged_response(&body, "\"v1\"", stale);
        assert!(
            full.starts_with("HTTP/1.1 200 OK\r\n"),
            "If-None-Match {stale:?} must not match"
        );
        assert!(full.ends_with(&body), "the 200 carries the body");
        assert!(
            full.contains(&format!("Content-Length: {}\r\n", body.len())),
            "content-length counts the body bytes"
        );
    }
}

/// Every response the fixture can send announces the close it actually performs (issue #872).
/// Regression-guards that the #935 rewrite kept that header on both branches.
#[test]
fn etagged_response_always_announces_connection_close() {
    let body = imposter_doc(14934, "x");
    for if_none_match in [Some("\"v1\""), None] {
        let response = etagged_response(&body, "\"v1\"", if_none_match);
        assert!(
            response
                .to_ascii_lowercase()
                .contains("connection: close\r\n"),
            "every branch announces the close: {response:.80}"
        );
    }
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
    // `{err:#}` rather than `to_string()`: the cap message is outermost only because it rides a
    // bare `?` today. Adding a context wrap on that path would otherwise gut this silently.
    let msg = format!("{err:#}");
    assert!(
        msg.contains("limit") || msg.contains("exceeds"),
        "the error must say the cap was hit: {msg}"
    );
    // The cap branch embeds the URI itself, so it is the other path a call-site wrap would make
    // say "imposter source" twice (issue #953).
    assert_eq!(
        msg.matches("imposter source").count(),
        1,
        "the source must be named exactly once, not once per wrap: {msg}"
    );
}

/// The third way `read_capped` can fail, and the last one that was still anonymous: the body dies
/// mid-stream. The size-cap and non-UTF-8 branches embed the URI in their own messages, but a
/// transport failure rode a bare `?`, so the operator saw `error decoding response body` with
/// nothing to say *which* `--imposters` source had died (issue #953).
///
/// Both halves are asserted because they are separate regressions, exactly as in
/// `http_source_error_keeps_the_reqwest_cause_in_its_chain`: the message half fails if the context
/// wrap is dropped, and the chain half fails if someone re-adds it as `anyhow!("…: {e}")`, which
/// reads identically but severs `source()` and takes `is_timeout()` with it.
#[tokio::test]
async fn body_read_failure_names_the_source() {
    let origin = Origin::start(Reply::Truncated {
        promised: 4096,
        sent: 16,
    });
    let err = HttpSource::new()
        .unwrap()
        .fetch(&SourceRef::new(origin.uri()))
        .await
        .expect_err("a body that ends mid-stream must fail the fetch");

    let msg = format!("{err:#}");
    assert!(
        msg.contains(&format!("reading imposter source {}", origin.uri())),
        "the failure must name the source that died: {msg}"
    );
    // Exactly once, and counted on the phrase rather than on the URI: what this pins is that our
    // own wraps name the source once, and reqwest is free to put the URL in its own message text
    // without that being a fault of ours. A `with_context` added at `read_capped`'s call site —
    // the change this design deliberately does not make — would say "imposter source" twice here
    // and still satisfy a `contains` check.
    assert_eq!(
        msg.matches("imposter source").count(),
        1,
        "the source must be named exactly once, not once per wrap: {msg}"
    );
    assert!(
        err.chain()
            .find_map(|c| c.downcast_ref::<reqwest::Error>())
            .is_some(),
        "the transport error must survive as a link in the chain, not be flattened into text \
         (issue #951): {msg}"
    );
    let faults = origin.faults();
    assert!(
        faults.is_empty(),
        "cutting the body short is this fixture's contract, not an origin fault: {faults:?}"
    );
}

/// The body-phase half of the timeout guarantee, and the reason the issue #953 wrap uses
/// `.with_context(…)` rather than `anyhow!("…: {e}")`.
///
/// `http_source_enforces_its_timeout` below cannot prove this: `Reply::Slow` sleeps *before*
/// writing anything, so its timeout fires inside `send()` and never reaches the body loop — which
/// left the newly wrapped line's stated rationale resting on the comment alone. Here the headers
/// arrive and the body then stalls, so the timeout fires under the new wrap, where a severed
/// chain would leave `is_timeout()` with nothing to find.
#[tokio::test]
async fn a_body_phase_timeout_stays_recognisable_through_the_context_wrap() {
    let origin = Origin::start(Reply::StallMidBody {
        sent: 16,
        stall: Duration::from_secs(3),
    });
    let started = std::time::Instant::now();
    let err = HttpSource::with_timeout(Duration::from_millis(150))
        .unwrap()
        .fetch(&SourceRef::new(origin.uri()))
        .await
        .expect_err("a body that never finishes must not hang the fetch");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "the timeout must fire while the body is stalled; took {elapsed:?}"
    );

    let transport = err
        .chain()
        .find_map(|c| c.downcast_ref::<reqwest::Error>())
        .expect("the transport error must survive the context wrap (issue #951)");
    assert!(
        transport.is_timeout(),
        "a stalled body must report as a timeout, not some other transport error: {err:#}"
    );
    assert!(
        format!("{err:#}").contains(&format!("reading imposter source {}", origin.uri())),
        "and it must still name the source it stalled on: {err:#}"
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
    // The elapsed check alone proves only that the fetch failed *fast* — a refused connection or a
    // reset origin satisfies it just as well as the timeout under test, which is what discarding
    // the error with `let _ = err;` let through (issue #950).
    //
    // Asked of the error structurally rather than by message: `{err:#}` could tell the two apart,
    // since `TimedOut` renders as "operation timed out" in the chain, but that would pin reqwest's
    // wording where what matters is its classification. `is_timeout` walks the chain for the marker
    // whichever phase planted it — connect, headers or body. The chain reaches us at all only
    // because issue #951 stopped the wrap from severing it, which is why the two failures below
    // are separate assertions: they are separate regressions.
    let transport = err
        .chain()
        .find_map(|c| c.downcast_ref::<reqwest::Error>())
        .expect("the transport error must survive as a link in the chain (issue #951)");
    assert!(
        transport.is_timeout(),
        "the fetch must have failed on the timeout, not some other transport error: {err:#}"
    );
}

/// Behaviour pin, not a proof of our own line: mutation testing showed reqwest refuses a
/// `file://` redirect on its own, so disabling our scheme check leaves this test green. It stays
/// because the *behaviour* is what the security claim rests on, and a future change of HTTP
/// client would silently drop it. The redirect guarantee that is genuinely ours is the chain cap
/// below — a custom `redirect::Policy` replaces reqwest's default limit rather than adding to it.
///
/// The assertion here stays negative because `file:` specifically never reaches our policy at
/// all: reqwest neither follows nor errors on it, handing the 302 back as an ordinary response, so
/// what the caller sees is our own `returned HTTP 302 Found` status check — a one-link chain with
/// no `reqwest::Error` in it. That is peculiar to `file:`, not to blocked schemes in general;
/// `redirect_to_a_blocked_scheme_names_our_own_refusal` below covers the reachable case and is
/// what actually proves the scheme check does something.
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
    let msg = format!("{err:#}");
    assert!(
        !msg.contains("root:"),
        "the target must never be read: {msg}"
    );
}

/// The scheme check's first actual proof of effect. Its long-standing test above can only assert
/// a negative, because `file:` never reaches our policy — which left the line itself unverified,
/// as that test's own comment concedes ("disabling our scheme check leaves this test green"). A
/// scheme reqwest *does* treat as a followable redirect target reaches the policy, so our refusal
/// runs and, since issue #951, its message survives to the caller: this fails both if the check is
/// removed and if the chain is severed again.
#[tokio::test]
async fn redirect_to_a_blocked_scheme_names_our_own_refusal() {
    let origin = Origin::start(Reply::Redirect {
        location: "ftp://127.0.0.1:1/x".to_string(),
    });
    let err = HttpSource::new()
        .unwrap()
        .fetch(&SourceRef::new(origin.uri()))
        .await
        .expect_err("a redirect off http(s) must be refused, never followed");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("refusing to follow a redirect to a non-http(s) URL: ftp://127.0.0.1:1/x"),
        "our own refusal, naming the rejected target, must reach the caller: {msg}"
    );
}

/// The structural half of issue #951: the `reqwest::Error` must still be *in* the chain, not just
/// its text in a string. `anyhow!("…: {e}")` produced a fresh error with no source, so no caller
/// could ever downcast to ask reqwest a question about the failure — which is precisely what
/// distinguishing a timeout from a reset requires (`reqwest::Error::is_timeout` walks the chain).
///
/// Asserted on the hop cap rather than the timeout: the timeout assertion belongs to the test
/// #950 fixes, and proving the chain survives does not depend on which kind of failure it is.
#[tokio::test]
async fn http_source_error_keeps_the_reqwest_cause_in_its_chain() {
    let origin = Origin::start(Reply::Redirect {
        location: String::new(),
    });
    origin.set(Reply::Redirect {
        location: format!("http://127.0.0.1:{}/loop", origin.port),
    });
    let err = HttpSource::with_timeout(Duration::from_secs(5))
        .unwrap()
        .fetch(&SourceRef::new(format!(
            "http://127.0.0.1:{}/loop",
            origin.port
        )))
        .await
        .expect_err("a redirect loop must terminate as an error");

    let reqwest_cause = err
        .chain()
        .find_map(|c| c.downcast_ref::<reqwest::Error>())
        .expect("the reqwest error must survive as a link in the chain, not be flattened to text");
    assert!(
        reqwest_cause.is_redirect(),
        "the surviving cause must be the redirect failure itself: {reqwest_cause}"
    );
}

/// A custom redirect policy REPLACES reqwest's default 10-hop limit, so without an explicit cap
/// a self-referential redirect loops until the request timeout — burning the origin, and the
/// fetch, for the whole budget. Asserted on the origin's own hit count, which is exact.
///
/// Served by the shared [`Origin`] rather than a hand-rolled listener (issue #939): the inline
/// one read the head with a single fixed-buffer `read`, answered with one `write!` whose three
/// format fragments each became their own write, and dropped the socket with request bytes still
/// queued — which makes the OS answer with RST instead of FIN and truncate the response
/// mid-header. `Origin` reads to the CRLFCRLF terminator, answers with one `write_all`, and
/// records the errors it hits.
#[tokio::test]
async fn http_source_caps_the_redirect_chain() {
    // The Location has to name the port, which only exists once the listener is bound — so the
    // self-referential loop, the one thing only a hop cap can stop, is set after start.
    let origin = Origin::start(Reply::Redirect {
        location: String::new(),
    });
    let uri = format!("http://127.0.0.1:{}/loop", origin.port);
    origin.set(Reply::Redirect {
        location: uri.clone(),
    });

    let source = HttpSource::with_timeout(Duration::from_secs(5)).unwrap();
    let err = source
        .fetch(&SourceRef::new(uri))
        .await
        .map(|_| ())
        .expect_err("a redirect loop must terminate as an error");

    // `{err:#}` renders the whole chain (issue #951); `to_string()` would show only the outermost
    // context and hide the cause the check below is now precise enough to demand.
    if let Err(why) =
        redirect_chain_hit_the_cap(origin.hits(), &origin.faults(), &format!("{err:#}"))
    {
        panic!("the redirect cap was not what stopped this chain: {why}");
    }
}

// ===== Issue #939: the cap assertion must not pass for the wrong reason =====

/// Hops the origin must serve before the fetch gives up, mirroring the `attempt.previous().len()
/// >= 10` guard in `sources.rs`.
///
/// The arithmetic is not the obvious one, which is why it is written down: reqwest pushes the
/// current URL into `previous` *before* consulting the policy, so on the Nth response
/// `previous().len() == N` — responses received so far, not hops taken before this one. Reading it
/// the natural way gives 11 and makes this constant look off by one. It also means a reqwest
/// upgrade that moves that push is what breaks the exact-equality check below.
const REDIRECT_HOP_CAP: usize = 10;

/// Decide whether a redirect-loop run actually proved the hop cap (issue #939).
///
/// Factored out of the test so it can be shown to *reject* the wrong reasons for a short chain.
/// `seen <= 12` on its own is a green light for a chain a transport error cut off at hop two: the
/// fetch still fails, the count is still under the bound, and the assertion proves nothing. All
/// three conditions have to hold — the origin hit no fault of its own, the fetch failed *because
/// of* a redirect rather than because the socket or the clock gave out first, and the origin
/// served exactly the capped number of hops.
///
/// The middle check names the cap itself. It used to settle for "a redirect failure", because
/// `sources.rs` formatted the reqwest error with `{e}` and reqwest's `Display` emits
/// `error following redirect for url (…)` without the `too many redirects` source underneath it —
/// so the scheme-refusal path was indistinguishable from the cap here, and only the exact hop
/// count pinned *which* redirect failure this was. Issue #951 preserved the chain, so the caller
/// now sees the message our own policy wrote and the check can demand it.
fn redirect_chain_hit_the_cap(seen: usize, faults: &[String], error: &str) -> Result<(), String> {
    if !faults.is_empty() {
        return Err(format!("the origin recorded faults of its own: {faults:?}"));
    }
    if !error.to_ascii_lowercase().contains("too many redirects") {
        return Err(format!(
            "the fetch failed for some reason other than the redirect cap: {error}"
        ));
    }
    if seen != REDIRECT_HOP_CAP {
        return Err(format!(
            "the origin served {seen} hop(s), not the {REDIRECT_HOP_CAP} the cap allows"
        ));
    }
    Ok(())
}

/// What a fetch that died on the hop cap actually reports, verbatim — the full chain as `{err:#}`
/// renders it since issue #951, ending in the message our own redirect policy wrote. Shared by
/// every case below that has to look like a genuine capped run, so those cases differ only in the
/// one input each is about.
const REDIRECT_FAILURE: &str = "fetching imposter source http://127.0.0.1:1/loop: error following redirect for url \
     (http://127.0.0.1:1/loop): too many redirects";

#[test]
fn redirect_cap_check_accepts_a_chain_stopped_by_the_hop_cap() {
    assert_eq!(
        redirect_chain_hit_the_cap(REDIRECT_HOP_CAP, &[], REDIRECT_FAILURE),
        Ok(())
    );
}

/// A redirect failure that is *not* the cap must still be rejected, independently of the hop
/// count — which is why the count passed here is the capped one rather than something already
/// wrong. This is the case the old `contains("following redirect")` check waved through: reqwest
/// words every redirect failure identically at the top level, so matching on that wording alone
/// accepted any of them. The input below is the real thing, not a hypothetical — it is the chain
/// `redirect_to_a_blocked_scheme_names_our_own_refusal` produces.
#[test]
fn redirect_cap_check_rejects_a_blocked_scheme_redirect() {
    let rejection = redirect_chain_hit_the_cap(
        REDIRECT_HOP_CAP,
        &[],
        "fetching imposter source http://127.0.0.1:1/loop: error following redirect for url \
         (http://127.0.0.1:1/loop): refusing to follow a redirect to a non-http(s) URL: \
         file:///etc/passwd",
    )
    .expect_err("a scheme refusal is not the hop cap, however similar reqwest's wording is");
    assert!(
        rejection.contains("refusing to follow"),
        "the rejection must quote the cause it rejected: {rejection}"
    );
}

/// The regression this issue is about: an origin whose socket dies mid-response cuts the chain
/// off early, and a `seen <= 12` assertion calls that a proven cap.
#[test]
fn redirect_cap_check_rejects_a_chain_cut_short_by_a_transport_error() {
    let rejection = redirect_chain_hit_the_cap(
        2,
        &[],
        "fetching imposter source http://127.0.0.1:1/loop: error sending request for url \
         (http://127.0.0.1:1/loop)",
    )
    .expect_err("a 2-hop chain must not count as a capped one");
    assert!(
        rejection.contains("error sending request"),
        "the rejection must quote the transport error it rejected: {rejection}"
    );
}

/// The nastiest shape, and the one the other cases short-circuit past: a chain that ended early
/// while still *looking* like an ordinary redirect failure — no origin-side fault, and an error
/// worded exactly like the capped run's. Only the hop count separates the two, so this is the
/// case that proves the count is doing real work rather than riding along behind the other two
/// checks.
#[test]
fn redirect_cap_check_rejects_a_short_chain_that_still_failed_on_a_redirect() {
    let rejection = redirect_chain_hit_the_cap(3, &[], REDIRECT_FAILURE)
        .expect_err("a 3-hop chain must not count as a capped one");
    assert!(
        rejection.contains("served 3 hop(s)"),
        "the rejection must name the hop count it saw: {rejection}"
    );
    assert!(
        rejection.contains(&REDIRECT_HOP_CAP.to_string()),
        "the rejection must name the cap it expected: {rejection}"
    );
}

#[test]
fn redirect_cap_check_rejects_an_origin_that_recorded_a_fault() {
    let rejection = redirect_chain_hit_the_cap(
        REDIRECT_HOP_CAP,
        &["InvalidData: request head reached 8192 bytes".to_string()],
        REDIRECT_FAILURE,
    )
    .expect_err("an origin-side fault must not be masked by a hop count that happens to match");
    assert!(
        rejection.contains("InvalidData"),
        "the rejection must carry the fault it saw: {rejection}"
    );
}

#[test]
fn redirect_cap_check_rejects_a_failure_that_was_not_the_redirect_chain() {
    let rejection = redirect_chain_hit_the_cap(
        REDIRECT_HOP_CAP,
        &[],
        "fetching imposter source http://127.0.0.1:1/loop: operation timed out",
    )
    .expect_err("the fetch must have failed because of the chain, not the clock");
    assert!(
        rejection.contains("timed out"),
        "the rejection must quote the error it rejected: {rejection}"
    );
}

/// The direction the original assertion did cover, kept: an uncapped policy loops past the cap.
#[test]
fn redirect_cap_check_rejects_an_uncapped_chain() {
    let rejection = redirect_chain_hit_the_cap(REDIRECT_HOP_CAP + 5, &[], REDIRECT_FAILURE)
        .expect_err("a chain that ran past the cap must fail");
    assert!(
        rejection.contains(&REDIRECT_HOP_CAP.to_string()),
        "the rejection must name the cap it expected: {rejection}"
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
    // `{err:#}`, not `to_string()`: since issue #951 the refusal is a cause under the
    // "imposter source <uri>" context rather than being flattened into it.
    let msg = format!("{err:#}");
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
    // `{err:#}`, not `to_string()`: since issue #951 the refusal is a cause under the
    // "imposter source <uri>" context rather than being flattened into it.
    let msg = format!("{err:#}");
    assert!(
        msg.contains("stringify"),
        "the error must name the refused tag: {msg}"
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
    // `{err:#}`, not `to_string()`: since issue #951 the refusal is a cause under the
    // "imposter source <uri>" context. Rendering only the context would make this assertion
    // unfalsifiable — the file's contents could not appear there however badly the refusal broke.
    let msg = format!("{err:#}");
    assert!(!msg.contains("root:"), "the file must never be read: {msg}");
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

/// The operator-facing half of issue #951. A reload failure is the one place a source fetch error
/// reaches a human, and `POST /admin/reload` answered with only reqwest's generic top line — so a
/// hop-cap trip and a blocked-scheme redirect produced byte-identical 500s and neither named a
/// cause anyone could act on. The handler renders the chain now, so the reason our own policy
/// wrote arrives with the failure.
#[tokio::test]
async fn reload_failure_body_carries_the_fetch_cause() {
    let origin = Origin::start(Reply::Etagged {
        body: imposter_doc(21419, "v1"),
        etag: "\"v1\"".to_string(),
    });
    let server = ServerBuilder::from_cli(cli(&[
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--imposters",
        &origin.uri(),
    ]))
    .start()
    .await
    .expect("server starts from an http source");
    let admin = server.admin_addr();

    // Flip the same source into a self-referential loop, so the re-fetch fails on the hop cap.
    origin.set(Reply::Redirect {
        location: origin.uri(),
    });
    let reload = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}/admin/reload", admin.port()))
        .send()
        .await
        .expect("reload responds");
    assert_eq!(
        reload.status(),
        500,
        "a source that cannot be fetched must fail the reload"
    );

    let body = reload.text().await.unwrap_or_default();
    assert!(
        body.contains("too many redirects"),
        "the reload failure must name the cause, not just reqwest's generic wording: {body}"
    );
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
