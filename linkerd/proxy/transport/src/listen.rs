use futures::{try_ready, Poll};
use linkerd2_proxy_core::listen;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::reactor;

/// A mockable source for address info, i.e., for tests.
pub trait OrigDstAddr: Clone {
    fn orig_dst_addr(&self, socket: &TcpStream) -> Option<SocketAddr>;
}

#[derive(Clone, Debug)]
pub struct Bind<O: OrigDstAddr = NoOrigDstAddr> {
    bind_addr: SocketAddr,
    keepalive: Option<Duration>,
    orig_dst_addr: O,
}

#[derive(Debug)]
pub struct Listen<O: OrigDstAddr = NoOrigDstAddr> {
    listen_addr: SocketAddr,
    keepalive: Option<Duration>,
    orig_dst_addr: O,
    state: State,
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

#[derive(Copy, Clone, Debug)]
pub struct SysOrigDstAddr(());

#[derive(Debug)]
enum State {
    Init(Option<std::net::TcpListener>),
    Bound(tokio::net::TcpListener),
}

impl Bind {
    pub fn new(bind_addr: SocketAddr, keepalive: Option<Duration>) -> Self {
        Self {
            bind_addr,
            keepalive,
            orig_dst_addr: NoOrigDstAddr(()),
        }
    }
}

impl<O: OrigDstAddr> Bind<O> {
    pub fn with_orig_dst_addrs_from<P: OrigDstAddr>(self, orig_dst_addr: P) -> Bind<P> {
        Bind {
            orig_dst_addr,
            bind_addr: self.bind_addr,
            keepalive: self.keepalive,
        }
    }

    pub fn with_orig_dst_addrs_from_system(self) -> Bind<SysOrigDstAddr> {
        self.with_orig_dst_addrs_from(SysOrigDstAddr(()))
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }

    pub fn keepalive(&self) -> Option<Duration> {
        self.keepalive
    }
}

impl<O: OrigDstAddr> listen::Bind for Bind<O> {
    type Connection = Connection;
    type Listen = Listen<O>;

    fn bind(self) -> std::io::Result<Listen<O>> {
        let tcp = std::net::TcpListener::bind(self.bind_addr)?;
        let listen_addr = tcp.local_addr()?;
        Ok(Listen {
            listen_addr,
            keepalive: self.keepalive,
            orig_dst_addr: self.orig_dst_addr,
            state: State::Init(Some(tcp)),
        })
    }
}

impl<O> listen::Listen for Listen<O>
where
    O: OrigDstAddr,
{
    type Connection = Connection;
    type Error = std::io::Error;

    fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    fn poll_accept(&mut self) -> Poll<Self::Connection, Self::Error> {
        loop {
            self.state = match self.state {
                State::Init(ref mut std) => {
                    // Create the TCP listener lazily, so that it's not bound to a
                    // reactor until the future is run. This will avoid
                    // `Handle::current()` creating a new thread for the global
                    // background reactor if `polled before the runtime is
                    // initialized.
                    tracing::trace!("listening on {}", self.listen_addr);
                    let listener = tokio::net::TcpListener::from_std(
                        std.take().expect("illegal state"),
                        &reactor::Handle::current(),
                    )?;
                    State::Bound(listener)
                }
                State::Bound(ref mut listener) => {
                    tracing::trace!("accepting on {}", self.listen_addr);
                    let (tcp, peer_addr) = try_ready!(listener.poll_accept());
                    // TODO: On Linux and most other platforms it would be better
                    // to set the `TCP_NODELAY` option on the bound socket and
                    // then have the listening sockets inherit it. However, that
                    // doesn't work on all platforms and also the underlying
                    // libraries don't have the necessary API for that, so just
                    // do it here.
                    super::set_nodelay_or_warn(&tcp);
                    super::set_keepalive_or_warn(&tcp, self.keepalive);

                    let addrs = Addrs::new(
                        tcp.local_addr()?,
                        peer_addr,
                        self.orig_dst_addr.orig_dst_addr(&tcp),
                    );

                    return Ok((addrs, tcp).into());
                }
            };
        }
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

impl OrigDstAddr for NoOrigDstAddr {
    fn orig_dst_addr(&self, _: &TcpStream) -> Option<SocketAddr> {
        None
    }
}

impl OrigDstAddr for SysOrigDstAddr {
    #[cfg(target_os = "linux")]
    fn orig_dst_addr(&self, sock: &TcpStream) -> Option<SocketAddr> {
        use std::os::unix::io::AsRawFd;

        let fd = sock.as_raw_fd();
        let r = unsafe { linux::so_original_dst(fd) };
        r.ok()
    }

    #[cfg(not(target_os = "linux"))]
    fn orig_dst_addr(&self, sock: &TcpStream) -> Option<SocketAddr> {
        None
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use libc;
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
