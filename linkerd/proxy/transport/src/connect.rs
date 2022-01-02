use crate::{ClientAddr, Keepalive, Local, Remote, ServerAddr};
use linkerd_io as io;
use linkerd_stack::{Param, Service};
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

impl<T: Param<Remote<ServerAddr>>> Service<T> for ConnectTcp {
    type Response = (io::ScopedIo<TcpStream>, Local<ClientAddr>);
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send + Sync + 'static>>;

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
            let io = super::set_keepalive_or_warn(io, keepalive)?;
            let local_addr = io.local_addr()?;
            debug!(
                local.addr = %local_addr,
                ?keepalive,
                "Connected",
            );
            Ok((io::ScopedIo::client(io), Local(ClientAddr(local_addr))))
        })
    }
}
