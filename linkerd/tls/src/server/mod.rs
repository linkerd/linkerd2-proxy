mod client_hello;

use crate::{LocalId, NegotiatedProtocol, ServerId};
use bytes::BytesMut;
use futures::prelude::*;
use linkerd_conditional::Conditional;
use linkerd_dns_name as dns;
use linkerd_error::Error;
use linkerd_identity as id;
use linkerd_io::{self as io, AsyncReadExt, EitherIo, PrefixedIo};
use linkerd_stack::{layer, NewService};
use rustls::Session;
use std::{
    fmt,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
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

pub type Meta<T> = (ConditionalServerTls, T);

pub type Io<T> = EitherIo<PrefixedIo<T>, TlsStream<PrefixedIo<T>>>;

pub type Connection<T, I> = (Meta<T>, Io<I>);

#[derive(Clone, Debug)]
pub struct NewDetectTls<L, A> {
    local_identity: Option<L>,
    inner: A,
    timeout: Duration,
}

#[derive(Clone, Debug)]
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
    L: Clone,
    for<'l> &'l L: Into<LocalId> + Into<Config>,
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
    for<'l> &'l L: Into<LocalId> + Into<Config>,
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
                let config: Config = local.into();
                let local_id = local.into();
                let timeout = tokio::time::sleep(self.timeout);

                Box::pin(async move {
                    let (peer, io) = tokio::select! {
                        res = detect(io, config, local_id) => { res? }
                        () = timeout => {
                            return Err(DetectTimeout(()).into());
                        }
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
                Box::pin(svc.oneshot(EitherIo::Left(io.into())).err_into::<Error>())
            }
        }
    }
}

async fn detect<I>(
    mut io: I,
    tls_config: Config,
    LocalId(local_id): LocalId,
) -> io::Result<(ConditionalServerTls, Io<I>)>
where
    I: io::Peek + io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin,
{
    const NO_TLS_META: ConditionalServerTls = Conditional::None(NoServerTls::NoClientHello);

    // First, try to use MSG_PEEK to read the SNI from the TLS ClientHello.
    // Because peeked data does not need to be retained, we use a static
    // buffer to prevent needless heap allocation.
    //
    // Anecdotally, the ClientHello sent by Linkerd proxies is <300B. So a
    // ~500B byte buffer is more than enough.
    let mut buf = [0u8; PEEK_CAPACITY];
    let sz = io.peek(&mut buf).await?;
    debug!(sz, "Peeked bytes from TCP stream");
    match client_hello::parse_sni(&buf) {
        Ok(Some(ServerId(sni))) if sni == local_id => {
            trace!(%sni, "Identified matching SNI via peek");
            // Terminate the TLS stream.
            let (tls, io) = handshake(tls_config, PrefixedIo::from(io)).await?;
            return Ok((Conditional::Some(tls), EitherIo::Right(io)));
        }

        Ok(Some(sni)) => {
            trace!(%sni, "Identified non-matching SNI via peek");
            let tls = Conditional::Some(ServerTls::Passthru { sni });
            return Ok((tls, EitherIo::Left(io.into())));
        }

        Ok(None) => {
            trace!("Not a matching TLS ClientHello");
            return Ok((NO_TLS_META, EitherIo::Left(io.into())));
        }

        Err(client_hello::Incomplete) => {}
    }

    // Peeking didn't return enough data, so instead we'll allocate more
    // capacity and try reading data from the socket.
    debug!("Attempting to buffer TLS ClientHello after incomplete peek");
    let mut buf = BytesMut::with_capacity(BUFFER_CAPACITY);
    debug!(buf.capacity = %buf.capacity(), "Reading bytes from TCP stream");
    while io.read_buf(&mut buf).await? != 0 {
        debug!(buf.len = %buf.len(), "Read bytes from TCP stream");
        match client_hello::parse_sni(buf.as_ref()) {
            Ok(Some(ServerId(sni))) if sni == local_id => {
                trace!(%sni, "Identified matching SNI via buffered read");
                // Terminate the TLS stream.
                let (tls, io) =
                    handshake(tls_config.clone(), PrefixedIo::new(buf.freeze(), io)).await?;
                return Ok((Conditional::Some(tls), EitherIo::Right(io)));
            }

            Ok(Some(sni)) => {
                trace!(%sni, "Identified non-matching SNI via peek");
                let tls = Conditional::Some(ServerTls::Passthru { sni });
                return Ok((tls, EitherIo::Left(io.into())));
            }

            Ok(None) => {
                trace!("Not a matching TLS ClientHello");
                return Ok((NO_TLS_META, EitherIo::Left(io.into())));
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
    let io = EitherIo::Left(PrefixedIo::new(buf.freeze(), io));
    Ok((NO_TLS_META, io))
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
            Some(ClientId(id::Name::from(dns::Name::from(n.to_owned()))))
        }
        GeneralDNSNameRef::Wildcard(_) => {
            // Wildcards can perhaps be handled in a future path...
            None
        }
    }
}

impl fmt::Display for DetectTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TLS detection timeout")
    }
}

impl std::error::Error for DetectTimeout {}

// === impl ClientId ===

impl From<id::Name> for ClientId {
    fn from(n: id::Name) -> Self {
        Self(n)
    }
}

impl Into<id::Name> for ClientId {
    fn into(self) -> id::Name {
        self.0
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
