mod dual_bind;

use crate::{addrs::*, Keepalive, UserTimeout, Backlog};
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
    T: Param<ListenAddr> + Param<Keepalive> + Param<UserTimeout> + Param<Backlog>,
{
    type Addrs = Addrs;
    type BoundAddrs = Local<ServerAddr>;
    type Incoming = Pin<Box<dyn Stream<Item = Result<(Self::Addrs, Self::Io)>> + Send + Sync>>;
    type Io = TcpStream;

    fn bind(self, params: &T) -> Result<(Self::BoundAddrs, Self::Incoming)> {
        let listen = {
            let ListenAddr(addr) = params.param();
            let Backlog(backlog) = params.param();

            // Use TcpSocket to create and configure the listener socket before binding.
            // This allows us to set socket options and configure the listen backlog.
            // TcpSocket::new_v4/v6 automatically sets O_NONBLOCK, which is required
            // for Tokio's async I/O operations.
            let socket = if addr.is_ipv4() {
                tokio::net::TcpSocket::new_v4()?
            } else {
                tokio::net::TcpSocket::new_v6()?
            };

            // Enable SO_REUSEADDR to match std::net::TcpListener::bind behavior.
            // This allows the server to bind to an address in TIME_WAIT state,
            // enabling faster restarts without "address already in use" errors.
            socket.set_reuseaddr(true)?;

            socket.bind(addr)?;
            socket.listen(backlog)?
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct TestParams {
        addr: ListenAddr,
        keepalive: Keepalive,
        user_timeout: UserTimeout,
        backlog: crate::Backlog,
    }

    impl Param<ListenAddr> for TestParams {
        fn param(&self) -> ListenAddr {
            self.addr
        }
    }

    impl Param<Keepalive> for TestParams {
        fn param(&self) -> Keepalive {
            self.keepalive
        }
    }

    impl Param<UserTimeout> for TestParams {
        fn param(&self) -> UserTimeout {
            self.user_timeout
        }
    }

    impl Param<crate::Backlog> for TestParams {
        fn param(&self) -> crate::Backlog {
            self.backlog
        }
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn listener_is_nonblocking() {
        use std::os::unix::io::AsRawFd;

        let params = TestParams {
            addr: ListenAddr("127.0.0.1:0".parse().unwrap()),
            keepalive: Keepalive(None),
            user_timeout: UserTimeout(None),
            backlog: crate::Backlog(1024),
        };

        let bind = BindTcp::default();
        let (bound_addr, _incoming) = bind.bind(&params).expect("failed to bind");

        // Verify the socket is non-blocking by checking the O_NONBLOCK flag.
        // Create a new listener to the bound address to check its flags.
        let listener = tokio::net::TcpListener::bind(bound_addr.0 .0)
            .await
            .expect("failed to bind test listener");

        let fd = listener.as_raw_fd();
        // Safety: We're just reading the file descriptor flags with fcntl(F_GETFL),
        // which is safe as long as the fd is valid (guaranteed by the listener being alive).
        #[allow(unsafe_code)]
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(flags, -1, "fcntl failed to get flags");
        assert_ne!(
            flags & libc::O_NONBLOCK,
            0,
            "O_NONBLOCK flag is not set on the listener socket"
        );
    }
}
