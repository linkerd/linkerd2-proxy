mod client_hello;

use crate::{
    HasNegotiatedProtocol, LocalId, NegotiatedProtocol, NegotiatedProtocolRef, ServerId,
    TlsAcceptor,
};
use bytes::BytesMut;
use futures::prelude::*;
use linkerd_conditional::Conditional;
use linkerd_error::Error;
use linkerd_identity as id;
use linkerd_io::{self as io, AsyncReadExt, EitherIo, PeerAddr, PrefixedIo, ReadBuf};
use linkerd_stack::{layer, NewService, Param};
use std::{
    fmt,
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::time::{self, Duration};
use tower::util::ServiceExt;
use tracing::{debug, trace, warn};

use crate::imp;

#[derive(Debug)]
pub struct TlsStream<IO>(imp::server::TlsStream<IO>);

impl<IO> TlsStream<IO> {
    pub fn client_identity(&self) -> Option<ClientId> {
        self.0.client_identity()
    }
}

impl<IO> From<imp::server::TlsStream<IO>> for TlsStream<IO> {
    fn from(stream: imp::server::TlsStream<IO>) -> Self {
        TlsStream(stream)
    }
}

impl<IO> io::AsyncRead for TlsStream<IO>
where
    IO: io::AsyncRead + io::AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<IO> io::AsyncWrite for TlsStream<IO>
where
    IO: io::AsyncRead + io::AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl<IO: PeerAddr> PeerAddr for TlsStream<IO> {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.0.peer_addr()
    }
}

impl<IO> HasNegotiatedProtocol for TlsStream<IO> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        self.0.negotiated_protocol()
    }
}

pub type Config = Arc<id::ServerConfig>;

/// Produces a server config that fails to handshake all connections.
pub fn empty_config() -> Config {
    Arc::new(id::ServerConfig::empty())
}

/// A newtype for remote client idenities.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ClientId(pub id::Name);

/// Indicates a serverside connection's TLS status.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum ServerTls {
    Established {
        client_id: Option<ClientId>,
        negotiated_protocol: Option<NegotiatedProtocol>,
    },
    Passthru {
        sni: ServerId,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum NoServerTls {
    /// Identity is administratively disabled.
    Disabled,

    /// No TLS is wanted because the connection is a loopback connection which
    /// doesn't need or support TLS.
    Loopback,

    /// No TLS is wanted because the connection is a loopback connection which
    /// doesn't need or support TLS.
    PortSkipped,

    // No TLS Client Hello detected
    NoClientHello,
}

/// Indicates whether TLS was established on an accepted connection.
pub type ConditionalServerTls = Conditional<ServerTls, NoServerTls>;

pub type Meta<T> = (ConditionalServerTls, T);

type DetectIo<T> = EitherIo<T, PrefixedIo<T>>;
pub type Io<T> = EitherIo<TlsStream<DetectIo<T>>, DetectIo<T>>;

pub type Connection<T, I> = (Meta<T>, Io<I>);

#[derive(Clone, Debug)]
pub struct NewDetectTls<L, A> {
    local_identity: Option<L>,
    inner: A,
    timeout: Duration,
}

#[derive(Clone, Debug, Error)]
#[error("TLS detection timed out")]
pub struct DetectTimeout(());

#[derive(Clone, Debug)]
pub struct DetectTls<T, L, N> {
    target: T,
    local_identity: Option<L>,
    inner: N,
    timeout: Duration,
}

// The initial peek buffer is statically allocated on the stack and is fairly small; but it is
// large enough to hold the ~300B ClientHello sent by proxies.
const PEEK_CAPACITY: usize = 512;

// A larger fallback buffer is allocated onto the heap if the initial peek buffer is
// insufficient. This is the same value used in HTTP detection.
const BUFFER_CAPACITY: usize = 8192;

impl<I, N> NewDetectTls<I, N> {
    pub fn new(local_identity: Option<I>, inner: N, timeout: Duration) -> Self {
        Self {
            local_identity,
            inner,
            timeout,
        }
    }

    pub fn layer(
        local_identity: Option<I>,
        timeout: Duration,
    ) -> impl layer::Layer<N, Service = Self> + Clone
    where
        I: Clone,
    {
        layer::mk(move |inner| Self::new(local_identity.clone(), inner, timeout))
    }
}

impl<T, L, N> NewService<T> for NewDetectTls<L, N>
where
    L: Clone + Param<LocalId> + Param<Config>,
    N: NewService<Meta<T>> + Clone,
{
    type Service = DetectTls<T, L, N>;

    fn new_service(&mut self, target: T) -> Self::Service {
        DetectTls {
            target,
            local_identity: self.local_identity.clone(),
            inner: self.inner.clone(),
            timeout: self.timeout,
        }
    }
}

impl<I, L, N, NSvc, T> tower::Service<I> for DetectTls<T, L, N>
where
    I: io::Peek + io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin + 'static,
    L: Param<LocalId> + Param<Config>,
    N: NewService<Meta<T>, Service = NSvc> + Clone + Send + 'static,
    NSvc: tower::Service<Io<I>, Response = ()> + Send + 'static,
    NSvc::Error: Into<Error>,
    NSvc::Future: Send,
    T: Clone + Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let target = self.target.clone();
        let mut new_accept = self.inner.clone();

        match self.local_identity.as_ref() {
            Some(local) => {
                let config: Config = local.param();
                let LocalId(local_id) = local.param();

                // Detect the SNI from a ClientHello (or timeout).
                let detect = time::timeout(self.timeout, detect_sni(io));
                Box::pin(async move {
                    let (sni, io) = detect.await.map_err(|_| DetectTimeout(()))??;

                    let (peer, io) = match sni {
                        // If we detected an SNI matching this proxy, terminate TLS.
                        Some(ServerId(id)) if id == local_id => {
                            trace!("Identified local SNI");
                            let (peer, io) = handshake(config, io).await?;
                            (Conditional::Some(peer), EitherIo::Left(io))
                        }
                        // If we detected another SNI, continue proxying the
                        // opaque stream.
                        Some(sni) => {
                            debug!(%sni, "Identified foreign SNI");
                            let peer = ServerTls::Passthru { sni };
                            (Conditional::Some(peer), EitherIo::Right(io))
                        }
                        // If no TLS was detected, continue proxying the stream.
                        None => (
                            Conditional::None(NoServerTls::NoClientHello),
                            EitherIo::Right(io),
                        ),
                    };

                    new_accept
                        .new_service((peer, target))
                        .oneshot(io)
                        .err_into::<Error>()
                        .await
                })
            }

            None => {
                let peer = Conditional::None(NoServerTls::Disabled);
                let svc = new_accept.new_service((peer, target));
                Box::pin(
                    svc.oneshot(EitherIo::Right(EitherIo::Left(io)))
                        .err_into::<Error>(),
                )
            }
        }
    }
}

/// Peek or buffer the provided stream to determine an SNI value.
async fn detect_sni<I>(mut io: I) -> io::Result<(Option<ServerId>, DetectIo<I>)>
where
    I: io::Peek + io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin,
{
    // First, try to use MSG_PEEK to read the SNI from the TLS ClientHello.
    // Because peeked data does not need to be retained, we use a static
    // buffer to prevent needless heap allocation.
    //
    // Anecdotally, the ClientHello sent by Linkerd proxies is <300B. So a
    // ~500B byte buffer is more than enough.
    let mut buf = [0u8; PEEK_CAPACITY];
    let sz = io.peek(&mut buf).await?;
    debug!(sz, "Peeked bytes from TCP stream");
    // Peek may return 0 bytes if the socket is not peekable.
    if sz > 0 {
        match client_hello::parse_sni(&buf) {
            Ok(sni) => {
                return Ok((sni, EitherIo::Left(io)));
            }

            Err(client_hello::Incomplete) => {}
        }
    }

    // Peeking didn't return enough data, so instead we'll allocate more
    // capacity and try reading data from the socket.
    debug!("Attempting to buffer TLS ClientHello after incomplete peek");
    let mut buf = BytesMut::with_capacity(BUFFER_CAPACITY);
    debug!(buf.capacity = %buf.capacity(), "Reading bytes from TCP stream");
    while io.read_buf(&mut buf).await? != 0 {
        debug!(buf.len = %buf.len(), "Read bytes from TCP stream");
        match client_hello::parse_sni(buf.as_ref()) {
            Ok(sni) => {
                return Ok((sni, EitherIo::Right(PrefixedIo::new(buf.freeze(), io))));
            }

            Err(client_hello::Incomplete) => {
                if buf.capacity() == 0 {
                    // If we can't buffer an entire TLS ClientHello, it
                    // almost definitely wasn't initiated by another proxy,
                    // at least.
                    warn!("Buffer insufficient for TLS ClientHello");
                    break;
                }
                // Continue if there is still buffer capacity.
            }
        }
    }

    trace!("Could not read TLS ClientHello via buffering");
    let io = EitherIo::Right(PrefixedIo::new(buf.freeze(), io));
    Ok((None, io))
}

async fn handshake<T>(tls_config: Config, io: T) -> io::Result<(ServerTls, TlsStream<T>)>
where
    T: io::AsyncRead + io::AsyncWrite + Unpin,
{
    let io = TlsAcceptor::from(tls_config).accept(io).await?;

    // Determine the peer's identity, if it exist.
    let client_id = io.client_identity();
    // Extract the negotiated protocol for the stream.
    let negotiated_protocol = io.negotiated_protocol().map(|p| p.to_owned());

    debug!(client.id = ?client_id, alpn = ?negotiated_protocol, "Accepted TLS connection");
    let tls = ServerTls::Established {
        client_id,
        negotiated_protocol,
    };
    Ok((tls, io))
}

// === impl ClientId ===

impl From<id::Name> for ClientId {
    fn from(n: id::Name) -> Self {
        Self(n)
    }
}

impl From<ClientId> for id::Name {
    fn from(ClientId(name): ClientId) -> id::Name {
        name
    }
}

impl AsRef<id::Name> for ClientId {
    fn as_ref(&self) -> &id::Name {
        &self.0
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ClientId {
    type Err = id::InvalidName;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        id::Name::from_str(s).map(Self)
    }
}

// === impl NoClientId ===

impl fmt::Display for NoServerTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "disabled"),
            Self::Loopback => write!(f, "loopback"),
            Self::PortSkipped => write!(f, "port_skipped"),
            Self::NoClientHello => write!(f, "no_tls_from_remote"),
        }
    }
}

#[cfg(test)]
mod tests {
    use io::AsyncWriteExt;

    use super::*;
    use std::str::FromStr;

    #[tokio::test(flavor = "current_thread")]
    async fn detect_buffered() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut client_io, server_io) = tokio::io::duplex(1024);
        let input = include_bytes!("testdata/curl-example-com-client-hello.bin");
        let len = input.len();
        let client_task = tokio::spawn(async move {
            client_io
                .write_all(&*input)
                .await
                .expect("Write must suceed");
        });

        let (sni, io) = detect_sni(server_io)
            .await
            .expect("SNI detection must not fail");

        let identity = id::Name::from_str("example.com").unwrap();
        assert_eq!(sni, Some(ServerId(identity)));

        match io {
            EitherIo::Left(_) => panic!("Detected IO should be buffered"),
            EitherIo::Right(io) => assert_eq!(io.prefix().len(), len, "All data must be buffered"),
        }

        client_task.await.expect("Client must not fail");
    }
}

#[cfg(fuzzing)]
pub mod fuzz_logic {
    use super::*;

    pub fn fuzz_entry(input: &[u8]) {
        let _ = client_hello::parse_sni(input);
    }
}
