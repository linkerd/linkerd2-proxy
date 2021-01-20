use crate::conditional_accept;
use bytes::BytesMut;
use futures::prelude::*;
use linkerd_conditional::Conditional;
use linkerd_dns_name as dns;
use linkerd_error::Error;
use linkerd_identity as id;
use linkerd_io::{self as io, AsyncReadExt, EitherIo, PrefixedIo};
use linkerd_stack::{layer, NewService};
use std::{
    fmt,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio_rustls::server::TlsStream;
use tower::util::ServiceExt;
use tracing::{debug, trace, warn};

pub type Config = Arc<rustls::ServerConfig>;

/// Produces a server config that fails to handshake all connections.
pub fn empty_config() -> Config {
    let verifier = rustls::NoClientAuth::new();
    Arc::new(rustls::ServerConfig::new(verifier))
}

/// A newtype for remote client idenities..
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ClientId(pub id::Name);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum NoTls {
    /// Identity is administratively disabled.
    Disabled,

    /// No TLS is wanted because the connection is a loopback connection which
    /// doesn't need or support TLS.
    Loopback,

    /// No TLS is wanted because the connection is a loopback connection which
    /// doesn't need or support TLS.
    PortSkipped,

    // TLS not established by the remote client.
    NoTlsFromRemote,
}

pub type ConditionalTls = Conditional<Option<ClientId>, NoTls>;

// TODO sni name
pub type Meta<T> = (ConditionalTls, T);

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
    for<'l> &'l L: Into<id::LocalId> + Into<Config>,
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
    for<'l> &'l L: Into<id::LocalId> + Into<Config>,
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
                let id::LocalId(local_id) = local.into();
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
                let peer = Conditional::None(NoTls::Disabled);
                let svc = new_accept.new_service((peer, target));
                Box::pin(svc.oneshot(EitherIo::Left(io.into())).err_into::<Error>())
            }
        }
    }
}

async fn detect<I>(
    mut io: I,
    tls_config: Config,
    local_id: id::Name,
) -> io::Result<(ConditionalTls, Io<I>)>
where
    I: io::Peek + io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin,
{
    const NO_TLS_META: ConditionalTls = Conditional::None(NoTls::NoTlsFromRemote);

    // First, try to use MSG_PEEK to read the SNI from the TLS ClientHello.
    // Because peeked data does not need to be retained, we use a static
    // buffer to prevent needless heap allocation.
    //
    // Anecdotally, the ClientHello sent by Linkerd proxies is <300B. So a
    // ~500B byte buffer is more than enough.
    let mut buf = [0u8; PEEK_CAPACITY];
    let sz = io.peek(&mut buf).await?;
    debug!(sz, "Peeked bytes from TCP stream");
    match conditional_accept::match_client_hello(&buf, &local_id) {
        conditional_accept::Match::Matched => {
            trace!("Identified matching SNI via peek");
            // Terminate the TLS stream.
            let (peer_id, tls) = handshake(tls_config, PrefixedIo::from(io)).await?;
            return Ok((peer_id, EitherIo::Right(tls)));
        }

        conditional_accept::Match::NotMatched => {
            trace!("Not a matching TLS ClientHello");
            return Ok((NO_TLS_META, EitherIo::Left(io.into())));
        }

        conditional_accept::Match::Incomplete => {}
    }

    // Peeking didn't return enough data, so instead we'll allocate more
    // capacity and try reading data from the socket.
    debug!("Attempting to buffer TLS ClientHello after incomplete peek");
    let mut buf = BytesMut::with_capacity(BUFFER_CAPACITY);
    debug!(buf.capacity = %buf.capacity(), "Reading bytes from TCP stream");
    while io.read_buf(&mut buf).await? != 0 {
        debug!(buf.len = %buf.len(), "Read bytes from TCP stream");
        match conditional_accept::match_client_hello(buf.as_ref(), &local_id) {
            conditional_accept::Match::Matched => {
                trace!("Identified matching SNI via buffered read");
                // Terminate the TLS stream.
                let (peer_id, tls) =
                    handshake(tls_config.clone(), PrefixedIo::new(buf.freeze(), io)).await?;
                return Ok((peer_id, EitherIo::Right(tls)));
            }

            conditional_accept::Match::NotMatched => break,

            conditional_accept::Match::Incomplete => {
                if buf.capacity() == 0 {
                    // If we can't buffer an entire TLS ClientHello, it
                    // almost definitely wasn't initiated by another proxy,
                    // at least.
                    warn!("Buffer insufficient for TLS ClientHello");
                    break;
                }
            }
        }
    }

    trace!("Could not read TLS ClientHello via buffering");
    let io = EitherIo::Left(PrefixedIo::new(buf.freeze(), io));
    Ok((NO_TLS_META, io))
}

async fn handshake<T>(
    tls_config: Config,
    io: T,
) -> io::Result<(ConditionalTls, tokio_rustls::server::TlsStream<T>)>
where
    T: io::AsyncRead + io::AsyncWrite + Unpin,
{
    let tls = tokio_rustls::TlsAcceptor::from(tls_config)
        .accept(io)
        .await?;

    // Determine the peer's identity, if it exist.
    let client_id = client_identity(&tls);

    trace!(client.id = ?client_id, "Accepted TLS connection");
    Ok((Conditional::Some(client_id), tls))
}

fn client_identity<S>(tls: &tokio_rustls::server::TlsStream<S>) -> Option<ClientId> {
    use rustls::Session;
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

impl fmt::Display for NoTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "disabled"),
            Self::Loopback => write!(f, "loopback"),
            Self::PortSkipped => write!(f, "port_skipped"),
            Self::NoTlsFromRemote => write!(f, "no_tls_from_remote"),
        }
    }
}
