//! Network utilities for the proxy server.
//!
//! This module provides network-related functionality including
//! creating TCP listeners with SO_REUSEPORT for multi-worker setups and
//! applying per-connection socket tuning (TCP_NODELAY) to accepted streams.

use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

/// Default listen backlog; overridable via `RIFT_TCP_BACKLOG`.
const DEFAULT_BACKLOG: i32 = 1024;

/// Default `hyper`/`hyper-util` connection buffer cap; overridable via `RIFT_HTTP_MAX_BUF`.
/// Hyper's own default is ~400KB, sized for arbitrary internet traffic; mock/proxy requests are
/// small and numerous, so a 64KB ceiling bounds per-connection memory without touching normal
/// traffic.
pub const DEFAULT_HTTP_MAX_BUF: usize = 64 * 1024;

/// Default HTTP/1 header-read timeout (slowloris hygiene); overridable via
/// `RIFT_HTTP_HEADER_TIMEOUT` (seconds).
const DEFAULT_HTTP_HEADER_TIMEOUT: Duration = Duration::from_secs(30);

/// `hyper`'s h1 buffer floor (`proto::h1::MINIMUM_MAX_BUFFER_SIZE`, currently 8KB). It isn't part
/// of hyper's public API, so it's mirrored here as a guard: `http1::Builder::max_buf_size` (and
/// the `auto::Builder` equivalent) panics if handed anything smaller. Enforced only in `parse` so
/// a bogus/too-small env value degrades to the default instead of crashing the server.
const HYPER_H1_MIN_MAX_BUF: usize = 8192;

/// HTTP connection-builder tuning knobs (issue #716): the hyper/hyper-util `serve_connection`
/// builders otherwise run on hyper's own defaults (a ~400KB buffer, no header-read timeout, and
/// no cap on concurrent connections).
///
/// Populated from the environment ([`HttpTuning::from_env`]) once per listener setup, mirroring
/// [`SocketTuning`] — not read per accepted connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpTuning {
    /// Cap on the connection's internal read/write buffer (`Builder::max_buf_size`).
    pub max_buf_size: usize,
    /// How long to wait for a client to finish sending request headers before closing the
    /// connection (`Builder::header_read_timeout`) — mitigates slowloris-style stalls.
    pub header_read_timeout: Duration,
    /// Cap on concurrently accepted connections. `None` (the default) preserves today's
    /// behavior: unlimited, accept-as-fast-as-the-kernel-hands-them-over.
    pub max_connections: Option<usize>,
}

impl Default for HttpTuning {
    fn default() -> Self {
        Self {
            max_buf_size: DEFAULT_HTTP_MAX_BUF,
            header_read_timeout: DEFAULT_HTTP_HEADER_TIMEOUT,
            max_connections: None,
        }
    }
}

impl HttpTuning {
    /// Read tuning from the environment: `RIFT_HTTP_MAX_BUF` (bytes), `RIFT_HTTP_HEADER_TIMEOUT`
    /// (seconds), and `RIFT_MAX_CONNECTIONS` (connection count, opt-in). Unset or unparsable
    /// values fall back to [`HttpTuning::default`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::parse(
            std::env::var("RIFT_HTTP_MAX_BUF").ok().as_deref(),
            std::env::var("RIFT_HTTP_HEADER_TIMEOUT").ok().as_deref(),
            std::env::var("RIFT_MAX_CONNECTIONS").ok().as_deref(),
        )
    }

    /// Pure parser behind [`HttpTuning::from_env`] — kept env-free so it is testable without
    /// mutating process-global state.
    fn parse(
        max_buf: Option<&str>,
        header_timeout_secs: Option<&str>,
        max_conns: Option<&str>,
    ) -> Self {
        let defaults = Self::default();
        let max_buf_size = max_buf
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&v| v >= HYPER_H1_MIN_MAX_BUF)
            .unwrap_or(defaults.max_buf_size);
        let header_read_timeout = header_timeout_secs
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&secs| secs > 0)
            .map(Duration::from_secs)
            .unwrap_or(defaults.header_read_timeout);
        // Opt-in: unset, zero, negative, or garbage all mean "unlimited" rather than an error —
        // this knob defaults to today's behavior, so a malformed value should not fail closed.
        let max_connections = max_conns
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&n| n > 0);
        Self {
            max_buf_size,
            header_read_timeout,
            max_connections,
        }
    }
}

/// Accept-loop / socket tuning knobs.
///
/// Populated from the environment ([`SocketTuning::from_env`]) at listener
/// setup so the values are read once, not per accepted connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketTuning {
    /// `listen(2)` backlog for the accept queue.
    pub backlog: i32,
    /// Whether to set `TCP_NODELAY` on accepted streams (disable Nagle).
    pub nodelay: bool,
}

impl Default for SocketTuning {
    fn default() -> Self {
        Self {
            backlog: DEFAULT_BACKLOG,
            nodelay: true,
        }
    }
}

impl SocketTuning {
    /// Read tuning from the environment: `RIFT_TCP_BACKLOG` (positive integer)
    /// and `RIFT_TCP_NODELAY` (`false`/`0`/`off` disables; anything else keeps
    /// the default of enabled). Unset or unparsable values fall back to
    /// [`SocketTuning::default`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::parse(
            std::env::var("RIFT_TCP_BACKLOG").ok().as_deref(),
            std::env::var("RIFT_TCP_NODELAY").ok().as_deref(),
        )
    }

    /// Pure parser behind [`SocketTuning::from_env`] — kept env-free so it is
    /// testable without mutating process-global state.
    fn parse(backlog: Option<&str>, nodelay: Option<&str>) -> Self {
        let defaults = Self::default();
        let backlog = backlog
            .and_then(|s| s.trim().parse::<i32>().ok())
            .filter(|&b| b > 0)
            .unwrap_or(defaults.backlog);
        let nodelay = nodelay.map_or(defaults.nodelay, |s| {
            let s = s.trim();
            !(s.eq_ignore_ascii_case("false") || s == "0" || s.eq_ignore_ascii_case("off"))
        });
        Self { backlog, nodelay }
    }
}

/// Create a TCP listener with SO_REUSEPORT enabled for multi-worker setup,
/// using tuning read from the environment.
///
/// This allows multiple workers to bind to the same port, enabling
/// load distribution across multiple processes.
pub fn create_reusable_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    create_reusable_listener_tuned(addr, &SocketTuning::from_env())
}

/// Create a TCP listener with an explicit [`SocketTuning`] (backlog).
pub fn create_reusable_listener_tuned(
    addr: SocketAddr,
    tuning: &SocketTuning,
) -> std::io::Result<TcpListener> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };

    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

    socket.set_reuse_address(true)?;

    // Set SO_REUSEPORT on Unix (macOS, Linux, BSD)
    // On macOS, SO_REUSEPORT is available but through setsockopt
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        unsafe {
            let optval: libc::c_int = 1;
            let ret = libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of_val(&optval) as libc::socklen_t,
            );
            if ret != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        use std::os::fd::AsRawFd;
        unsafe {
            let optval: libc::c_int = 1;
            let ret = libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of_val(&optval) as libc::socklen_t,
            );
            if ret != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    socket.set_nonblocking(true)?;

    socket.bind(&addr.into())?;
    socket.listen(tuning.backlog)?;

    // Convert to tokio TcpListener
    let std_listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(std_listener)
}

/// Apply per-connection tuning to a freshly accepted stream.
///
/// A failure to set `TCP_NODELAY` is logged, not fatal: the connection is still
/// fully functional (just potentially higher-latency for small writes), so
/// serving the request is preferable to dropping the connection.
pub fn apply_stream_tuning(stream: &TcpStream, tuning: &SocketTuning) {
    if !tuning.nodelay {
        return;
    }
    if let Err(e) = stream.set_nodelay(true) {
        tracing::warn!("failed to set TCP_NODELAY on accepted stream: {e}");
    }
}

// ── Accept-error handling (issue #750) ───────────────────────────────────────────────────────
//
// A bare `accept(2)` error arm that logs and immediately retries has two coupled defects, both
// amplified ×N under the per-core listener fan-out (#745): it spins hot on a *systemic* error
// (fd exhaustion cannot be cured by retrying), and it logs per failed accept — the exact
// per-event-rate logging trap the journal-cap fix (#741) removed. The pieces below fix both with
// no cost on the happy path.

/// Whether an `accept(2)` error means the listener itself is unusable, as opposed to a transient
/// or resource-pressure condition that retrying can clear.
///
/// `EBADF` / `ENOTSOCK` / `EINVAL` say the descriptor is not a working listening socket; no amount
/// of backoff fixes that, so a loop whose owner can observe its death should terminate and surface
/// the error rather than retry forever (issues #826, #834). Everything else — including unknown
/// errnos — is left to the two-way [`classify_accept_error`], which retries.
///
/// Not every loop wants this: the imposter serve loops deliberately never terminate, because a
/// dying imposter loop is independently recoverable through the still-live admin API. Loops whose
/// failure is the *only* signal an owner gets (the admin plane, the metrics plane, `ProxyServer::run`)
/// consult this first.
///
/// Unix-only by construction: these are POSIX errnos. On other platforms nothing is treated as
/// fatal, matching the conservative "retry rather than die" default.
#[cfg(unix)]
pub fn is_fatal_listener_error(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EBADF) | Some(libc::ENOTSOCK) | Some(libc::EINVAL)
    )
}

#[cfg(not(unix))]
pub fn is_fatal_listener_error(_e: &std::io::Error) -> bool {
    false
}

/// How an `accept(2)` error should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptErrorClass {
    /// Expected under load; the next accept is likely fine — retry immediately, log at `debug`.
    Transient,
    /// Resource exhaustion (or an unknown error): retrying now cannot succeed until something
    /// frees, so back off. Unknown errors land here deliberately — spinning on an unrecognized
    /// failure is the worse outcome.
    Systemic,
}

/// Classify via [`std::io::ErrorKind`] so no `libc` dependency is needed: only the transient
/// classes are named (`ECONNABORTED`/`EINTR`/`ECONNRESET`); everything else — resource exhaustion
/// like `EMFILE`/`ENFILE`/`ENOBUFS` (`Uncategorized`) or `ENOMEM` (`OutOfMemory`), and any truly
/// unknown error — falls through `_` to systemic.
pub fn classify_accept_error(e: &std::io::Error) -> AcceptErrorClass {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionReset, Interrupted};
    match e.kind() {
        ConnectionAborted | Interrupted | ConnectionReset => AcceptErrorClass::Transient,
        _ => AcceptErrorClass::Systemic,
    }
}

/// Exponential backoff for the systemic accept-error path: 1ms doubling to a 1s cap, reset on the
/// first successful accept. Pure and clock-free so the schedule is unit-testable without sockets.
#[derive(Debug)]
pub struct AcceptBackoff {
    current: Duration,
}

impl Default for AcceptBackoff {
    fn default() -> Self {
        Self::new()
    }
}

impl AcceptBackoff {
    const INITIAL: Duration = Duration::from_millis(1);
    const MAX: Duration = Duration::from_secs(1);

    pub fn new() -> Self {
        Self {
            current: Self::INITIAL,
        }
    }

    /// The delay to wait now; advances the schedule (double, capped) for the next call.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(Self::MAX);
        delay
    }

    pub fn reset(&mut self) {
        self.current = Self::INITIAL;
    }
}

/// What a systemic accept error should produce in the log (issue #838).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptErrorEvent {
    /// The transition *into* the error state — emit the onset `error!`.
    Onset,
    /// Still failing after another [`AcceptErrorLog::REEMIT_EVERY`] suppressed errors — emit a
    /// reminder carrying the running suppressed count.
    StillDown { suppressed: u64 },
}

/// Rate-limited logging for the systemic accept-error path, mirroring the journal-cap warn fix
/// (#741): one `error!` on entering the error state, near-silence while in it (counting suppressed
/// errors), one `info!` carrying the suppressed count on recovery.
///
/// "Near-silence" rather than silence because a *sustained* outage would otherwise leave a single
/// log line as its only evidence — possibly emitted weeks before anyone looked. Since the retry
/// policy keeps the listener bound while it cannot serve, a wedged listener no longer refuses
/// connections, so that one line may be the only local signal there is (issue #838). Every
/// [`Self::REEMIT_EVERY`] suppressed errors the outage re-announces itself.
#[derive(Debug, Default)]
pub struct AcceptErrorLog {
    in_error: bool,
    suppressed: u64,
}

impl AcceptErrorLog {
    /// How many suppressed errors pass between reminder log lines while an outage persists.
    ///
    /// Deliberately a **count**, not a duration: this type is pure and clock-free so its schedule
    /// is unit-testable without sockets or a timer. The count maps onto time closely enough —
    /// a sustained outage sits at [`AcceptBackoff::MAX`] (1 s), so ≈1 error/s puts a reminder
    /// roughly every 10 minutes. Early in an outage the 1 ms→1 s ramp delivers errors faster and
    /// the first reminder arrives sooner, which is the right bias.
    pub const REEMIT_EVERY: u64 = 600;

    /// Record a systemic error. Returns `Some(Onset)` on the transition *into* the error state,
    /// `Some(StillDown { .. })` on every `REEMIT_EVERY`-th error while it persists, and `None`
    /// otherwise (the error is counted, not logged).
    ///
    /// A reminder does **not** reset the suppressed count: it stays the running total for the
    /// outage, so [`Self::on_success`] still reports the whole outage on recovery.
    pub fn on_error(&mut self) -> Option<AcceptErrorEvent> {
        if self.in_error {
            self.suppressed += 1;
            if self.suppressed.is_multiple_of(Self::REEMIT_EVERY) {
                Some(AcceptErrorEvent::StillDown {
                    suppressed: self.suppressed,
                })
            } else {
                None
            }
        } else {
            self.in_error = true;
            self.suppressed = 0;
            Some(AcceptErrorEvent::Onset)
        }
    }

    /// Record a successful accept. Returns `Some(suppressed)` **only** on the transition *out* of
    /// the error state (emit the single recovery `info!`); `None` in steady state, so a healthy
    /// accept path pays just one branch.
    pub fn on_success(&mut self) -> Option<u64> {
        if self.in_error {
            self.in_error = false;
            Some(std::mem::take(&mut self.suppressed))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- issue #716: HTTP connection-builder tuning knobs -------------------------------

    #[test]
    fn http_tuning_defaults() {
        let t = HttpTuning::default();
        assert_eq!(t.max_buf_size, DEFAULT_HTTP_MAX_BUF);
        assert_eq!(t.header_read_timeout, std::time::Duration::from_secs(30));
        assert_eq!(
            t.max_connections, None,
            "unlimited by default (today's behavior)"
        );
    }

    #[test]
    fn http_tuning_parse_falls_back_to_defaults_when_unset() {
        assert_eq!(HttpTuning::parse(None, None, None), HttpTuning::default());
    }

    #[test]
    fn http_tuning_parse_reads_max_buf() {
        assert_eq!(
            HttpTuning::parse(Some(" 16384 "), None, None).max_buf_size,
            16384
        );
    }

    #[test]
    fn http_tuning_parse_rejects_garbage_or_zero_max_buf() {
        for v in ["0", "-5", "nope", ""] {
            assert_eq!(
                HttpTuning::parse(Some(v), None, None).max_buf_size,
                DEFAULT_HTTP_MAX_BUF,
                "value {v} must fall back to the default"
            );
        }
    }

    #[test]
    fn http_tuning_parse_reads_header_timeout_seconds() {
        assert_eq!(
            HttpTuning::parse(None, Some("10"), None).header_read_timeout,
            std::time::Duration::from_secs(10)
        );
    }

    #[test]
    fn http_tuning_parse_rejects_garbage_header_timeout() {
        for v in ["nope", "-1", ""] {
            assert_eq!(
                HttpTuning::parse(None, Some(v), None).header_read_timeout,
                std::time::Duration::from_secs(30),
                "value {v} must fall back to the default"
            );
        }
    }

    #[test]
    fn http_tuning_parse_max_connections_opt_in() {
        // Unset or non-positive => unlimited (None); a positive value => a cap.
        assert_eq!(HttpTuning::parse(None, None, None).max_connections, None);
        assert_eq!(
            HttpTuning::parse(None, None, Some("0")).max_connections,
            None
        );
        assert_eq!(
            HttpTuning::parse(None, None, Some("nope")).max_connections,
            None
        );
        assert_eq!(
            HttpTuning::parse(None, None, Some(" 500 ")).max_connections,
            Some(500)
        );
    }

    #[test]
    fn default_tuning_enables_nodelay_and_uses_default_backlog() {
        let t = SocketTuning::default();
        assert_eq!(t.backlog, DEFAULT_BACKLOG);
        assert!(t.nodelay);
    }

    #[test]
    fn parse_falls_back_to_defaults_when_unset() {
        assert_eq!(SocketTuning::parse(None, None), SocketTuning::default());
    }

    #[test]
    fn parse_reads_a_positive_backlog() {
        assert_eq!(SocketTuning::parse(Some(" 2048 "), None).backlog, 2048);
    }

    #[test]
    fn parse_rejects_non_positive_or_garbage_backlog() {
        assert_eq!(
            SocketTuning::parse(Some("0"), None).backlog,
            DEFAULT_BACKLOG
        );
        assert_eq!(
            SocketTuning::parse(Some("-5"), None).backlog,
            DEFAULT_BACKLOG
        );
        assert_eq!(
            SocketTuning::parse(Some("nope"), None).backlog,
            DEFAULT_BACKLOG
        );
    }

    #[test]
    fn parse_disables_nodelay_on_falsey_values() {
        for v in ["false", "FALSE", "0", "off", "Off"] {
            assert!(!SocketTuning::parse(None, Some(v)).nodelay, "value {v}");
        }
    }

    #[test]
    fn parse_keeps_nodelay_enabled_otherwise() {
        for v in ["true", "1", "on", "yes", ""] {
            assert!(SocketTuning::parse(None, Some(v)).nodelay, "value {v}");
        }
    }

    #[tokio::test]
    async fn accepted_stream_gets_nodelay() {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
        let listener =
            create_reusable_listener_tuned(addr, &SocketTuning::default()).expect("listener");
        let local = listener.local_addr().expect("local_addr");

        let client = tokio::spawn(async move { TcpStream::connect(local).await });

        let (server_stream, _) = listener.accept().await.expect("accept");
        apply_stream_tuning(&server_stream, &SocketTuning::default());
        assert!(server_stream.nodelay().expect("nodelay query"));

        let _ = client.await.expect("join").expect("connect");
    }

    #[tokio::test]
    async fn nodelay_left_untouched_when_disabled() {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
        let tuning = SocketTuning {
            backlog: 128,
            nodelay: false,
        };
        let listener = create_reusable_listener_tuned(addr, &tuning).expect("listener");
        let local = listener.local_addr().expect("local_addr");

        let client = tokio::spawn(async move { TcpStream::connect(local).await });

        let (server_stream, _) = listener.accept().await.expect("accept");
        // With nodelay disabled we must not force it on; the OS default is off.
        apply_stream_tuning(&server_stream, &tuning);
        assert!(!server_stream.nodelay().expect("nodelay query"));

        let _ = client.await.expect("join").expect("connect");
    }
}

#[cfg(test)]
mod accept_error {
    use super::*;
    use std::io::{Error, ErrorKind};

    // The fatal class (#826, shared by every owner-observable loop since #834): a broken listener fd
    // must still end the loop so its owner can report the death instead of retrying forever.
    #[cfg(unix)]
    #[test]
    fn broken_listener_errnos_are_fatal_but_pressure_is_not() {
        for raw in [libc::EBADF, libc::ENOTSOCK, libc::EINVAL] {
            assert!(
                is_fatal_listener_error(&Error::from_raw_os_error(raw)),
                "errno {raw} means the listener is unusable; retrying forever would hide a dead listener from its owner"
            );
        }
        // Recoverable pressure and transient blips must never be treated as fatal.
        for raw in [libc::EMFILE, libc::ENFILE, libc::ECONNABORTED, libc::EINTR] {
            assert!(
                !is_fatal_listener_error(&Error::from_raw_os_error(raw)),
                "errno {raw} is recoverable and must be retried, not fatal"
            );
        }
    }

    #[test]
    fn classify_transient_vs_systemic() {
        for kind in [
            ErrorKind::ConnectionAborted,
            ErrorKind::Interrupted,
            ErrorKind::ConnectionReset,
        ] {
            assert_eq!(
                classify_accept_error(&Error::from(kind)),
                AcceptErrorClass::Transient,
                "{kind:?} must be transient (retry immediately)"
            );
        }
        // EMFILE (24) / ENFILE (23) map to Uncategorized -> systemic; and a named non-transient
        // kind is systemic too. Unknown must never be treated as transient.
        for e in [
            Error::from_raw_os_error(24), // EMFILE
            Error::from_raw_os_error(23), // ENFILE
            Error::from(ErrorKind::OutOfMemory),
            Error::from(ErrorKind::Other),
        ] {
            assert_eq!(
                classify_accept_error(&e),
                AcceptErrorClass::Systemic,
                "{e:?} must back off, not spin"
            );
        }
    }

    #[test]
    fn backoff_doubles_from_1ms_caps_at_1s_and_resets() {
        let mut b = AcceptBackoff::new();
        assert_eq!(
            b.next_delay(),
            Duration::from_millis(1),
            "first delay is 1ms, not 0"
        );
        assert_eq!(b.next_delay(), Duration::from_millis(2));
        assert_eq!(b.next_delay(), Duration::from_millis(4));
        assert_eq!(b.next_delay(), Duration::from_millis(8));
        // Fast-forward well past the cap.
        for _ in 0..20 {
            b.next_delay();
        }
        assert_eq!(b.next_delay(), Duration::from_secs(1), "capped at 1s");
        b.reset();
        assert_eq!(
            b.next_delay(),
            Duration::from_millis(1),
            "reset returns to 1ms"
        );
    }

    // The log state machine backs the "one error! on onset, one info! with the suppressed count
    // on recovery" AC without sockets: on_error transitions once, steady state stays silent.
    #[test]
    fn log_emits_once_per_transition_with_suppressed_count() {
        let mut log = AcceptErrorLog::default();
        assert_eq!(
            log.on_error(),
            Some(AcceptErrorEvent::Onset),
            "first systemic error -> emit the one error!"
        );
        assert_eq!(log.on_error(), None, "second is suppressed");
        assert_eq!(log.on_error(), None, "third is suppressed");
        assert_eq!(
            log.on_success(),
            Some(2),
            "recovery emits one info! carrying the 2 suppressed errors"
        );
        assert_eq!(log.on_success(), None, "steady state is silent");
        assert_eq!(log.on_success(), None);

        // A fresh outage re-arms: one error!, count restarts from 0.
        assert_eq!(log.on_error(), Some(AcceptErrorEvent::Onset));
        assert_eq!(
            log.on_success(),
            Some(0),
            "immediate recovery suppressed nothing"
        );
    }

    // Issue #838: a sustained outage must re-announce itself, or its only evidence is one log line
    // emitted whenever it started. Reminders are count-based so this stays clock-free.
    #[test]
    fn sustained_outage_reemits_every_n_and_keeps_the_running_total() {
        let mut log = AcceptErrorLog::default();
        assert_eq!(log.on_error(), Some(AcceptErrorEvent::Onset));

        // Everything strictly between onset and the Nth suppressed error is silent.
        for i in 1..AcceptErrorLog::REEMIT_EVERY {
            assert_eq!(log.on_error(), None, "suppressed error {i} must not log");
        }
        assert_eq!(
            log.on_error(),
            Some(AcceptErrorEvent::StillDown {
                suppressed: AcceptErrorLog::REEMIT_EVERY
            }),
            "the REEMIT_EVERY-th suppressed error re-announces the outage"
        );

        // The reminder does NOT reset the count — the next one carries 2N, and recovery still
        // reports the whole outage.
        for _ in 1..AcceptErrorLog::REEMIT_EVERY {
            assert_eq!(log.on_error(), None);
        }
        assert_eq!(
            log.on_error(),
            Some(AcceptErrorEvent::StillDown {
                suppressed: AcceptErrorLog::REEMIT_EVERY * 2
            }),
            "reminders carry the running total, not a per-window count"
        );
        assert_eq!(
            log.on_success(),
            Some(AcceptErrorLog::REEMIT_EVERY * 2),
            "recovery reports every error suppressed during the outage"
        );

        // And a fresh outage re-arms the reminder schedule from zero.
        assert_eq!(log.on_error(), Some(AcceptErrorEvent::Onset));
        assert_eq!(log.on_error(), None, "the new outage counts from 0 again");
    }
}
