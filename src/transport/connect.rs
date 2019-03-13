extern crate tokio_connect;

pub use self::tokio_connect::Connect;
use futures::{Future, Poll};
use std::{hash, io, net::SocketAddr};
use tokio::net::{tcp, TcpStream};

use super::{connection, tls};
use never::Never;
use svc;
use Conditional;

pub trait PeerSocketAddr {
    fn peer_socket_addr(&self) -> SocketAddr;
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
pub struct ConnectFuture(SocketAddr, tcp::ConnectFuture);

// ===== impl Stack =====

impl Stack {
    pub fn new() -> Self {
        Stack(())
    }
}

impl<T: PeerSocketAddr> svc::Stack<T> for Stack {
    type Value = ConnectSocketAddr;
    type Error = Never;

    fn make(&self, t: &T) -> Result<Self::Value, Self::Error> {
        Ok(t.peer_socket_addr().into())
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
        ConnectFuture(TcpStream::connect(self.0))
    }
}

// === impl ConnectFuture ===

impl Future for ConnectFuture {
    type Item = TcpStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.1
            .poll()
            .map_err(|e| {
                let details = format!("{} (address: {})", e, self.0);
                io::Error::new(e.kind(), details)
            })
            .map(|s| {
                super::set_nodelay_or_warn(&s);
                s
            })
    }
}
