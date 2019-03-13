mod addr_info;
pub mod connect;
mod connection;
mod io;
pub mod keepalive;
pub mod metrics;
mod prefixed;
pub mod tls;

#[cfg(test)]
mod connection_tests;

pub use self::{
    addr_info::{AddrInfo, GetOriginalDst, SoOriginalDst},
    connect::Connect,
    connection::{Connection, Peek},
    io::BoxedIo,
    keepalive::SetKeepalive,
};

// Misc.

fn set_nodelay_or_warn(socket: &::std::net::TcpStream) {
    if let Err(e) = socket.set_nodelay(true) {
        warn!(
            "could not set TCP_NODELAY on {:?}/{:?}: {}",
            socket.local_addr(),
            socket.peer_addr(),
            e
        );
    }
}
