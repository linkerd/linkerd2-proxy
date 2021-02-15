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
use tower::util::{Oneshot, ServiceExt};
use tracing::debug;

pub trait Connect: Clone {
    type Io: io::AsyncRead + io::AsyncWrite + Send + Unpin;
    type Future: Future<Output = io::Result<Self::Io>> + Send;

    fn connect(&self, target: Target) -> Self::Future;

    fn into_service(self) -> ConnectService<Self>
    where
        Self: Sized,
    {
        ConnectService(self)
    }
}

#[derive(Copy, Clone, Debug)]
pub struct ConnectService<C>(C);

impl<S> Connect for S
where
    S: tower::Service<Target, Error = io::Error> + Clone + Send,
    S::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
    S::Future: Send,
{
    type Io = S::Response;
    type Future = Oneshot<S, Target>;

    fn connect(&self, target: Target) -> Self::Future {
        self.clone().oneshot(target)
    }
}

impl<C: Connect> tower::Service<Target> for ConnectService<C> {
    type Response = C::Io;
    type Error = io::Error;
    type Future = C::Future;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: Target) -> Self::Future {
        self.0.connect(target)
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct ConnectTcp(());

#[derive(Copy, Clone, Hash, Debug, Eq, PartialEq)]
pub struct ConnectAddr(pub SocketAddr);

#[derive(Copy, Clone, Debug)]
pub struct Target {
    pub keepalive: Keepalive,
    pub addr: ConnectAddr,
}

// === impl Target ===

impl Param<ConnectAddr> for Target {
    fn param(&self) -> ConnectAddr {
        self.addr
    }
}

impl Param<Keepalive> for Target {
    fn param(&self) -> Keepalive {
        self.keepalive
    }
}

// === impl ConnectTcp ===

impl ConnectTcp {
    pub fn new() -> Self {
        Self::default()
    }

    async fn connect(
        ConnectAddr(addr): ConnectAddr,
        Keepalive(keepalive): Keepalive,
    ) -> io::Result<io::ScopedIo<TcpStream>> {
        debug!(server.addr = %addr, "Connecting");
        let io = TcpStream::connect(&addr).await?;
        debug!(local.addr = %io.local_addr()?, ?keepalive, "Connected");
        super::set_nodelay_or_warn(&io);
        super::set_keepalive_or_warn(&io, keepalive);
        Ok(io::ScopedIo::client(io))
    }
}

impl<T> tower::Service<T> for ConnectTcp
where
    T: Param<ConnectAddr> + Param<Keepalive>,
{
    type Response = io::ScopedIo<TcpStream>;
    type Error = io::Error;
    type Future =
        Pin<Box<dyn Future<Output = io::Result<io::ScopedIo<TcpStream>>> + Send + Sync + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, t: T) -> Self::Future {
        Box::pin(Self::connect(t.param(), t.param()))
    }
}
