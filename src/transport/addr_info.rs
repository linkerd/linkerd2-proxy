use std::fmt::Debug;
use std::io;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tracing::trace;

pub trait AddrInfo: Debug {
    fn local_addr(&self) -> Result<SocketAddr, io::Error>;
    fn remote_addr(&self) -> Result<SocketAddr, io::Error>;
    fn get_original_dst(&self) -> Option<SocketAddr>;
}

impl<T: AddrInfo + ?Sized> AddrInfo for Box<T> {
    fn remote_addr(&self) -> Result<SocketAddr, io::Error> {
        self.as_ref().remote_addr()
    }

    fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.as_ref().local_addr()
    }

    fn get_original_dst(&self) -> Option<SocketAddr> {
        self.as_ref().get_original_dst()
    }
}

impl AddrInfo for TcpStream {
    fn remote_addr(&self) -> Result<SocketAddr, io::Error> {
        TcpStream::peer_addr(&self)
    }

    fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        TcpStream::local_addr(&self)
    }

    #[cfg(target_os = "linux")]
    fn get_original_dst(&self) -> Option<SocketAddr> {
        use std::os::unix::io::AsRawFd;

        let fd = self.as_raw_fd();
        let r = unsafe { linux::so_original_dst(fd) };
        r.ok()
    }

    #[cfg(not(target_os = "linux"))]
    fn get_original_dst(&self) -> Option<SocketAddr> {
        tracing::debug!("no support for SO_ORIGINAL_DST");
        None
    }
}

/// A generic way to get the original destination address of a socket.
///
/// This is especially useful to allow tests to provide a mock implementation.
pub trait GetOriginalDst {
    fn get_original_dst(&self, socket: &dyn AddrInfo) -> Option<SocketAddr>;
}

#[derive(Copy, Clone, Debug)]
pub struct SoOriginalDst;

impl GetOriginalDst for SoOriginalDst {
    fn get_original_dst(&self, sock: &dyn AddrInfo) -> Option<SocketAddr> {
        trace!("get_original_dst {:?}", sock);
        sock.get_original_dst()
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
