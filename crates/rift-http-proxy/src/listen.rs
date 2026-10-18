//! Where the server-level listeners bind (issue #1528): the admin API, `/metrics`, the front door.
//!
//! With no `--host`, Mountebank's admin listener is Node's `listen(port)`, which binds `[::]`
//! dual-stack. [`ListenAddr`] gives the server listeners the same default, a two-address loopback
//! for `--local-only`, and an exact bind for an explicit `--host`. [`ListenerSet`] accepts on every
//! listener one address needs, so each accept loop keeps a single loop and a single outcome.

pub(crate) use rift_mock_core::imposter::PortReservation;
use rift_mock_core::imposter::{ImposterManager, ServerListener};
use rift_mock_core::proxy::{bind_server_listener, bind_unspecified};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::task::Poll;
use tokio::net::{TcpListener, TcpStream};
use tracing::debug;

/// Refuse to bind a server listener on a port an imposter already accepts on (issue #1557). A
/// startup imposter loads before the server listeners bind, and on macOS the listener's bind would
/// otherwise succeed beside the imposter's and split the port's clients with it. Linux refuses the
/// bind itself. `0` asks the kernel for a free port, which no imposter holds.
pub(crate) fn refuse_imposter_port(
    manager: &ImposterManager,
    port: u16,
    by: ServerListener,
) -> io::Result<()> {
    if port != 0 && manager.imposter_listens_on(port) {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("port {port} is already in use by an imposter; the {by} cannot bind it"),
        ));
    }
    Ok(())
}

/// How many kernel-chosen ports `Loopback(0)` tries before giving up, when `[::1]` is taken on the
/// port `127.0.0.1` was given.
const LOOPBACK_PORT_ATTEMPTS: usize = 8;

/// Where a server-level listener binds (#1528).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenAddr {
    /// No host given: `[::]` dual-stack, falling back to `0.0.0.0` where the host has no IPv6
    /// (#1514's `bind_unspecified`). Judged as `[::]:port` by the exposure check.
    Any(u16),
    /// `--local-only`: `127.0.0.1` and, where the host has IPv6 loopback, `[::1]` on the same port.
    Loopback(u16),
    /// An explicit `--host`: exactly this address, never a fallback. An IPv6 address is dual-stack.
    Exact(SocketAddr),
}

impl ListenAddr {
    /// The address #863's exposure check judges.
    #[must_use]
    pub fn judged(&self) -> SocketAddr {
        match *self {
            Self::Any(port) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
            Self::Loopback(port) => SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            Self::Exact(addr) => addr,
        }
    }

    /// Bind every listener this address needs: nonblocking std listeners, ready for tokio. Never
    /// empty on success.
    ///
    /// # Errors
    /// The bind error. Under `Loopback` with an explicit port, `[::1]` held by another socket is an
    /// error naming `[::1]`: serving IPv4 only would hand `localhost` clients that try `::1` first
    /// to that socket.
    pub fn bind(&self) -> io::Result<Vec<std::net::TcpListener>> {
        match *self {
            Self::Any(port) => {
                refuse_held_ipv4_wildcard(port)?;
                Ok(vec![bind_unspecified(port, bind_server_listener)?])
            }
            Self::Loopback(port) => bind_loopback(port),
            Self::Exact(addr) => Ok(vec![bind_server_listener(addr)?]),
        }
    }
}

/// Refuse a port another socket already listens on at `0.0.0.0`. On macOS and the BSDs
/// `SO_REUSEADDR` lets a dual-stack `[::]` bind succeed beside it, and the two would then split the
/// port's IPv4 clients (the #1501 class). A plain IPv4 wildcard bind is refused by such a holder and
/// by nothing else, since it never listens long enough to leave connections in `TIME_WAIT`.
fn refuse_held_ipv4_wildcard(port: u16) -> io::Result<()> {
    if port == 0 {
        return Ok(());
    }
    let v4 = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    match std::net::TcpListener::bind(v4) {
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Err(io::Error::new(
            e.kind(),
            format!("binding [::]:{port}: {v4} is held by another socket: {e}"),
        )),
        // Free, or no IPv4 at all: the dual-stack bind below decides.
        _ => Ok(()),
    }
}

fn bind_loopback(port: u16) -> io::Result<Vec<std::net::TcpListener>> {
    let attempts = if port == 0 { LOOPBACK_PORT_ATTEMPTS } else { 1 };
    let mut last_conflict = None;
    for _ in 0..attempts {
        let v4 = bind_server_listener(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
        let bound = v4.local_addr()?.port();
        let v6_addr = SocketAddr::from((Ipv6Addr::LOCALHOST, bound));
        match bind_server_listener(v6_addr) {
            Ok(v6) => return Ok(vec![v4, v6]),
            Err(e) if no_ipv6_loopback(&e) => {
                debug!("no IPv6 loopback ({e}); --local-only listens on 127.0.0.1:{bound} only");
                return Ok(vec![v4]);
            }
            // A kernel-chosen port that is free on IPv4 but taken on IPv6: try another one.
            Err(e) if port == 0 && e.kind() == io::ErrorKind::AddrInUse => last_conflict = Some(e),
            Err(e) => return Err(io::Error::new(e.kind(), format!("binding {v6_addr}: {e}"))),
        }
    }
    let detail = last_conflict.map_or_else(String::new, |e| format!(": {e}"));
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        format!(
            "no port free on both 127.0.0.1 and [::1] after {LOOPBACK_PORT_ATTEMPTS} attempts{detail}"
        ),
    ))
}

/// Whether a `[::1]` bind failed because this host has no IPv6 loopback, rather than over a
/// conflict.
fn no_ipv6_loopback(e: &io::Error) -> bool {
    #[cfg(unix)]
    let unsupported_family = e.raw_os_error() == Some(libc::EAFNOSUPPORT);
    #[cfg(windows)]
    let unsupported_family = e.raw_os_error() == Some(10047); // WSAEAFNOSUPPORT
    #[cfg(not(any(unix, windows)))]
    let unsupported_family = false;
    unsupported_family
        || matches!(
            e.kind(),
            io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
        )
}

/// The tokio listeners behind one [`ListenAddr`], accepted on as one.
pub(crate) struct ListenerSet {
    listeners: Vec<TcpListener>,
    /// The listener polled first on the next accept, rotated so a busy one cannot starve another.
    next: usize,
}

impl ListenerSet {
    /// Register `listeners` with the current tokio runtime.
    pub(crate) fn from_std(listeners: Vec<std::net::TcpListener>) -> io::Result<Self> {
        if listeners.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no listener to accept on",
            ));
        }
        let listeners = listeners
            .into_iter()
            .map(TcpListener::from_std)
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { listeners, next: 0 })
    }

    /// Every bound address, in bind order.
    pub(crate) fn local_addrs(&self) -> io::Result<Vec<SocketAddr>> {
        self.listeners.iter().map(TcpListener::local_addr).collect()
    }

    /// The next connection on any listener, or the first listener error. Cancel-safe, like
    /// [`TcpListener::accept`].
    pub(crate) async fn accept(&mut self) -> io::Result<(TcpStream, SocketAddr)> {
        std::future::poll_fn(|cx| {
            let count = self.listeners.len();
            for offset in 0..count {
                let index = (self.next + offset) % count;
                if let Poll::Ready(result) = self.listeners[index].poll_accept(cx) {
                    self.next = (index + 1) % count;
                    return Poll::Ready(result);
                }
            }
            Poll::Pending
        })
        .await
    }
}

/// `http://a{suffix} and http://b{suffix}` for a startup log line.
pub(crate) fn http_urls(addrs: &[SocketAddr], suffix: &str) -> String {
    addrs
        .iter()
        .map(|addr| format!("http://{addr}{suffix}"))
        .collect::<Vec<_>>()
        .join(" and ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_ipv6_loopback() -> bool {
        std::net::TcpListener::bind("[::1]:0").is_ok()
    }

    fn connects(addr: SocketAddr) -> bool {
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)).is_ok()
    }

    #[test]
    fn judged_covers_every_variant() {
        assert_eq!(
            ListenAddr::Any(2525).judged(),
            "[::]:2525".parse::<SocketAddr>().expect("addr")
        );
        assert_eq!(
            ListenAddr::Loopback(2525).judged(),
            "127.0.0.1:2525".parse::<SocketAddr>().expect("addr")
        );
        let exact: SocketAddr = "[::1]:7".parse().expect("addr");
        assert_eq!(ListenAddr::Exact(exact).judged(), exact);
    }

    #[test]
    fn any_binds_the_ipv6_wildcard_and_answers_both_families() {
        let listeners = ListenAddr::Any(0).bind().expect("bind");
        assert_eq!(listeners.len(), 1);
        let addr = listeners[0].local_addr().expect("addr");
        let port = addr.port();
        assert!(connects(SocketAddr::from((Ipv4Addr::LOCALHOST, port))));
        if has_ipv6_loopback() {
            assert_eq!(addr.ip(), Ipv6Addr::UNSPECIFIED);
            assert!(connects(SocketAddr::from((Ipv6Addr::LOCALHOST, port))));
        }
    }

    #[test]
    fn any_on_a_port_held_on_the_ipv4_wildcard_is_refused() {
        let holder = std::net::TcpListener::bind("0.0.0.0:0").expect("hold a port");
        let port = holder.local_addr().expect("addr").port();
        let err = ListenAddr::Any(port)
            .bind()
            .expect_err("0.0.0.0 is held by another socket");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn loopback_binds_both_loopbacks_on_one_port() {
        let listeners = ListenAddr::Loopback(0).bind().expect("bind");
        let addrs: Vec<SocketAddr> = listeners
            .iter()
            .map(|l| l.local_addr().expect("addr"))
            .collect();
        let port = addrs[0].port();
        assert_eq!(addrs[0], SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
        assert!(connects(addrs[0]));
        if has_ipv6_loopback() {
            assert_eq!(
                addrs,
                vec![
                    SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                    SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
                ]
            );
            assert!(connects(addrs[1]));
        }
    }

    /// The host's first non-loopback IPv4 address, found by asking the routing table which source
    /// address it would use; no packet is sent by a UDP `connect`.
    fn non_loopback_ipv4() -> Option<Ipv4Addr> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("192.0.2.1:9").ok()?;
        match socket.local_addr().ok()?.ip() {
            std::net::IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
            _ => None,
        }
    }

    #[test]
    fn loopback_refuses_the_lan_address() {
        let Some(lan) = non_loopback_ipv4() else {
            return; // no non-loopback interface on this host
        };
        let listeners = ListenAddr::Loopback(0).bind().expect("bind");
        let port = listeners[0].local_addr().expect("addr").port();
        assert!(
            !connects(SocketAddr::from((lan, port))),
            "--local-only must not answer on {lan}:{port}"
        );
    }

    #[test]
    fn loopback_with_ipv6_loopback_held_names_it() {
        let Ok(holder) = std::net::TcpListener::bind("[::1]:0") else {
            return; // no IPv6 loopback on this host
        };
        let port = holder.local_addr().expect("addr").port();
        // The kernel may have handed this port to someone on 127.0.0.1 already; then the v4 bind
        // fails first and there is nothing to learn from this run.
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
            return;
        }
        let err = ListenAddr::Loopback(port)
            .bind()
            .expect_err("[::1] is held by another socket");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(err.to_string().contains("[::1]"), "{err}");
    }

    #[test]
    fn exact_ipv6_wildcard_is_dual_stack() {
        let Ok(listeners) = ListenAddr::Exact("[::]:0".parse().expect("addr")).bind() else {
            return; // no IPv6 on this host
        };
        assert!(
            !socket2::SockRef::from(&listeners[0])
                .only_v6()
                .expect("IPV6_V6ONLY")
        );
    }

    #[cfg(unix)]
    #[test]
    fn any_falls_back_to_ipv4_without_ipv6() {
        let bound = bind_unspecified(0, |addr: SocketAddr| {
            if addr.is_ipv6() {
                Err(io::Error::from_raw_os_error(libc::EAFNOSUPPORT))
            } else {
                bind_server_listener(addr)
            }
        })
        .expect("v4 fallback");
        assert_eq!(
            bound.local_addr().expect("addr").ip(),
            Ipv4Addr::UNSPECIFIED
        );
    }

    #[tokio::test]
    async fn a_listener_set_accepts_on_every_listener() {
        let listeners = ListenAddr::Loopback(0).bind().expect("bind");
        let mut set = ListenerSet::from_std(listeners).expect("register");
        for addr in set.local_addrs().expect("addrs") {
            let connect = tokio::net::TcpStream::connect(addr);
            let (accepted, connected) = tokio::join!(set.accept(), connect);
            connected.expect("connect");
            let (_, peer) = accepted.expect("accept");
            assert_eq!(peer.is_ipv6(), addr.is_ipv6(), "{addr}");
        }
    }

    #[test]
    fn an_empty_listener_set_is_refused() {
        let err = ListenerSet::from_std(Vec::new())
            .err()
            .expect("an empty set never accepts");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
