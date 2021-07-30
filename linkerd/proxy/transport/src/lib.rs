//! Utilities for use TCP servers & clients.
//!
//! Uses unsafe code to interact with socket options for keepalive and SO_ORIGINAL_DST.

#![deny(warnings, rust_2018_idioms)]
//#![forbid(unsafe_code)]

pub mod addrs;
mod connect;
pub mod listen;
pub mod metrics;
pub mod orig_dst;

pub use self::{
    addrs::{ClientAddr, ListenAddr, Local, OrigDstAddr, Remote, ServerAddr},
    connect::ConnectTcp,
    listen::{Bind, BindTcp},
    orig_dst::BindWithOrigDst,
};
use socket2::TcpKeepalive;
use std::time::Duration;
use tokio::net::TcpStream;

#[derive(Copy, Clone, Debug, Default)]
pub struct Keepalive(pub Option<Duration>);

impl From<Keepalive> for Option<Duration> {
    fn from(Keepalive(duration): Keepalive) -> Option<Duration> {
        duration
    }
}

// Misc.

fn set_nodelay_or_warn(socket: &TcpStream) {
    if let Err(e) = socket.set_nodelay(true) {
        tracing::warn!("failed to set nodelay: {}", e);
    }
}

fn set_keepalive_or_warn(tcp: &TcpStream, keepalive_duration: Option<Duration>) {
    // TODO(eliza): when https://github.com/tokio-rs/tokio/pull/3189 merges
    // upstream, we will be able to convert the Tokio `TcpStream` into a
    // `socket2::Socket` without unsafe, by converting it to a
    // `std::net::TcpStream` (as `socket2::Socket` has a
    // `From<std::net::TcpStream>`). What we're doing now is more or less
    // equivalent, but this would use a safe interface...
    #[cfg(unix)]
    let sock = unsafe {
        // Safety: `from_raw_fd` takes ownership of the underlying file
        // descriptor, and will close it when dropped. However, we obtain the
        // file descriptor via `as_raw_fd` rather than `into_raw_fd`, so the
        // Tokio `TcpStream` *also* retains ownership of the socket --- which is
        // what we want. Instead of letting the `socket2` socket returned by
        // `from_raw_fd` close the fd, we `mem::forget` the `Socket`, so that
        // its `Drop` impl will not run. This ensures the fd is not closed
        // prematurely.
        use std::os::unix::io::{AsRawFd, FromRawFd};
        socket2::Socket::from_raw_fd(tcp.as_raw_fd())
    };
    #[cfg(windows)]
    let sock = unsafe {
        // Safety: `from_raw_socket` takes ownership of the underlying Windows
        // SOCKET, and will close it when dropped. However, we obtain the
        // SOCKET via `as_raw_socket` rather than `into_raw_socket`, so the
        // Tokio `TcpStream` *also* retains ownership of the socket --- which is
        // what we want. Instead of letting the `socket2` socket returned by
        // `from_raw_socket` close the SOCKET, we `mem::forget` the `Socket`, so
        // that its `Drop` impl will not run. This ensures the socket is not
        // closed prematurely.
        use std::os::windows::io::{AsRawSocket, FromRawSocket};
        socket2::Socket::from_raw_socket(tcp.as_raw_socket())
    };

    let ka = keepalive_duration
        .into_iter()
        .fold(TcpKeepalive::new(), |k, t| k.with_time(t));
    if let Err(e) = sock.set_tcp_keepalive(&ka) {
        tracing::warn!("failed to set keepalive: {}", e);
    }

    // Don't let the socket2 socket close the fd on drop!
    std::mem::forget(sock);
}
