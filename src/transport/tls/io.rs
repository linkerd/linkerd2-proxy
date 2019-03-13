use bytes::{Buf, Bytes, BytesMut};
use futures::Future;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::{cmp, io};
use tokio::net::TcpStream;
use tokio::prelude::*;

use super::tokio_rustls::TlsStream;
use dns;
use identity;
use transport::io::internal::Io;
use transport::prefixed::Prefixed;
use transport::tls::{
    self, HasPeerIdentity, PeerIdentity, ReasonForNoIdentity, ReasonForNoPeerName, Session,
};
use transport::{AddrInfo, BoxedIo, GetOriginalDst, Peek, SetKeepalive};
use Conditional;

/// Wraps a TLS stream to implement Io.
#[derive(Debug)]
pub(super) struct TlsIo<S, C>(TlsStream<S, C>)
where
    S: Debug,
    C: Debug;


// === imp TlsIo ===

impl<S, C> From<TlsStream<S, C>> for TlsIo<S, C>
where
    S: Debug,
    C: Debug,
{
    fn from(s: TlsStream<S, C>) -> Self {
        TlsIo(s)
    }
}

impl<S, C> io::Read for TlsIo<S, C>
where
    S: Debug + io::Read + io::Write,
    C: Session + Debug,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<S, C> AsyncRead for TlsIo<S, C>
where
    S: AsyncRead + AsyncWrite + Debug + io::Read + io::Write,
    C: Session + Debug,
{
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.0.prepare_uninitialized_buffer(buf)
    }
}

impl<S, C> io::Write for TlsIo<S, C>
where
    S: Debug + io::Read + io::Write,
    C: Session + Debug,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<S, C> AsyncWrite for TlsIo<S, C>
where
    S: AsyncRead + AsyncWrite + Debug + io::Read + io::Write,
    C: Session + Debug,
{
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.0.shutdown()
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        self.0.write_buf(buf)
    }
}

impl<S, C> AddrInfo for TlsIo<S, C>
where
    S: AddrInfo + Debug,
    C: Session + Debug,
{
    fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.0.get_ref().0.local_addr()
    }

    fn get_original_dst(&self) -> Option<SocketAddr> {
        self.0.get_ref().0.get_original_dst()
    }
}

impl<S, C> SetKeepalive for TlsIo<S, C>
where
    S: SetKeepalive + Debug,
    C: Session + Debug,
{
    fn keepalive(&self) -> io::Result<Option<::std::time::Duration>> {
        self.0.get_ref().0.keepalive()
    }

    fn set_keepalive(&mut self, ka: Option<::std::time::Duration>) -> io::Result<()> {
        self.0.get_mut().0.set_keepalive(ka)
    }
}

impl<S, C> Io for TlsIo<S, C>
where
    S: Io + Debug,
    C: Session + Debug,
{
    fn shutdown_write(&mut self) -> Result<(), io::Error> {
        self.0.get_mut().0.shutdown_write()
    }

    fn write_buf_erased(&mut self, mut buf: &mut Buf) -> Poll<usize, io::Error> {
        self.0.write_buf(&mut buf)
    }
}
