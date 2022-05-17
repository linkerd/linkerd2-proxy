//! Utilities for use TCP servers & clients.
//!
//! Uses unsafe code to interact with socket options for SO_ORIGINAL_DST.

#![deny(
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    unsafe_code
)]

pub mod addrs;
mod connect;
pub mod listen;
pub mod orig_dst;

pub use self::{
    addrs::{ClientAddr, ListenAddr, Local, OrigDstAddr, Remote, ServerAddr},
    connect::ConnectTcp,
    listen::{Bind, BindTcp},
    orig_dst::BindWithOrigDst,
};
use linkerd_io as io;
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

fn set_keepalive_or_warn(
    tcp: TcpStream,
    keepalive_duration: Option<Duration>,
) -> io::Result<TcpStream> {
    let sock = {
        let stream = tokio::net::TcpStream::into_std(tcp)?;
        socket2::Socket::from(stream)
    };
    let ka = keepalive_duration
        .into_iter()
        .fold(TcpKeepalive::new(), |k, t| k.with_time(t));
    if let Err(e) = sock.set_tcp_keepalive(&ka) {
        tracing::warn!("failed to set keepalive: {}", e);
    }
    let stream: std::net::TcpStream = socket2::Socket::into(sock);
    tokio::net::TcpStream::from_std(stream)
}
