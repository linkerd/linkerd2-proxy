use super::{conditional_accept, Conditional, PeerIdentity, ReasonForNoPeerName};
use crate::io::{EitherIo, PrefixedIo};
use crate::listen::Addrs;
use bytes::BytesMut;
use futures::prelude::*;
use linkerd2_dns_name as dns;
use linkerd2_error::Error;
use linkerd2_identity as identity;
use linkerd2_stack::{layer, NewService};
pub use rustls::ServerConfig as Config;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{self, AsyncReadExt},
    net::TcpStream,
};
use tokio_rustls::server::TlsStream;
use tower::util::ServiceExt;
use tracing::{debug, trace, warn};

pub trait HasConfig {
    fn tls_server_name(&self) -> identity::Name;
    fn tls_server_config(&self) -> Arc<Config>;
}

/// Must be implemented for I/O types like `TcpStream` on which TLS is
/// transparently detected.
///
/// This is necessary so that we can be generic over the I/O type but still use
/// `TcpStream::peek` to avoid allocating for mTLS SNI detection.
#[async_trait::async_trait]
pub trait Detectable {
    /// Attempts to detect a `ClientHello` message from the underlying transport
    /// and, if its SNI matches `local_name`, initiates a TLS server handshake to
    /// decrypt the stream.
    ///
    /// Returns the client's identity, if one exists, and an optionally decrypted
    /// transport.
    async fn detected(
        self,
        config: Arc<Config>,
        local_name: identity::Name,
    ) -> io::Result<(PeerIdentity, Io<Self>)>
    where
        Self: Sized;
}

/// Produces a server config that fails to handshake all connections.
pub fn empty_config() -> Arc<Config> {
    let verifier = rustls::NoClientAuth::new();
    Arc::new(Config::new(verifier))
}

#[derive(Clone, Debug)]
pub struct Meta {
    // TODO sni name
    pub peer_identity: PeerIdentity,
    pub addrs: Addrs,
}

pub type Io<T> = EitherIo<PrefixedIo<T>, TlsStream<PrefixedIo<T>>>;

pub type Connection<T> = (Meta, Io<T>);

#[derive(Clone, Debug)]
pub struct NewDetectTls<I, A> {
    local_identity: Conditional<I>,
    inner: A,
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct DetectTimeout(());

#[derive(Clone, Debug)]
pub struct DetectTls<I, N> {
    addrs: Addrs,
    local_identity: Conditional<I>,
    inner: N,
    timeout: Duration,
}

// The initial peek buffer is statically allocated on the stack and is fairly small; but it is
// large enough to hold the ~300B ClientHello sent by proxies.
const PEEK_CAPACITY: usize = 512;

// A larger fallback buffer is allocated onto the heap if the initial peek buffer is
// insufficient. This is the same value used in HTTP detection.
const BUFFER_CAPACITY: usize = 8192;

impl<I: HasConfig, N> NewDetectTls<I, N> {
    pub fn new(local_identity: Conditional<I>, inner: N, timeout: Duration) -> Self {
        Self {
            local_identity,
            inner,
            timeout,
        }
    }

    pub fn layer(
        local_identity: Conditional<I>,
        timeout: Duration,
    ) -> impl layer::Layer<N, Service = Self> + Clone
    where
        I: Clone,
    {
        layer::mk(move |inner| Self::new(local_identity.clone(), inner, timeout))
    }
}

impl<I, N> NewService<Addrs> for NewDetectTls<I, N>
where
    I: HasConfig + Clone,
    N: NewService<Meta> + Clone,
{
    type Service = DetectTls<I, N>;

    fn new_service(&mut self, addrs: Addrs) -> Self::Service {
        DetectTls {
            addrs,
            local_identity: self.local_identity.clone(),
            inner: self.inner.clone(),
            timeout: self.timeout,
        }
    }
}

impl<T, I, N, NSvc> tower::Service<T> for DetectTls<I, N>
where
    T: Detectable + Send + 'static,
    I: HasConfig,
    N: NewService<Meta, Service = NSvc> + Clone + Send + 'static,
    NSvc: tower::Service<Io<T>, Response = ()> + Send + 'static,
    NSvc::Error: Into<Error>,
    NSvc::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, tcp: T) -> Self::Future {
        let addrs = self.addrs.clone();
        let mut new_accept = self.inner.clone();

        match self.local_identity.as_ref() {
            Conditional::Some(local) => {
                let config = local.tls_server_config();
                let name = local.tls_server_name();
                let timeout = tokio::time::sleep(self.timeout);

                Box::pin(async move {
                    let (peer_identity, io) = tokio::select! {
                        res = tcp.detected(config, name) => { res? }
                        () = timeout => {
                            return Err(DetectTimeout(()).into());
                        }
                    };
                    let meta = Meta {
                        peer_identity,
                        addrs,
                    };
                    new_accept
                        .new_service(meta)
                        .oneshot(io)
                        .err_into::<Error>()
                        .await
                })
            }

            Conditional::None(reason) => {
                let meta = Meta {
                    peer_identity: Conditional::None(reason),
                    addrs,
                };
                let svc = new_accept.new_service(meta);
                Box::pin(svc.oneshot(EitherIo::Left(tcp.into())).err_into::<Error>())
            }
        }
    }
}

#[async_trait::async_trait]
impl Detectable for TcpStream {
    async fn detected(
        mut self,
        tls_config: Arc<Config>,
        local_id: identity::Name,
    ) -> io::Result<(PeerIdentity, Io<Self>)> {
        const NO_TLS_META: PeerIdentity = Conditional::None(ReasonForNoPeerName::NoTlsFromRemote);

        // First, try to use MSG_PEEK to read the SNI from the TLS ClientHello.
        // Because peeked data does not need to be retained, we use a static
        // buffer to prevent needless heap allocation.
        //
        // Anecdotally, the ClientHello sent by Linkerd proxies is <300B. So a
        // ~500B byte buffer is more than enough.
        let mut buf = [0u8; PEEK_CAPACITY];
        let sz = self.peek(&mut buf).await?;
        debug!(sz, "Peeked bytes from TCP stream");
        match conditional_accept::match_client_hello(&buf, &local_id) {
            conditional_accept::Match::Matched => {
                trace!("Identified matching SNI via peek");
                // Terminate the TLS stream.
                let (peer_id, tls) = handshake(tls_config, PrefixedIo::from(self)).await?;
                return Ok((peer_id, EitherIo::Right(tls)));
            }

            conditional_accept::Match::NotMatched => {
                trace!("Not a matching TLS ClientHello");
                return Ok((NO_TLS_META, EitherIo::Left(self.into())));
            }

            conditional_accept::Match::Incomplete => {}
        }

        // Peeking didn't return enough data, so instead we'll allocate more
        // capacity and try reading data from the socket.
        debug!("Attempting to buffer TLS ClientHello after incomplete peek");
        let mut buf = BytesMut::with_capacity(BUFFER_CAPACITY);
        debug!(buf.capacity = %buf.capacity(), "Reading bytes from TCP stream");
        while self.read_buf(&mut buf).await? != 0 {
            debug!(buf.len = %buf.len(), "Read bytes from TCP stream");
            match conditional_accept::match_client_hello(buf.as_ref(), &local_id) {
                conditional_accept::Match::Matched => {
                    trace!("Identified matching SNI via buffered read");
                    // Terminate the TLS stream.
                    let (peer_id, tls) =
                        handshake(tls_config.clone(), PrefixedIo::new(buf.freeze(), self)).await?;
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
        let io = EitherIo::Left(PrefixedIo::new(buf.freeze(), self));
        Ok((NO_TLS_META, io))
    }
}

async fn handshake<T>(
    tls_config: Arc<Config>,
    io: T,
) -> io::Result<(PeerIdentity, tokio_rustls::server::TlsStream<T>)>
where
    T: io::AsyncRead + io::AsyncWrite + Unpin,
{
    let tls = tokio_rustls::TlsAcceptor::from(tls_config)
        .accept(io)
        .await?;

    // Determine the peer's identity, if it exist.
    let peer_id = client_identity(&tls)
        .map(Conditional::Some)
        .unwrap_or_else(|| Conditional::None(ReasonForNoPeerName::NoPeerIdFromRemote));

    trace!(peer.identity = ?peer_id, "Accepted TLS connection");
    Ok((peer_id, tls))
}

fn client_identity<S>(tls: &tokio_rustls::server::TlsStream<S>) -> Option<identity::Name> {
    use rustls::Session;
    use webpki::GeneralDNSNameRef;

    let (_io, session) = tls.get_ref();
    let certs = session.get_peer_certificates()?;
    let c = certs.first().map(rustls::Certificate::as_ref)?;
    let end_cert = webpki::EndEntityCert::from(c).ok()?;
    let dns_names = end_cert.dns_names().ok()?;

    match dns_names.first()? {
        GeneralDNSNameRef::DNSName(n) => Some(identity::Name::from(dns::Name::from(n.to_owned()))),
        GeneralDNSNameRef::Wildcard(_) => {
            // Wildcards can perhaps be handled in a future path...
            None
        }
    }
}

impl HasConfig for identity::CrtKey {
    fn tls_server_name(&self) -> identity::Name {
        identity::CrtKey::tls_server_name(self)
    }

    fn tls_server_config(&self) -> Arc<Config> {
        identity::CrtKey::tls_server_config(self)
    }
}

impl std::fmt::Display for DetectTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TLS detection timeout")
    }
}

impl std::error::Error for DetectTimeout {}

impl Into<std::net::SocketAddr> for &'_ Meta {
    fn into(self) -> std::net::SocketAddr {
        (&self.addrs).into()
    }
}
