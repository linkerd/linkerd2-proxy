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
    keepalive::SetKeepalive,
    peek::Peek,
    tls::{Connection, Listen},
};

// Misc.

fn set_nodelay_or_warn(socket: &::tokio::net::TcpStream) {
    if let Err(e) = socket.set_nodelay(true) {
        warn!(
            "could not set TCP_NODELAY on {:?}/{:?}: {}",
            socket.local_addr(),
            socket.peer_addr(),
            e
        );
    }
}
