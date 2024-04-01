use crate::{
    addrs::*,
    listen::{self, Bind, Bound},
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
    type Io = TcpStream;
    type Incoming =
        Pin<Box<dyn Stream<Item = Result<(Self::Addrs, TcpStream)>> + Send + Sync + 'static>>;

    fn bind(self, t: &T) -> Result<Bound<Self::Incoming>> {
        let (addr, incoming) = self.inner.bind(t)?;

        let incoming = incoming.map(|res| {
            let (inner, tcp) = res?;
            let is_ipv4 = match inner.param() {
                // used when binding to 0.0.0.0
                Remote(ClientAddr(SocketAddr::V4(_))) => true,
                // used when binding to ::
                Remote(ClientAddr(SocketAddr::V6(a))) => a.ip().to_ipv4_mapped().is_some(),
            };
            let orig_dst = if is_ipv4 {
                orig_dst_addr_v4(&tcp)?
            } else {
                orig_dst_addr_v6(&tcp)?
            };
            let orig_dst = OrigDstAddr(orig_dst);
            let addrs = Addrs { inner, orig_dst };
            Ok((addrs, tcp))
        });

        Ok((addr, Box::pin(incoming)))
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn orig_dst_addr_v4(sock: &TcpStream) -> io::Result<SocketAddr> {
    use std::os::unix::io::AsRawFd;

    let fd = sock.as_raw_fd();
    unsafe { linux::so_original_dst_v4(fd) }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn orig_dst_addr_v6(sock: &TcpStream) -> io::Result<SocketAddr> {
    use std::os::unix::io::AsRawFd;

    let fd = sock.as_raw_fd();
    unsafe { linux::so_original_dst_v6(fd) }
}

#[cfg(not(target_os = "linux"))]
fn orig_dst_addr_v4(_: &TcpStream) -> io::Result<OrigDstAddr> {
    Err(io::Error::new(
        io::ErrorKind::Other,
        "SO_ORIGINAL_DST not supported on this operating system",
    ))
}

#[cfg(not(target_os = "linux"))]
fn orig_dst_addr_v6(_: &TcpStream) -> io::Result<OrigDstAddr> {
    Err(io::Error::new(
        io::ErrorKind::Other,
        "SO_ORIGINAL_DST not supported on this operating system",
    ))
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
    use std::os::unix::io::RawFd;
    use std::{io, mem};

    pub unsafe fn so_original_dst(fd: RawFd, level: i32, optname: i32) -> io::Result<SocketAddr> {
        let mut sockaddr: libc::sockaddr_storage = mem::zeroed();
        let mut sockaddr_len: libc::socklen_t = mem::size_of::<libc::sockaddr_storage>() as u32;

        let ret = libc::getsockopt(
            fd,
            level,
            optname,
            &mut sockaddr as *mut _ as *mut _,
            &mut sockaddr_len as *mut _ as *mut _,
        );
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }

        mk_addr(&sockaddr, sockaddr_len)
    }

    pub unsafe fn so_original_dst_v4(fd: RawFd) -> io::Result<SocketAddr> {
        so_original_dst(fd, libc::SOL_IP, libc::SO_ORIGINAL_DST)
    }

    pub unsafe fn so_original_dst_v6(fd: RawFd) -> io::Result<SocketAddr> {
        so_original_dst(fd, libc::SOL_IPV6, libc::IP6T_SO_ORIGINAL_DST)
    }

    // Borrowed with love from net2-rs
    // https://github.com/rust-lang-nursery/net2-rs/blob/1b4cb4fb05fbad750b271f38221eab583b666e5e/src/socket.rs#L103
    //
    // Copyright (c) 2014 The Rust Project Developers
    fn mk_addr(storage: &libc::sockaddr_storage, len: libc::socklen_t) -> io::Result<SocketAddr> {
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
