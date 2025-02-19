use crate::{ClientAddr, Keepalive, Local, Remote, ServerAddr, UserTimeout};
use linkerd_io as io;
use linkerd_stack::{Param, Service};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::debug;

#[derive(Copy, Clone, Debug)]
pub struct ConnectTcp {
    keepalive: Keepalive,
    user_timeout: UserTimeout,
}

type TcpStream = io::ScopedIo<hyper_util::rt::TokioIo<tokio::net::TcpStream>>;

impl ConnectTcp {
    pub fn new(keepalive: Keepalive, user_timeout: UserTimeout) -> Self {
        Self {
            keepalive,
            user_timeout,
        }
    }
}

impl<T: Param<Remote<ServerAddr>>> Service<T> for ConnectTcp {
    type Response = (TcpStream, Local<ClientAddr>);
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send + Sync + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, t: T) -> Self::Future {
        let Keepalive(keepalive) = self.keepalive;
        let UserTimeout(user_timeout) = self.user_timeout;
        let Remote(ServerAddr(addr)) = t.param();
        debug!(server.addr = %addr, "Connecting");
        Box::pin(async move {
            let io = tokio::net::TcpStream::connect(&addr).await?;
            super::set_nodelay_or_warn(&io);
            let io = super::set_keepalive_or_warn(io, keepalive)?;
            let io = super::set_user_timeout_or_warn(io, user_timeout)?;
            let local_addr = io.local_addr()?;
            debug!(
                local.addr = %local_addr,
                ?keepalive,
                "Connected",
            );
            Ok((
                io::ScopedIo::client(hyper_util::rt::TokioIo::new(io)),
                Local(ClientAddr(local_addr)),
            ))
        })
    }
}
