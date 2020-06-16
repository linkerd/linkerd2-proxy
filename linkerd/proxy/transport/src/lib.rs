#![deny(warnings, rust_2018_idioms)]
#![type_length_limit = "1586225"]

use std::time::Duration;
use tokio::net::TcpStream;

pub mod connect;
pub use linkerd2_io as io;
pub mod listen;
pub mod metrics;
pub mod tls;

pub use self::{
    connect::Connect,
    io::BoxedIo,
    listen::{Bind, DefaultOrigDstAddr, Listen, NoOrigDstAddr, OrigDstAddr},
};

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
