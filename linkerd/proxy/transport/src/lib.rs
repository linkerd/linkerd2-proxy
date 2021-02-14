#![deny(warnings, rust_2018_idioms)]

use std::time::Duration;
use tokio::net::TcpStream;

mod connect;
pub mod listen;
pub mod metrics;

pub use self::{
    connect::{ConnectAddr, ConnectTcp},
    listen::{BindTcp, DefaultOrigDstAddr, NoOrigDstAddr, OrigDstAddr},
};

// Misc.

fn set_nodelay_or_warn(socket: &TcpStream) {
    if let Err(e) = socket.set_nodelay(true) {
        tracing::warn!("failed to set nodelay: {}", e);
    }
}

fn set_keepalive_or_warn(tcp: &TcpStream, ka: Option<Duration>) {
    if let Err(err) = match tcp.into_std() {
        Ok(tcp_stream) => {
            let sock = socket2::Socket::from(tcp_stream);

            sock.set_keepalive(ka)
        }
        Err(err) => err,
    } {
        tracing::warn!("failed to set keepalive: {}", err);
    }
}
