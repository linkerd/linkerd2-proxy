extern crate tokio_connect;

pub use self::tokio_connect::Connect;
use futures::{Future, Poll};
use std::{io, net::SocketAddr};
use tokio::net::{tcp, TcpStream};

use never::Never;
use svc;

pub trait HasPeerAddr {
    fn peer_addr(&self) -> SocketAddr;
}

#[derive(Debug, Clone)]
pub struct Stack(());

/// A TCP connection target, optionally with TLS.
///
/// Comparison operations ignore the TLS ClientConfig and only account for the
/// TLS status.
#[derive(Clone, Debug)]
pub struct ConnectSocketAddr(SocketAddr);

#[derive(Debug)]
pub struct ConnectFuture {
    addr: SocketAddr,
    future: tcp::ConnectFuture,
}

impl HasPeerAddr for SocketAddr {
    fn peer_addr(&self) -> SocketAddr {
        *self
    }
}

// ===== impl Stack =====

impl Stack {
    pub fn new() -> Self {
        Stack(())
    }
}

impl<T: HasPeerAddr> svc::Stack<T> for Stack {
    type Value = ConnectSocketAddr;
    type Error = Never;

    fn make(&self, t: &T) -> Result<Self::Value, Self::Error> {
        Ok(t.peer_addr().into())
    }
}

// === impl ConnectSocketAddr ===

impl From<SocketAddr> for ConnectSocketAddr {
    fn from(sa: SocketAddr) -> Self {
        ConnectSocketAddr(sa)
    }
}

impl Connect for ConnectSocketAddr {
    type Connected = TcpStream;
    type Error = io::Error;
    type Future = ConnectFuture;

    fn connect(&self) -> Self::Future {
        ConnectFuture {
            addr: self.0,
            future: TcpStream::connect(&self.0),
        }
    }
}

// === impl ConnectFuture ===

impl Future for ConnectFuture {
    type Item = TcpStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let io = try_ready!(self.future.poll().map_err(|e| {
            let details = format!("{} (address: {})", e, self.addr);
            io::Error::new(e.kind(), details)
        }));
        super::set_nodelay_or_warn(&io);
        Ok(io.into())
    }
}
