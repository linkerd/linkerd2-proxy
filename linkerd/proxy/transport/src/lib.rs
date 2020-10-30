#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "256"]

use std::time::Duration;
use tokio::net::TcpStream;

pub mod connect;
pub use linkerd2_io as io;
pub mod listen;
pub mod metrics;
pub mod prefix;
pub mod tls;

pub use self::{
    connect::Connect,
    io::BoxedIo,
    listen::{Bind, DefaultOrigDstAddr, NoOrigDstAddr, OrigDstAddr},
    prefix::Prefix,
};

// Misc.

fn set_nodelay_or_warn(socket: &TcpStream) {
    if let Err(e) = socket.set_nodelay(true) {
        tracing::warn!("failed to set nodelay: {}", e);
    }
}

fn set_keepalive_or_warn(tcp: &TcpStream, ka: Option<Duration>) {
    // TODO(eliza): we now have to do "socket2 nonsense" here because tokio
    // doesn't have `set_keepalive` any more...
    let _ = (tcp, ka);
    // if let Err(e) = tcp.set_keepalive(ka) {
    //     tracing::warn!("failed to set keepalive: {}", e);
    // }
}
