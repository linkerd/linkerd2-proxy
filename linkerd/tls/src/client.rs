use futures::{
    future::{Either, MapOk},
    prelude::*,
};
use linkerd_conditional::Conditional;
use linkerd_identity as id;
use linkerd_io as io;
use linkerd_stack::{layer, Param};
use std::{
    fmt,
    future::Future,
    ops::Deref,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};
pub use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{self, Session};
use tracing::{debug, trace};

/// A newtype for target server identities.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ServerId(pub id::Name);

/// A stack paramter that configures a `Client` to establish a TLS connection.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ClientTls {
    pub server_id: ServerId,
    pub alpn: Option<AlpnProtocols>,
}

/// A stack param that configures the available ALPN protocols.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct AlpnProtocols(pub Vec<Vec<u8>>);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum NoClientTls {
    /// Identity is administratively disabled.
    Disabled,

    /// No TLS is wanted because the connection is a loopback connection which
    /// doesn't need or support TLS.
    Loopback,

    /// The destination service didn't give us the identity, which is its way
    /// of telling us that we shouldn't do TLS for this endpoint.
    NotProvidedByServiceDiscovery,

    /// No discovery was attempted.
    IngressWithoutOverride,
}

/// A stack paramater that indicates whether the target server endpoint has a
/// known TLS identity.
pub type ConditionalClientTls = Conditional<ClientTls, NoClientTls>;

pub type Config = Arc<rustls::ClientConfig>;

#[derive(Clone, Debug)]
pub struct Client<L, C> {
    local: Option<L>,
    inner: C,
}

type Connect<F, I> = MapOk<F, fn(I) -> io::EitherIo<I, TlsStream<I>>>;
type Handshake<I> =
    Pin<Box<dyn Future<Output = io::Result<io::EitherIo<I, TlsStream<I>>>> + Send + 'static>>;

pub type Io<I> = io::EitherIo<I, TlsStream<I>>;

// === impl ClientTls ===

impl From<ServerId> for ClientTls {
    fn from(server_id: ServerId) -> Self {
        Self {
            server_id,
            alpn: None,
        }
    }
}

// === impl Client ===

impl<L: Clone, C> Client<L, C> {
    pub fn layer(local: Option<L>) -> impl layer::Layer<C, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            local: local.clone(),
        })
    }
}

impl<L, C, T> tower::Service<T> for Client<L, C>
where
    L: Clone + Param<Config>,
    T: Param<ConditionalClientTls>,
    C: tower::Service<T, Error = io::Error>,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
    C::Future: Send + 'static,
{
    type Response = Io<C::Response>;
    type Error = io::Error;
    type Future = Either<Connect<C::Future, C::Response>, Handshake<C::Response>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let ClientTls { server_id, alpn } = match target.param() {
            Conditional::Some(tls) => tls,
            Conditional::None(reason) => {
                debug!(%reason, "Peer does not support TLS");
                return Either::Left(self.inner.call(target).map_ok(io::EitherIo::Left));
            }
        };

        let handshake = match self.local.as_ref() {
            Some(local) => {
                // Build a rustls ClientConfig for this connection.
                //
                // If ALPN protocols are configured by the endpoint, we have to clone the
                // entire configuration and set the protocols. If there are no
                // ALPN options, clone the Arc'd base configuration without
                // extra allocation.
                //
                // TODO it would be better to avoid cloning the whole TLS config
                // per-connection.
                match alpn {
                    None => tokio_rustls::TlsConnector::from(local.param()),
                    Some(AlpnProtocols(protocols)) => {
                        let mut config: rustls::ClientConfig = local.param().as_ref().clone();
                        config.alpn_protocols = protocols;
                        tokio_rustls::TlsConnector::from(Arc::new(config))
                    }
                }
            }
            None => {
                trace!("Local identity disabled");
                return Either::Left(self.inner.call(target).map_ok(io::EitherIo::Left));
            }
        };

        debug!(server.id = %server_id, "Initiating TLS connection");
        let connect = self.inner.call(target);
        Either::Right(Box::pin(async move {
            let io = connect.await?;
            let sni = webpki::DNSNameRef::try_from_ascii(server_id.as_bytes())
                .expect("identity must be a valid DNS-like name");
            let io = handshake.connect(sni, io).await?;
            if let Some(alpn) = io.get_ref().1.get_alpn_protocol() {
                debug!(alpn = ?std::str::from_utf8(alpn));
            }
            Ok(io::EitherIo::Right(io))
        }))
    }
}

// === impl ServerId ===

impl From<id::Name> for ServerId {
    fn from(n: id::Name) -> Self {
        Self(n)
    }
}

impl From<ServerId> for id::Name {
    fn from(ServerId(name): ServerId) -> id::Name {
        name
    }
}

impl Deref for ServerId {
    type Target = id::Name;

    fn deref(&self) -> &id::Name {
        &self.0
    }
}

impl FromStr for ServerId {
    type Err = id::InvalidName;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        id::Name::from_str(s).map(ServerId)
    }
}

impl fmt::Display for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl NoClientTls ===

impl fmt::Display for NoClientTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "disabled"),
            Self::Loopback => write!(f, "loopback"),
            Self::NotProvidedByServiceDiscovery => {
                write!(f, "not_provided_by_service_discovery")
            }
            Self::IngressWithoutOverride => {
                write!(f, "ingress_without_override")
            }
        }
    }
}

// === impl AlpnProtocols ===

impl fmt::Debug for AlpnProtocols {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut dbg = f.debug_tuple("AlpnProtocols");
        for p in self.0.iter() {
            if let Ok(s) = std::str::from_utf8(p) {
                dbg.field(&s);
            } else {
                dbg.field(p);
            }
        }
        dbg.finish()
    }
}
