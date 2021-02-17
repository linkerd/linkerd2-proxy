use crate::Keepalive;
use futures::prelude::*;
use std::{io, net::SocketAddr};
use tokio::net::TcpStream;
use tokio_stream::wrappers::TcpListenerStream;
use tracing::trace;

/// A mockable source for address info, i.e., for tests.
pub trait OrigDstAddr: Clone {
    fn orig_dst_addr(&self, socket: &TcpStream) -> Option<SocketAddr>;
}

#[derive(Clone, Debug)]
pub struct BindTcp<O: OrigDstAddr = NoOrigDstAddr> {
    bind_addr: SocketAddr,
    keepalive: Keepalive,
    orig_dst_addr: O,
}

pub type Connection = (Addrs, TcpStream);

#[derive(Clone, Debug)]
pub struct Addrs {
    local: SocketAddr,
    peer: SocketAddr,
    orig_dst: Option<SocketAddr>,
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
    pub fn new(bind_addr: SocketAddr, keepalive: Keepalive) -> Self {
        Self {
            bind_addr,
            keepalive,
            orig_dst_addr: NoOrigDstAddr(()),
        }
    }
}

impl<A: OrigDstAddr> BindTcp<A> {
    pub fn with_orig_dst_addr<B: OrigDstAddr>(self, orig_dst_addr: B) -> BindTcp<B> {
        BindTcp {
            orig_dst_addr,
            bind_addr: self.bind_addr,
            keepalive: self.keepalive,
        }
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }

    pub fn keepalive(&self) -> Keepalive {
        self.keepalive
    }

    pub fn bind(&self) -> io::Result<(SocketAddr, impl Stream<Item = io::Result<Connection>>)> {
        let listen = {
            let l = std::net::TcpListener::bind(self.bind_addr)?;
            // Ensure that O_NONBLOCK is set on the socket before using it with Tokio.
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l).expect("listener must be valid")
        };
        let addr = listen.local_addr()?;
        let keepalive = self.keepalive;
        let get_orig = self.orig_dst_addr.clone();

        let accept = TcpListenerStream::new(listen)
            .and_then(move |tcp| future::ready(Self::accept(tcp, keepalive, get_orig.clone())));

        Ok((addr, accept))
    }

    fn accept(tcp: TcpStream, keepalive: Keepalive, get_orig: A) -> io::Result<Connection> {
        let addrs = {
            let local = tcp.local_addr()?;
            let peer = tcp.peer_addr()?;
            let orig_dst = get_orig.orig_dst_addr(&tcp);
            trace!(
                local.addr = %local,
                peer.addr = %peer,
                orig.addr = ?orig_dst,
                "Accepted",
            );
            Addrs::new(local, peer, orig_dst)
        };

        let Keepalive(keepalive) = keepalive;
        super::set_nodelay_or_warn(&tcp);
        super::set_keepalive_or_warn(&tcp, keepalive);

        Ok((addrs, tcp))
    }
}

impl Addrs {
    pub fn new(local: SocketAddr, peer: SocketAddr, orig_dst: Option<SocketAddr>) -> Self {
        Self {
            local,
            peer,
            orig_dst,
        }
    }

    pub fn local(&self) -> SocketAddr {
        self.local
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn orig_dst(&self) -> Option<SocketAddr> {
        self.orig_dst
    }

    pub fn target_addr(&self) -> SocketAddr {
        self.orig_dst.unwrap_or(self.local)
    }

    pub fn target_addr_is_local(&self) -> bool {
        self.orig_dst
            .map(|orig_dst| Self::same_addr(orig_dst, self.local))
            .unwrap_or(true)
    }

    pub fn target_addr_if_not_local(&self) -> Option<SocketAddr> {
        if !self.target_addr_is_local() {
            Some(self.target_addr())
        } else {
            None
        }
    }

    fn same_addr(a0: SocketAddr, a1: SocketAddr) -> bool {
        use std::net::IpAddr::{V4, V6};
        (a0.port() == a1.port())
            && match (a0.ip(), a1.ip()) {
                (V6(a0), V4(a1)) => a0.to_ipv4() == Some(a1),
                (V4(a0), V6(a1)) => Some(a0) == a1.to_ipv4(),
                (a0, a1) => (a0 == a1),
            }
    }
}

impl Into<SocketAddr> for &'_ Addrs {
    fn into(self) -> SocketAddr {
        self.target_addr()
    }
}

impl OrigDstAddr for NoOrigDstAddr {
    fn orig_dst_addr(&self, _: &TcpStream) -> Option<SocketAddr> {
        None
    }
}

#[cfg(not(feature = "mock-orig-dst"))]
mod sys {
    use super::{OrigDstAddr, SocketAddr, TcpStream};

    #[derive(Copy, Clone, Debug, Default)]
    pub struct SysOrigDstAddr(());

    impl OrigDstAddr for SysOrigDstAddr {
        #[cfg(target_os = "linux")]
        fn orig_dst_addr(&self, sock: &TcpStream) -> Option<SocketAddr> {
            use std::os::unix::io::AsRawFd;

            let fd = sock.as_raw_fd();
            let r = unsafe { linux::so_original_dst(fd) };
            r.ok()
        }

        #[cfg(not(target_os = "linux"))]
        fn orig_dst_addr(&self, _sock: &TcpStream) -> Option<SocketAddr> {
            None
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
    use super::{OrigDstAddr, SocketAddr, TcpStream};

    #[derive(Copy, Clone, Debug)]
    pub struct MockOrigDstAddr(SocketAddr);

    impl From<SocketAddr> for MockOrigDstAddr {
        fn from(addr: SocketAddr) -> Self {
            MockOrigDstAddr(addr)
        }
    }

    impl OrigDstAddr for MockOrigDstAddr {
        fn orig_dst_addr(&self, _: &TcpStream) -> Option<SocketAddr> {
            Some(self.0)
        }
    }
}
