use crate::{
    addrs::*,
    listen::{self, Bind},
};
use futures::prelude::*;
use linkerd_error::Result;
use linkerd_io as io;
use linkerd_stack::Param;
use std::{net::SocketAddr, pin::Pin};
use tokio::net::TcpStream;

#[derive(Copy, Clone, Debug, Default)]
pub struct BindWithOrigDst<B = listen::BindTcp> {
    inner: B,
}

#[derive(Clone, Debug)]
pub struct Addrs<A = listen::Addrs> {
    pub inner: A,
    pub orig_dst: OrigDstAddr,
}

// === impl Addrs ===

impl<A> Param<OrigDstAddr> for Addrs<A> {
    #[inline]
    fn param(&self) -> OrigDstAddr {
        self.orig_dst
    }
}

impl<A> Param<Remote<ClientAddr>> for Addrs<A>
where
    A: Param<Remote<ClientAddr>>,
{
    #[inline]
    fn param(&self) -> Remote<ClientAddr> {
        self.inner.param()
    }
}

impl<A> Param<AddrPair> for Addrs<A>
where
    A: Param<Remote<ClientAddr>>,
{
    #[inline]
    fn param(&self) -> AddrPair {
        let Remote(client) = self.inner.param();
        AddrPair(client, ServerAddr(self.orig_dst.into()))
    }
}

impl<A> Param<Local<ServerAddr>> for Addrs<A>
where
    A: Param<Local<ServerAddr>>,
{
    fn param(&self) -> Local<ServerAddr> {
        self.inner.param()
    }
}

// === impl WithOrigDst ===

impl<B> From<B> for BindWithOrigDst<B> {
    fn from(inner: B) -> Self {
        Self { inner }
    }
}

impl<T, B> Bind<T> for BindWithOrigDst<B>
where
    B: Bind<T, Io = TcpStream> + 'static,
    B::Addrs: Param<Remote<ClientAddr>>,
{
    type Addrs = Addrs<B::Addrs>;
    type BoundAddrs = B::BoundAddrs;
    type Io = TcpStream;
    type Incoming =
        Pin<Box<dyn Stream<Item = Result<(Self::Addrs, TcpStream)>> + Send + Sync + 'static>>;

    fn bind(self, t: &T) -> Result<(Self::BoundAddrs, Self::Incoming)> {
        let (addr, incoming) = self.inner.bind(t)?;

        let incoming = incoming.map(|res| {
            let (inner, tcp) = res?;

            let sock = {
                let stream = tokio::net::TcpStream::into_std(tcp)?;
                socket2::Socket::from(stream)
            };

            let orig_dst = match inner.param() {
                // IPv4-mapped IPv6 addresses are unwrapped by BindTcp::bind() and received here as
                // SocketAddr::V4. We must call getsockopt with IPv4 constants (via
                // orig_dst_addr_v4) even if it originally was an IPv6
                Remote(ClientAddr(SocketAddr::V4(_))) => sock.original_dst()?,
                Remote(ClientAddr(SocketAddr::V6(_))) => sock.original_dst_ipv6()?,
            };

            let orig_dst = orig_dst.as_socket().ok_or(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid address format",
            ))?;

            let stream: std::net::TcpStream = socket2::Socket::into(sock);
            let stream = tokio::net::TcpStream::from_std(stream)?;
            let orig_dst = OrigDstAddr(orig_dst);
            let addrs = Addrs { inner, orig_dst };
            Ok((addrs, stream))
        });

        Ok((addr, Box::pin(incoming)))
    }
}
