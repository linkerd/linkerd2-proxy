use crate::{addrs::*, Keepalive};
use futures::prelude::*;
use linkerd_stack::Param;
use std::{io, net::SocketAddr};
use tokio::net::TcpStream;
use tokio_stream::wrappers::TcpListenerStream;

/// A mockable source for address info, i.e., for tests.
pub trait GetOrigDstAddr: Clone {
    fn orig_dst_addr(&self, socket: &TcpStream) -> Option<OrigDstAddr>;
}

#[derive(Clone, Debug)]
pub struct BindTcp {
    addr: ListenAddr,
    keepalive: Keepalive,
}

#[derive(Clone, Debug)]
pub struct Addrs {
    server: Local<ServerAddr>,
    client: Remote<ClientAddr>,
    orig_dst: OrigDstAddr,
}

pub trait GetAddrs<T>: Clone {
    type Addrs: Param<Remote<ClientAddr>>;

    fn addrs(&self, conn: &T) -> io::Result<Self::Addrs>;
}

#[derive(Copy, Clone, Debug)]
pub struct NoOrigDstAddr(());

// The mock-orig-dst feature disables use of the syscall-based OrigDstAddr implementation and
// replaces it with one that must be configured.

#[cfg(not(feature = "mock-orig-dst"))]
pub use self::sys::SysOrigDstAddr as DefaultOrigDstAddr;

#[cfg(feature = "mock-orig-dst")]
pub use self::mock::MockOrigDstAddr as DefaultOrigDstAddr;

impl BindTcp {
    pub fn new(addr: ListenAddr, keepalive: Keepalive) -> Self {
        Self { addr, keepalive }
    }
}

impl BindTcp {
    pub fn addr(&self) -> ListenAddr {
        self.addr
    }

    pub fn keepalive(&self) -> Keepalive {
        self.keepalive
    }

    pub fn bind(&self) -> io::Result<(SocketAddr, impl Stream<Item = io::Result<TcpStream>>)> {
        let listen = {
            let l = std::net::TcpListener::bind(self.addr)?;
            // Ensure that O_NONBLOCK is set on the socket before using it with Tokio.
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l).expect("listener must be valid")
        };
        let addr = listen.local_addr()?;
        let keepalive = self.keepalive;

        let accept = TcpListenerStream::new(listen)
            .and_then(move |tcp| future::ready(Self::accept(tcp, keepalive)));

        Ok((addr, accept))
    }

    fn accept(tcp: TcpStream, keepalive: Keepalive) -> io::Result<TcpStream> {
        let Keepalive(keepalive) = keepalive;
        super::set_nodelay_or_warn(&tcp);
        super::set_keepalive_or_warn(&tcp, keepalive);

        Ok(tcp)
    }
}

impl Addrs {
    pub fn new(
        server: Local<ServerAddr>,
        client: Remote<ClientAddr>,
        orig_dst: OrigDstAddr,
    ) -> Self {
        Self {
            server,
            client,
            orig_dst,
        }
    }

    pub fn server(&self) -> Local<ServerAddr> {
        self.server
    }

    pub fn client(&self) -> Remote<ClientAddr> {
        self.client
    }

    pub fn orig_dst(&self) -> OrigDstAddr {
        self.orig_dst
    }
}

impl GetOrigDstAddr for NoOrigDstAddr {
    fn orig_dst_addr(&self, _: &TcpStream) -> Option<OrigDstAddr> {
        None
    }
}

impl Param<OrigDstAddr> for Addrs {
    #[inline]
    fn param(&self) -> OrigDstAddr {
        self.orig_dst()
    }
}

impl Param<Remote<ClientAddr>> for Addrs {
    #[inline]
    fn param(&self) -> Remote<ClientAddr> {
        self.client()
    }
}

impl Param<Local<ServerAddr>> for Addrs {
    #[inline]
    fn param(&self) -> Local<ServerAddr> {
        self.server()
    }
}

mod sys {
    use super::{
        io, Addrs, ClientAddr, GetAddrs, Local, OrigDstAddr, Remote, ServerAddr, TcpStream,
    };

    #[derive(Copy, Clone, Debug, Default)]
    pub struct SysOrigDstAddr(());

    impl SysOrigDstAddr {
        #[cfg(target_os = "linux")]
        fn orig_dst_addr(&self, sock: &TcpStream) -> io::Result<OrigDstAddr> {
            use std::os::unix::io::AsRawFd;

            let fd = sock.as_raw_fd();
            let r = unsafe { linux::so_original_dst(fd) };
            r.map(OrigDstAddr)
        }

        #[cfg(not(target_os = "linux"))]
        fn orig_dst_addr(&self, _: &TcpStream) -> io::Result<OrigDstAddr> {
            io::Error::new(
                io::ErrorKind::Other,
                "SO_ORIGINAL_DST not supported on this operating system",
            )
        }
    }

    impl GetAddrs<TcpStream> for SysOrigDstAddr {
        type Addrs = Addrs;
        fn addrs(&self, tcp: &TcpStream) -> io::Result<Self::Addrs> {
            let server = Local(ServerAddr(tcp.local_addr()?));
            let client = Remote(ClientAddr(tcp.peer_addr()?));
            let orig_dst = self.orig_dst_addr(tcp)?;
            tracing::trace!(
                server.addr = %server,
                client.addr = %client,
                orig.addr = ?orig_dst,
                "Accepted",
            );
            Ok(Addrs::new(server, client, orig_dst))
        }
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
        use std::os::unix::io::RawFd;
        use std::{io, mem};
        use tracing::warn;

        pub unsafe fn so_original_dst(fd: RawFd) -> io::Result<SocketAddr> {
            let mut sockaddr: libc::sockaddr_storage = mem::zeroed();
            let mut socklen: libc::socklen_t = mem::size_of::<libc::sockaddr_storage>() as u32;

            let ret = libc::getsockopt(
                fd,
                libc::SOL_IP,
                libc::SO_ORIGINAL_DST,
                &mut sockaddr as *mut _ as *mut _,
                &mut socklen as *mut _ as *mut _,
            );
            if ret != 0 {
                let e = io::Error::last_os_error();
                warn!("failed to read SO_ORIGINAL_DST: {:?}", e);
                return Err(e);
            }

            mk_addr(&sockaddr, socklen)
        }

        // Borrowed with love from net2-rs
        // https://github.com/rust-lang-nursery/net2-rs/blob/1b4cb4fb05fbad750b271f38221eab583b666e5e/src/socket.rs#L103
        fn mk_addr(
            storage: &libc::sockaddr_storage,
            len: libc::socklen_t,
        ) -> io::Result<SocketAddr> {
            match storage.ss_family as libc::c_int {
                libc::AF_INET => {
                    assert!(len as usize >= mem::size_of::<libc::sockaddr_in>());

                    let sa = {
                        let sa = storage as *const _ as *const libc::sockaddr_in;
                        unsafe { *sa }
                    };

                    let bits = ntoh32(sa.sin_addr.s_addr);
                    let ip = Ipv4Addr::new(
                        (bits >> 24) as u8,
                        (bits >> 16) as u8,
                        (bits >> 8) as u8,
                        bits as u8,
                    );
                    let port = sa.sin_port;
                    Ok(SocketAddr::V4(SocketAddrV4::new(ip, ntoh16(port))))
                }
                libc::AF_INET6 => {
                    assert!(len as usize >= mem::size_of::<libc::sockaddr_in6>());

                    let sa = {
                        let sa = storage as *const _ as *const libc::sockaddr_in6;
                        unsafe { *sa }
                    };

                    let arr = sa.sin6_addr.s6_addr;
                    let ip = Ipv6Addr::new(
                        (arr[0] as u16) << 8 | (arr[1] as u16),
                        (arr[2] as u16) << 8 | (arr[3] as u16),
                        (arr[4] as u16) << 8 | (arr[5] as u16),
                        (arr[6] as u16) << 8 | (arr[7] as u16),
                        (arr[8] as u16) << 8 | (arr[9] as u16),
                        (arr[10] as u16) << 8 | (arr[11] as u16),
                        (arr[12] as u16) << 8 | (arr[13] as u16),
                        (arr[14] as u16) << 8 | (arr[15] as u16),
                    );

                    let port = sa.sin6_port;
                    let flowinfo = sa.sin6_flowinfo;
                    let scope_id = sa.sin6_scope_id;
                    Ok(SocketAddr::V6(SocketAddrV6::new(
                        ip,
                        ntoh16(port),
                        flowinfo,
                        scope_id,
                    )))
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid argument",
                )),
            }
        }

        fn ntoh16(i: u16) -> u16 {
            <u16>::from_be(i)
        }

        fn ntoh32(i: u32) -> u32 {
            <u32>::from_be(i)
        }
    }
}

#[cfg(feature = "mock-orig-dst")]
mod mock {
    use super::{GetOrigDstAddr, OrigDstAddr, SocketAddr, TcpStream};

    #[derive(Copy, Clone, Debug)]
    pub struct MockOrigDstAddr(SocketAddr);

    impl From<SocketAddr> for MockOrigDstAddr {
        fn from(addr: SocketAddr) -> Self {
            MockOrigDstAddr(addr)
        }
    }

    impl GetOrigDstAddr for MockOrigDstAddr {
        fn orig_dst_addr(&self, _: &TcpStream) -> Option<OrigDstAddr> {
            Some(OrigDstAddr(self.0))
        }
    }
}
