use crate::svc::{mk, Service};
use futures::{try_ready, Future, Poll};
use std::{io, net::SocketAddr, time::Duration};
use tokio::net::{tcp, TcpStream};
use tracing::debug;

pub trait HasPeerAddr {
    fn peer_addr(&self) -> SocketAddr;
}

pub fn svc<T>(
    keepalive: Option<Duration>,
) -> impl Service<T, Response = TcpStream, Error = io::Error, Future = ConnectFuture> + Clone
where
    T: HasPeerAddr,
{
    mk(move |target: T| {
        let addr = target.peer_addr();
        debug!("connecting to {}", addr);
        ConnectFuture {
            addr,
            keepalive,
            future: TcpStream::connect(&addr),
        }
    })
}

#[derive(Debug)]
pub struct ConnectFuture {
    addr: SocketAddr,
    keepalive: Option<Duration>,
    future: tcp::ConnectFuture,
}

impl HasPeerAddr for SocketAddr {
    fn peer_addr(&self) -> SocketAddr {
        *self
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
        debug!("connection established to {}", self.addr);
        super::set_nodelay_or_warn(&io);
        super::set_keepalive_or_warn(&io, self.keepalive);
        Ok(io.into())
    }
}
