use crate::Keepalive;
use linkerd_io as io;
use linkerd_stack::Param;
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::net::TcpStream;
use tracing::debug;

#[derive(Copy, Clone, Debug)]
pub struct ConnectTcp {
    keepalive: Keepalive,
}

#[derive(Copy, Clone, Debug)]
pub struct ConnectAddr(pub SocketAddr);

impl ConnectTcp {
    pub fn new(keepalive: Keepalive) -> Self {
        Self { keepalive }
    }
}

impl<T: Param<ConnectAddr>> tower::Service<T> for ConnectTcp {
    type Response = io::ScopedIo<TcpStream>;
    type Error = io::Error;
    type Future =
        Pin<Box<dyn Future<Output = io::Result<io::ScopedIo<TcpStream>>> + Send + Sync + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, t: T) -> Self::Future {
        let Keepalive(keepalive) = self.keepalive;
        let ConnectAddr(addr) = t.param();
        debug!(server.addr = %addr, "Connecting");
        Box::pin(async move {
            let io = TcpStream::connect(&addr).await?;
            super::set_nodelay_or_warn(&io);
            super::set_keepalive_or_warn(&io, keepalive);
            debug!(
                local.addr = %io.local_addr().expect("cannot load local addr"),
                ?keepalive,
                "Connected",
            );
            Ok(io::ScopedIo::client(io))
        })
    }
}
