use std::io;
use std::net::SocketAddr;
use tokio::net::TcpStream;

pub trait AddrInfo {
    fn local_addr(&self) -> io::Result<SocketAddr>;
    fn peer_addr(&self) -> io::Result<SocketAddr>;
    fn orig_dst_addr(&self) -> io::Result<SocketAddr>;
}

impl<T: AddrInfo + ?Sized> AddrInfo for Box<T> {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.as_ref().peer_addr()
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.as_ref().local_addr()
    }

    fn orig_dst_addr(&self) -> io::Result<SocketAddr> {
        self.as_ref().orig_dst_addr()
    }
}

impl AddrInfo for TcpStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        TcpStream::peer_addr(&self)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        TcpStream::local_addr(&self)
    }

    #[cfg(target_os = "linux")]
    fn orig_dst_addr(&self) -> io::Result<SocketAddr> {
        use std::os::unix::io::AsRawFd;

        let fd = self.as_raw_fd();
        let r = unsafe { linux::so_original_dst(fd) };
        r
    }

    #[cfg(not(target_os = "linux"))]
    fn orig_dst_addr(&self) -> io::Result<SocketAddr> {
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "platform does not support SO_ORIGINAL_DST",
        ))
    }
}
