//! Network utilities for the proxy server.
//!
//! This module provides network-related functionality including
//! creating TCP listeners with SO_REUSEPORT for multi-worker setups.

use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use tokio::net::TcpListener;

/// Create a TCP listener with SO_REUSEPORT enabled for multi-worker setup.
///
/// This allows multiple workers to bind to the same port, enabling
/// load distribution across multiple processes.
pub fn create_reusable_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
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
    socket.listen(1024)?; // Backlog size

    // Convert to tokio TcpListener
    let std_listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(std_listener)
}
