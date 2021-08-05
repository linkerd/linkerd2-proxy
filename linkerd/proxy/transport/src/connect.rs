use crate::{Keepalive, Remote, ServerAddr};
use linkerd_io as io;
use linkerd_stack::Param;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::net::TcpStream;
use tracing::debug;

#[derive(Copy, Clone, Debug)]
pub struct ConnectTcp {
    keepalive: Keepalive,
}

impl ConnectTcp {
    pub fn new(keepalive: Keepalive) -> Self {
        Self { keepalive }
    }
}

impl<T: Param<Remote<ServerAddr>>> tower::Service<T> for ConnectTcp {
    type Response = io::ScopedIo<TcpStream>;
    type Error = io::Error;
    type Future =
        Pin<Box<dyn Future<Output = io::Result<io::ScopedIo<TcpStream>>> + Send + Sync + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, t: T) -> Self::Future {
        let Keepalive(keepalive) = self.keepalive;
        let Remote(ServerAddr(addr)) = t.param();
        debug!(server.addr = %addr, "Connecting");
        Box::pin(async move {
            let io = TcpStream::connect(&addr).await?;
            super::set_nodelay_or_warn(&io);
            let io = super::set_keepalive(io, keepalive)?;
            debug!(
                local.addr = %io.local_addr().expect("cannot load local addr"),
                ?keepalive,
                "Connected",
            );
            Ok(io::ScopedIo::client(io))
        })
    }
}
