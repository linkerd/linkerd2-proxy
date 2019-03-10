use bytes::Buf;
use futures::Future;
use std::fmt::Debug;
use std::io;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio::prelude::*;

use dns;
use identity::Name as Identity;
use transport::{io::internal::Io, prefixed::Prefixed, AddrInfo, SetKeepalive};

use super::{
    rustls,
    tokio_rustls::{self, TlsAcceptor, TlsConnector, TlsStream},
    ClientConfig, ServerConfig,
};

pub use self::rustls::Session;
use bytes::Bytes;

// In theory we could replace `TcpStream` with `Io`. However, it is likely that
// in the future we'll need to do things specific to `TcpStream`, so optimize
// for that unless/until there is some benefit to doing otherwise.
#[derive(Debug)]
pub struct Connection<S, C>(TlsStream<S, C>)
where
    S: Debug,
    C: Debug;

pub struct UpgradeToTls<S, C, F>(F)
where
    C: Session,
    F: Future<Item = TlsStream<S, C>, Error = io::Error>;

impl<C, S, F> Future for UpgradeToTls<S, C, F>
where
    S: Debug,
    C: Session + Debug,
    F: Future<Item = TlsStream<S, C>, Error = io::Error>,
{
    type Item = Connection<S, C>;
    type Error = io::Error;

    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        let tls_stream = try_ready!(self.0.poll());
        return Ok(Async::Ready(Connection(tls_stream)));
    }
}

pub type UpgradeClientToTls =
    UpgradeToTls<TcpStream, rustls::ClientSession, tokio_rustls::Connect<TcpStream>>;

pub type UpgradeServerToTls = UpgradeToTls<
    Prefixed<TcpStream>,
    rustls::ServerSession,
    tokio_rustls::Accept<Prefixed<TcpStream>>,
>;

impl Connection<TcpStream, rustls::ClientSession> {
    pub fn connect(
        socket: TcpStream,
        id: &Identity,
        ClientConfig(config): ClientConfig,
    ) -> UpgradeClientToTls {
        let c = TlsConnector::from(config).connect(id.as_dns_name_ref(), socket);
        UpgradeToTls(c)
    }
}

impl Connection<Prefixed<TcpStream>, rustls::ServerSession> {
    pub fn accept(
        socket: TcpStream,
        prefix: Bytes,
        ServerConfig(config): ServerConfig,
    ) -> UpgradeServerToTls {
        let a = TlsAcceptor::from(config).accept(Prefixed::new(prefix, socket));
        UpgradeToTls(a)
    }

    pub fn client_identity(&self) -> Option<Identity> {
        let (_io, session) = self.0.get_ref();
        let certs = session.get_peer_certificates()?;
        let end_cert = super::parse_end_entity_cert(&certs).ok()?;
        // Use the first DNS name ias the identity.
        let name: &str = end_cert.dns_names().ok()?.into_iter().next()?.into();
        Identity::from_sni_hostname(name.as_bytes()).ok()
    }
}

impl<S, C> io::Read for Connection<S, C>
where
    S: Debug + io::Read + io::Write,
    C: Session + Debug,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<S, C> AsyncRead for Connection<S, C>
where
    S: AsyncRead + AsyncWrite + Debug + io::Read + io::Write,
    C: Session + Debug,
{
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.0.prepare_uninitialized_buffer(buf)
    }
}

impl<S, C> io::Write for Connection<S, C>
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

impl<S, C> AsyncWrite for Connection<S, C>
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

impl<S, C> AddrInfo for Connection<S, C>
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

impl<S, C> SetKeepalive for Connection<S, C>
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

impl<S, C> Io for Connection<S, C>
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
