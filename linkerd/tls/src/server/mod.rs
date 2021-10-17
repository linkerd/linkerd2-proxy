mod client_hello;

use crate::{LocalId, NegotiatedProtocol, ServerId};
use bytes::BytesMut;
use futures::prelude::*;
use linkerd_conditional::Conditional;
use linkerd_dns_name as dns;
use linkerd_error::Error;
use linkerd_identity as id;
use linkerd_io::{self as io, AsyncReadExt, EitherIo, PrefixedIo};
use linkerd_stack::{layer, ExtractParam, InsertParam, NewService, Param};
use std::{
    fmt,
    ops::Deref,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::time::{self, Duration};
use tokio_rustls::rustls::{self, Session};
pub use tokio_rustls::server::TlsStream;
use tower::util::ServiceExt;
use tracing::{debug, trace, warn};

pub type Config = Arc<rustls::ServerConfig>;

/// Produces a server config that fails to handshake all connections.
pub fn empty_config() -> Config {
    let verifier = rustls::NoClientAuth::new();
    Arc::new(rustls::ServerConfig::new(verifier))
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

type DetectIo<T> = EitherIo<T, PrefixedIo<T>>;

pub type Io<T> = EitherIo<TlsStream<DetectIo<T>>, DetectIo<T>>;

#[derive(Clone, Debug)]
pub struct NewDetectTls<L, P, N> {
    inner: N,
    params: P,
    _local_identity: std::marker::PhantomData<fn() -> L>,
}

#[derive(Copy, Clone, Debug)]
pub struct Timeout(pub Duration);

#[derive(Clone, Debug, Error)]
#[error("TLS detection timed out")]
pub struct ServerTlsTimeoutError(());

#[derive(Clone, Debug)]
pub struct DetectTls<T, L, P, N> {
    target: T,
    local_identity: L,
    timeout: Timeout,
    params: P,
    inner: N,
}

// The initial peek buffer is statically allocated on the stack and is fairly small; but it is
// large enough to hold the ~300B ClientHello sent by proxies.
const PEEK_CAPACITY: usize = 512;

// A larger fallback buffer is allocated onto the heap if the initial peek buffer is
// insufficient. This is the same value used in HTTP detection.
const BUFFER_CAPACITY: usize = 8192;

impl<L, P, N> NewDetectTls<L, P, N> {
    pub fn new(params: P, inner: N) -> Self {
        Self {
            inner,
            params,
            _local_identity: std::marker::PhantomData,
        }
    }

    pub fn layer(params: P) -> impl layer::Layer<N, Service = Self> + Clone
    where
        P: Clone,
    {
        layer::mk(move |inner| Self::new(params.clone(), inner))
    }
}

impl<T, L, P, N> NewService<T> for NewDetectTls<L, P, N>
where
    P: ExtractParam<Timeout, T> + ExtractParam<L, T> + Clone,
    N: Clone,
{
    type Service = DetectTls<T, L, P, N>;

    fn new_service(&self, target: T) -> Self::Service {
        let timeout = self.params.extract_param(&target);
        let local_identity = self.params.extract_param(&target);
        DetectTls {
            target,
            local_identity,
            timeout,
            params: self.params.clone(),
            inner: self.inner.clone(),
        }
    }
}

impl<I, T, L, P, N, NSvc> tower::Service<I> for DetectTls<T, L, P, N>
where
    I: io::Peek + io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin + 'static,
    T: Clone + Send + 'static,
    P: InsertParam<ConditionalServerTls, T> + Clone + Send + Sync + 'static,
    P::Target: Send + 'static,
    L: Param<LocalId> + Param<Config>,
    N: NewService<P::Target, Service = NSvc> + Clone + Send + 'static,
    NSvc: tower::Service<Io<I>, Response = ()> + Send + 'static,
    NSvc::Error: Into<Error>,
    NSvc::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let target = self.target.clone();
        let params = self.params.clone();
        let new_accept = self.inner.clone();

        let config: Config = self.local_identity.param();
        let LocalId(local_id) = self.local_identity.param();

        // Detect the SNI from a ClientHello (or timeout).
        let Timeout(timeout) = self.timeout;
        let detect = time::timeout(timeout, detect_sni(io));
        Box::pin(async move {
            let (sni, io) = detect.await.map_err(|_| ServerTlsTimeoutError(()))??;

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

            let svc = new_accept.new_service(params.insert_param(peer, target));
            svc.oneshot(io).err_into::<Error>().await
        })
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
    let io = tokio_rustls::TlsAcceptor::from(tls_config)
        .accept(io)
        .await?;

    // Determine the peer's identity, if it exist.
    let client_id = client_identity(&io);

    let negotiated_protocol = io
        .get_ref()
        .1
        .get_alpn_protocol()
        .map(|b| NegotiatedProtocol(b.into()));

    debug!(client.id = ?client_id, alpn = ?negotiated_protocol, "Accepted TLS connection");
    let tls = ServerTls::Established {
        client_id,
        negotiated_protocol,
    };
    Ok((tls, io))
}

fn client_identity<S>(tls: &TlsStream<S>) -> Option<ClientId> {
    use webpki::GeneralDNSNameRef;

    let (_io, session) = tls.get_ref();
    let certs = session.get_peer_certificates()?;
    let c = certs.first().map(rustls::Certificate::as_ref)?;
    let end_cert = webpki::EndEntityCert::from(c).ok()?;
    let dns_names = end_cert.dns_names().ok()?;

    match dns_names.first()? {
        GeneralDNSNameRef::DNSName(n) => {
            // Unfortunately we have to allocate a new string here, since there's no way to get the
            // underlying bytes from a `DNSNameRef`.
            let name = AsRef::<str>::as_ref(&n.to_owned())
                .parse::<dns::Name>()
                .ok()?;
            Some(ClientId(name.into()))
        }
        GeneralDNSNameRef::Wildcard(_) => {
            // Wildcards can perhaps be handled in a future path...
            None
        }
    }
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

impl Deref for ClientId {
    type Target = id::Name;

    fn deref(&self) -> &id::Name {
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
