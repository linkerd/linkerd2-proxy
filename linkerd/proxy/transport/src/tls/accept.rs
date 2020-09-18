use super::{conditional_accept, Conditional, PeerIdentity, ReasonForNoPeerName};
use crate::io::{BoxedIo, PrefixedIo};
use crate::listen::Addrs;
use bytes::BytesMut;
use futures::prelude::*;
use linkerd2_dns_name as dns;
use linkerd2_error::{Error, Never};
use linkerd2_identity as identity;
use linkerd2_stack::layer;
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
use tower::util::ServiceExt;
use tracing::{debug, trace, warn};

pub trait HasConfig {
    fn tls_server_name(&self) -> identity::Name;
    fn tls_server_config(&self) -> Arc<Config>;
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

pub type Connection = (Meta, BoxedIo);

#[derive(Clone, Debug)]
pub struct DetectTls<I, A> {
    local_identity: Conditional<I>,
    inner: A,
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct DetectTimeout(());

#[derive(Clone, Debug)]
pub struct AcceptTls<I, A> {
    addrs: Addrs,
    local_identity: Conditional<I>,
    inner: A,
    timeout: Duration,
}

// The initial peek buffer is statically allocated on the stack and is fairly small; but it is
// large enough to hold the ~300B ClientHello sent by proxies.
const PEEK_CAPACITY: usize = 512;

// A larger fallback buffer is allocated onto the heap if the initial peek buffer is
// insufficient. This is the same value used in HTTP detection.
const BUFFER_CAPACITY: usize = 8192;

impl<I: HasConfig, M> DetectTls<I, M> {
    pub fn new(local_identity: Conditional<I>, inner: M, timeout: Duration) -> Self {
        Self {
            local_identity,
            inner,
            timeout,
        }
    }

    pub fn layer(
        local_identity: Conditional<I>,
        timeout: Duration,
    ) -> impl layer::Layer<M, Service = Self>
    where
        I: Clone,
    {
        layer::mk(move |inner| Self::new(local_identity.clone(), inner, timeout))
    }
}

impl<I: HasConfig, M> tower::Service<Addrs> for DetectTls<I, M>
where
    I: Clone,
    M: tower::Service<Meta> + Clone,
{
    type Response = AcceptTls<I, M>;
    type Error = Never;
    type Future = future::Ready<Result<AcceptTls<I, M>, Never>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // The `accept` is cloned into the response future, so its readiness isn't important.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, addrs: Addrs) -> Self::Future {
        future::ok(AcceptTls {
            addrs,
            local_identity: self.local_identity.clone(),
            inner: self.inner.clone(),
            timeout: self.timeout,
        })
    }
}

impl<I: HasConfig, M, A> tower::Service<TcpStream> for AcceptTls<I, M>
where
    M: tower::Service<Meta, Response = A> + Clone + Send + 'static,
    M::Error: Into<Error>,
    M::Future: Send,
    A: tower::Service<BoxedIo, Response = ()> + Send,
    A::Error: Into<Error>,
    A::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, tcp: TcpStream) -> Self::Future {
        let addrs = self.addrs.clone();
        let make = self.inner.clone();

        match self.local_identity.as_ref() {
            Conditional::Some(local) => {
                let config = local.tls_server_config();
                let name = local.tls_server_name();
                let timeout = tokio::time::delay_for(self.timeout);

                Box::pin(async move {
                    let (peer_identity, io) = tokio::select! {
                        res = detect(config, name, tcp) => { res? }
                        () = timeout => {
                            return Err(DetectTimeout(()).into());
                        }
                    };
                    let meta = Meta {
                        peer_identity,
                        addrs,
                    };
                    make.oneshot(meta)
                        .err_into::<Error>()
                        .await?
                        .oneshot(io)
                        .err_into::<Error>()
                        .await
                })
            }

            Conditional::None(reason) => Box::pin(async move {
                let meta = Meta {
                    peer_identity: Conditional::None(reason),
                    addrs,
                };
                make.oneshot(meta)
                    .err_into::<Error>()
                    .await?
                    .oneshot(BoxedIo::new(tcp))
                    .err_into::<Error>()
                    .await
            }),
        }
    }
}

pub async fn detect(
    tls_config: Arc<Config>,
    local_id: identity::Name,
    mut tcp: TcpStream,
) -> io::Result<(PeerIdentity, BoxedIo)> {
    const NO_TLS_META: PeerIdentity = Conditional::None(ReasonForNoPeerName::NoTlsFromRemote);

    // First, try to use MSG_PEEK to read the SNI from the TLS ClientHello.
    // Because peeked data does not need to be retained, we use a static
    // buffer to prevent needless heap allocation.
    //
    // Anecdotally, the ClientHello sent by Linkerd proxies is <300B. So a
    // ~500B byte buffer is more than enough.
    let mut buf = [0u8; PEEK_CAPACITY];
    let sz = tcp.peek(&mut buf).await?;
    debug!(sz, "Peeked bytes from TCP stream");
    match conditional_accept::match_client_hello(&buf, &local_id) {
        conditional_accept::Match::Matched => {
            trace!("Identified matching SNI via peek");
            // Terminate the TLS stream.
            let (peer_id, tls) = handshake(tls_config, tcp).await?;
            return Ok((peer_id, BoxedIo::new(tls)));
        }

        conditional_accept::Match::NotMatched => {
            trace!("Not a matching TLS ClientHello");
            return Ok((NO_TLS_META, BoxedIo::new(tcp)));
        }

        conditional_accept::Match::Incomplete => {}
    }

    // Peeking didn't return enough data, so instead we'll allocate more
    // capacity and try reading data from the socket.
    debug!("Attempting to buffer TLS ClientHello after incomplete peek");
    let mut buf = BytesMut::with_capacity(BUFFER_CAPACITY);
    debug!(buf.capacity = %buf.capacity(), "Reading bytes from TCP stream");
    while tcp.read_buf(&mut buf).await? != 0 {
        debug!(buf.len = %buf.len(), "Read bytes from TCP stream");
        match conditional_accept::match_client_hello(buf.as_ref(), &local_id) {
            conditional_accept::Match::Matched => {
                trace!("Identified matching SNI via buffered read");
                // Terminate the TLS stream.
                let (peer_id, tls) =
                    handshake(tls_config.clone(), PrefixedIo::new(buf.freeze(), tcp)).await?;
                return Ok((peer_id, BoxedIo::new(tls)));
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
    let io = BoxedIo::new(PrefixedIo::new(buf.freeze(), tcp));
    Ok((NO_TLS_META, io))
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
