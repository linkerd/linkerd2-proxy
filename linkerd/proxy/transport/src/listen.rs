use crate::{addrs::*, Keepalive};
use futures::prelude::*;
use linkerd_stack::Param;
use std::{io, pin::Pin};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_stream::wrappers::TcpListenerStream;

/// Binds a listener, producing a stream of incoming connections.
///
/// Typically, this represents binding a TCP socket. However, it may also be an
/// stream of in-memory mock connections, for testing purposes.
pub trait Bind<T> {
    type Io: AsyncRead + AsyncWrite + Send + Sync + 'static;
    type Addrs: Send + Sync + 'static;
    type Incoming: Stream<Item = io::Result<(Self::Addrs, Self::Io)>> + Send + Sync + 'static;

    fn bind(self, params: &T) -> io::Result<Bound<Self::Incoming>>;
}

pub type Bound<I> = (Local<ServerAddr>, I);

#[derive(Copy, Clone, Debug, Default)]
pub struct BindTcp(());

#[derive(Clone, Debug)]
pub struct Addrs {
    pub server: Local<ServerAddr>,
    pub client: Remote<ClientAddr>,
}

// === impl BindTcp ===

impl BindTcp {
    pub fn with_orig_dst() -> super::BindWithOrigDst<Self> {
        super::BindWithOrigDst::from(Self::default())
    }
}

impl<T> Bind<T> for BindTcp
where
    T: Param<ListenAddr> + Param<Keepalive>,
{
    type Addrs = Addrs;
    type Incoming = Pin<Box<dyn Stream<Item = io::Result<(Self::Addrs, Self::Io)>> + Send + Sync>>;
    type Io = TcpStream;

    fn bind(self, params: &T) -> io::Result<Bound<Self::Incoming>> {
        let listen = {
            let ListenAddr(addr) = params.param();
            let l = std::net::TcpListener::bind(addr)?;
            // Ensure that O_NONBLOCK is set on the socket before using it with Tokio.
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l).expect("listener must be valid")
        };
        let server = Local(ServerAddr(listen.local_addr()?));
        let Keepalive(keepalive) = params.param();
        let accept = TcpListenerStream::new(listen).map(move |res| {
            let tcp = res?;
            super::set_nodelay_or_warn(&tcp);
            super::set_keepalive_or_warn(&tcp, keepalive);
            let client = Remote(ClientAddr(tcp.peer_addr()?));
            Ok((Addrs { server, client }, tcp))
        });

        Ok((server, Box::pin(accept)))
    }
}

// === impl Addrs ===

impl Param<Remote<ClientAddr>> for Addrs {
    #[inline]
    fn param(&self) -> Remote<ClientAddr> {
        self.client
    }
}

impl Param<Local<ServerAddr>> for Addrs {
    #[inline]
    fn param(&self) -> Local<ServerAddr> {
        self.server
    }
}
