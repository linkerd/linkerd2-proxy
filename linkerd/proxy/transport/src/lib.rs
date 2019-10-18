#![warn(rust_2018_idioms)]

use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;

mod addr_info;
pub mod connect;
mod io;
mod listen;
pub mod metrics;
mod peek;
mod prefixed;
pub mod tls;

pub use self::{
    addr_info::{AddrInfo, GetOriginalDst, SoOriginalDst},
    io::BoxedIo,
    listen::Listen,
    peek::Peek,
    tls::Connection,
};

/// Describes an accepted connection.
// XXX This type should be retired.
#[derive(Clone, Debug)]
pub struct Source {
    pub remote: SocketAddr,
    pub local: SocketAddr,
    pub orig_dst: Option<SocketAddr>,
    pub tls_peer: tls::PeerIdentity,
}

impl Source {
    pub fn orig_dst_if_not_local(&self) -> Option<SocketAddr> {
        self.orig_dst.and_then(|orig_dst| {
            if Self::same_addr(orig_dst, self.local) {
                None
            } else {
                Some(orig_dst)
            }
        })
    }

    fn same_addr(a0: SocketAddr, a1: SocketAddr) -> bool {
        use std::net::IpAddr::{V4, V6};
        (a0.port() == a1.port())
            && match (a0.ip(), a1.ip()) {
                (V6(a0), V4(a1)) => a0.to_ipv4() == Some(a1),
                (V4(a0), V6(a1)) => Some(a0) == a1.to_ipv4(),
                (a0, a1) => (a0 == a1),
            }
    }
}

// Misc.

fn set_nodelay_or_warn(socket: &TcpStream) {
    if let Err(e) = socket.set_nodelay(true) {
        tracing::warn!("failed to set nodelay: {}", e);
    }
}

fn set_keepalive_or_warn(tcp: &TcpStream, ka: Option<Duration>) {
    if let Err(e) = tcp.set_keepalive(ka) {
        tracing::warn!("failed to set keepalive: {}", e);
    }
}
