use futures_03::compat::{Compat01As03, Future01CompatExt};
use std::task::{Context, Poll};
use std::{future::Future, io, net::SocketAddr, pin::Pin, time::Duration};
use tokio::net::{tcp, TcpStream};
use tracing::debug;

pub trait ConnectAddr {
    fn connect_addr(&self) -> SocketAddr;
}

#[derive(Copy, Clone, Debug)]
pub struct Connect {
    keepalive: Option<Duration>,
}

#[pin_project::pin_project]
#[derive(Debug)]
pub struct ConnectFuture {
    keepalive: Option<Duration>,
    #[pin]
    future: Compat01As03<tcp::ConnectFuture>,
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

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, c: C) -> Self::Future {
        let keepalive = self.keepalive;
        let addr = c.connect_addr();
        debug!(peer.addr = %addr, "Connecting");
        let future = TcpStream::connect(&addr).compat();
        ConnectFuture { future, keepalive }
    }
}

impl Future for ConnectFuture {
    type Output = Result<TcpStream, io::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let io = futures_03::ready!(this.future.poll(cx))?;
        let keepalive = this.keepalive.take();
        super::set_nodelay_or_warn(&io);
        super::set_keepalive_or_warn(&io, keepalive);
        debug!(
            local.addr = %io.local_addr().expect("cannot load local addr"),
            ?keepalive,
            "Connected",
        );
        Poll::Ready(Ok(io.into()))
    }
}
