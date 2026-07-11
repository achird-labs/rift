//! Network utilities for the proxy server.
//!
//! This module provides network-related functionality including
//! creating TCP listeners with SO_REUSEPORT for multi-worker setups and
//! applying per-connection socket tuning (TCP_NODELAY) to accepted streams.

use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};

/// Default listen backlog; overridable via `RIFT_TCP_BACKLOG`.
const DEFAULT_BACKLOG: i32 = 1024;

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

#[cfg(test)]
mod tests {
    use super::*;

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
