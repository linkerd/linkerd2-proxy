mod dual_bind;

use crate::{addrs::*, Keepalive, UserTimeout};
use dual_bind::DualBind;
use futures::prelude::*;
use linkerd_error::Result;
use linkerd_io as io;
use linkerd_stack::Param;
use std::{fmt, net::SocketAddr, pin::Pin};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_stream::wrappers::TcpListenerStream;

/// Binds a listener, producing a stream of incoming connections.
///
/// Typically, this represents binding a TCP socket. However, it may also be an
/// stream of in-memory mock connections, for testing purposes.
pub trait Bind<T> {
    type Io: io::AsyncRead
        + io::AsyncWrite
        + io::Peek
        + io::PeerAddr
        + fmt::Debug
        + Unpin
        + Send
        + Sync
        + 'static;
    type Addrs: Clone + Send + Sync + 'static;
    type BoundAddrs;
    type Incoming: Stream<Item = Result<(Self::Addrs, Self::Io)>> + Send + Sync + 'static;

    fn bind(self, params: &T) -> Result<(Self::BoundAddrs, Self::Incoming)>;
}

#[derive(Copy, Clone, Debug, Default)]
pub struct BindTcp(());

#[derive(Clone, Debug)]
pub struct Addrs {
    pub server: Local<ServerAddr>,
    pub client: Remote<ClientAddr>,
}

#[derive(Debug, Error)]
#[error("failed to accept socket: {0}")]
struct AcceptError(#[source] io::Error);

#[derive(Debug, Error)]
#[error("failed to set TCP keepalive: {0}")]
struct KeepaliveError(#[source] io::Error);

#[derive(Debug, Error)]
#[error("failed to set TCP User Timeout: {0}")]
struct UserTimeoutError(#[source] io::Error);

#[derive(Debug, Error)]
#[error("failed to obtain peer address: {0}")]
struct PeerAddrError(#[source] io::Error);

// === impl BindTcp ===

impl BindTcp {
    pub fn with_orig_dst() -> super::BindWithOrigDst<Self> {
        super::BindWithOrigDst::from(Self::default())
    }

    pub fn dual_with_orig_dst() -> DualBind<super::BindWithOrigDst<Self>> {
        DualBind::from(super::BindWithOrigDst::default())
    }
}

impl<T> Bind<T> for BindTcp
where
    T: Param<ListenAddr> + Param<Keepalive> + Param<UserTimeout>,
{
    type Addrs = Addrs;
    type BoundAddrs = Local<ServerAddr>;
    type Incoming = Pin<Box<dyn Stream<Item = Result<(Self::Addrs, Self::Io)>> + Send + Sync>>;
    type Io = TcpStream;

    fn bind(self, params: &T) -> Result<(Self::BoundAddrs, Self::Incoming)> {
        let listen = {
            let ListenAddr(addr) = params.param();
            let l = std::net::TcpListener::bind(addr)?;
            // Ensure that O_NONBLOCK is set on the socket before using it with Tokio.
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l).expect("listener must be valid")
        };
        let server = Local(ServerAddr(listen.local_addr()?));
        let Keepalive(keepalive) = params.param();
        let UserTimeout(user_timeout) = params.param();
        let accept = TcpListenerStream::new(listen).map(move |res| {
            let tcp = res.map_err(AcceptError)?;
            super::set_nodelay_or_warn(&tcp);
            let tcp = super::set_keepalive_or_warn(tcp, keepalive).map_err(KeepaliveError)?;
            let tcp =
                super::set_user_timeout_or_warn(tcp, user_timeout).map_err(UserTimeoutError)?;

            fn ipv4_mapped(orig: SocketAddr) -> SocketAddr {
                if let SocketAddr::V6(v6) = orig {
                    if let Some(ip) = v6.ip().to_ipv4_mapped() {
                        return (ip, orig.port()).into();
                    }
                }
                orig
            }

            let client_addr = tcp.peer_addr().map_err(PeerAddrError)?;
            let client = Remote(ClientAddr(ipv4_mapped(client_addr)));
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

impl Param<AddrPair> for Addrs {
    #[inline]
    fn param(&self) -> AddrPair {
        let Remote(client) = self.client;
        let Local(server) = self.server;
        AddrPair(client, server)
    }
}
