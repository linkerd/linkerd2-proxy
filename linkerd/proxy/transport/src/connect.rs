use futures::{try_ready, Future, Poll};
use std::{io, net::SocketAddr, time::Duration};
use tokio::net::{tcp, TcpStream};
use tracing::debug;

pub trait ConnectAddr {
    fn connect_addr(&self) -> SocketAddr;
}

#[derive(Copy, Clone, Debug)]
pub struct Connect {
    keepalive: Option<Duration>,
}

#[derive(Debug)]
pub struct ConnectFuture {
    keepalive: Option<Duration>,
    future: tcp::ConnectFuture,
}

impl Connect {
    pub fn new(keepalive: Option<Duration>) -> Self {
        Connect { keepalive }
    }
}

impl<C: ConnectAddr> tower::Service<C> for Connect {
    type Response = TcpStream;
    type Error = io::Error;
    type Future = ConnectFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, c: C) -> Self::Future {
        let keepalive = self.keepalive;
        let addr = c.connect_addr();
        debug!(peer.addr = %addr, "Connecting");
        let future = TcpStream::connect(&addr);
        ConnectFuture { future, keepalive }
    }
}

impl Future for ConnectFuture {
    type Item = TcpStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let io = try_ready!(self.future.poll());
        super::set_nodelay_or_warn(&io);
        super::set_keepalive_or_warn(&io, self.keepalive);
        debug!(
            local.addr = %io.local_addr().expect("cannot load local addr"),
            keepalive = ?self.keepalive,
            "Connected",
        );
        Ok(io.into())
    }
}
