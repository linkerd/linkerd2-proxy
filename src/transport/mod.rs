use std::time::Duration;
use tokio::net::TcpStream;

mod addr_info;
pub mod connect;
mod io;
pub mod keepalive;
pub mod metrics;
mod peek;
mod prefixed;
pub mod tls;

pub use self::{
    addr_info::{AddrInfo, GetOriginalDst, SoOriginalDst},
    io::BoxedIo,
    peek::Peek,
    tls::{Connection, Listen},
};

// Misc.

fn set_nodelay_or_warn(socket: &::tokio::net::TcpStream) {
    if let Err(e) = socket.set_nodelay(true) {
        tracing::warn!("failed to set nodelay: {}", e);
    }
}

pub fn set_keepalive_or_warn(tcp: &mut TcpStream, ka: Option<Duration>) {
    if let Err(e) = tcp.set_keepalive(ka) {
        tracing::warn!("failed to set keepalive: {}", e);
    }
}
