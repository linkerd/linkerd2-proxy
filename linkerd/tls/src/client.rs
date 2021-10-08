use crate::{HasNegotiatedProtocol, NegotiatedProtocolRef};
use futures::{
    future::{Either, MapOk},
    prelude::*,
};
use linkerd_conditional::Conditional;
use linkerd_identity as id;
use linkerd_io as io;
use linkerd_stack::{layer, NewService, Param, Service, ServiceExt};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
pub use tokio_rustls::client::TlsStream;
use tracing::debug;

/// A newtype for target server identities.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ServerId(pub id::Name);

/// A stack parameter that configures a `Client` to establish a TLS connection.
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

#[derive(Clone, Debug)]
pub struct Client<L, C> {
    identity: L,
    inner: C,
}

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
    pub fn layer(identity: L) -> impl layer::Layer<C, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            identity: identity.clone(),
        })
    }
}

impl<T, L, H, C> Service<T> for Client<L, C>
where
    T: Param<ConditionalClientTls>,
    L: NewService<ClientTls, Service = H>,
    C: Service<T, Error = io::Error>,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
    C::Future: Send + 'static,
    H: Service<C::Response, Error = io::Error> + Send + 'static,
    H::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + HasNegotiatedProtocol,
    H::Future: Send + 'static,
{
    type Response = io::EitherIo<C::Response, H::Response>;
    type Error = io::Error;
    type Future = Either<
        MapOk<C::Future, fn(C::Response) -> io::EitherIo<C::Response, H::Response>>,
        Pin<
            Box<
                dyn Future<Output = io::Result<io::EitherIo<C::Response, H::Response>>>
                    + Send
                    + 'static,
            >,
        >,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let handshake = match target.param() {
            Conditional::Some(tls) => self.identity.new_service(tls),
            Conditional::None(reason) => {
                debug!(%reason, "Peer does not support TLS");
                return Either::Left(self.inner.call(target).map_ok(io::EitherIo::Left));
            }
        };

        let connect = self.inner.call(target);
        Either::Right(Box::pin(async move {
            let io = connect.await?;
            let io = handshake.oneshot(io).await?;
            debug!(
                alpn = io
                    .negotiated_protocol()
                    .and_then(|NegotiatedProtocolRef(p)| std::str::from_utf8(p).ok())
                    .map(tracing::field::display)
            );
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

impl AsRef<id::Name> for ServerId {
    fn as_ref(&self) -> &id::Name {
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
