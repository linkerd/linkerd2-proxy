use crate::{NegotiatedProtocol, ServerName};
use futures::prelude::*;
use linkerd_conditional::Conditional;
use linkerd_identity as id;
use linkerd_io as io;
use linkerd_stack::{layer, MakeConnection, NewService, Oneshot, Param, Service, ServiceExt};
use std::{
    fmt,
    ops::Deref,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use tracing::debug;

/// Describes the authenticated identity of a remote server.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ServerId(pub id::Id);

/// A stack parameter that configures a `Client` to establish a TLS connection.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ClientTls {
    pub server_name: ServerName,
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

#[pin_project::pin_project(project = ConnectProj)]
#[derive(Debug)]
pub enum Connect<F, I, H: Service<I>, M> {
    Connect(#[pin] F, Option<Conditional<H, NoClientTls>>),
    Handshake {
        #[pin]
        inner: Oneshot<H, I>,
        state: Option<(Conditional<(), NoClientTls>, M)>,
    },
}

#[derive(Clone, Debug)]
pub struct ConnectMeta<M> {
    pub socket: M,
    pub tls: Conditional<Option<NegotiatedProtocol>, NoClientTls>,
}

// === impl ClientTls ===

impl ClientTls {
    // XXX(ver) We'll have to change this when ServerIds are not necessarily DNS names.
    pub fn new(server_id: ServerId, server_name: ServerName) -> Self {
        Self {
            server_name,
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

impl<T, L, H, I, C> Service<T> for Client<L, C>
where
    T: Param<ConditionalClientTls>,
    L: NewService<ClientTls, Service = H>,
    C: MakeConnection<T, Error = io::Error>,
    C::Connection: Send + Unpin,
    C::Metadata: Send + Unpin,
    C::Future: Send + 'static,
    H: Service<C::Connection, Response = (I, Option<NegotiatedProtocol>), Error = io::Error>
        + Send
        + 'static,
    H::Future: Send + 'static,
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin,
{
    type Response = (io::EitherIo<C::Connection, I>, ConnectMeta<C::Metadata>);
    type Error = io::Error;
    type Future = Connect<C::Future, C::Connection, H, C::Metadata>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let handshake = match target.param() {
            Conditional::Some(tls) => Conditional::Some(self.identity.new_service(tls)),
            Conditional::None(reason) => {
                debug!(%reason, "Peer does not support TLS");
                Conditional::None(reason)
            }
        };

        let connect = self.inner.connect(target);
        Connect::Connect(connect, Some(handshake))
    }
}

impl<F, I, J, H, M> Future for Connect<F, I, H, M>
where
    F: TryFuture<Ok = (I, M), Error = io::Error>,
    H: Service<I, Response = (J, Option<NegotiatedProtocol>), Error = io::Error>,
{
    type Output = io::Result<(io::EitherIo<I, J>, ConnectMeta<M>)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match self.as_mut().project() {
                ConnectProj::Connect(fut, tls) => {
                    let (io, socket) = futures::ready!(fut.try_poll(cx))?;
                    match tls.take().expect("tls handshake must be set") {
                        Conditional::Some(tls) => self.set(Connect::Handshake {
                            inner: tls.oneshot(io),
                            state: Some((Conditional::Some(()), socket)),
                        }),
                        Conditional::None(reason) => {
                            let meta = ConnectMeta {
                                socket,
                                tls: Conditional::None(reason),
                            };
                            return Poll::Ready(Ok((io::EitherIo::Left(io), meta)));
                        }
                    }
                }
                ConnectProj::Handshake { inner, state } => {
                    let (io, alpn) = futures::ready!(inner.try_poll(cx))?;
                    debug!(
                        alpn = alpn
                            .as_ref()
                            .and_then(|NegotiatedProtocol(ref p)| std::str::from_utf8(p).ok())
                            .map(tracing::field::display)
                    );
                    let (tls, socket) = state.take().expect("metadata must be set");
                    let meta = ConnectMeta {
                        socket,
                        tls: tls.map(move |()| alpn),
                    };
                    return Poll::Ready(Ok((io::EitherIo::Right(io), meta)));
                }
            }
        }
    }
}

// === impl ServerId ===

impl From<id::Id> for ServerId {
    fn from(id: id::Id) -> Self {
        Self(id)
    }
}

impl From<ServerId> for id::Id {
    fn from(ServerId(id): ServerId) -> id::Id {
        id
    }
}

impl ServerId {
    pub fn to_str(&self) -> std::borrow::Cow<'_, str> {
        self.0.to_str()
    }
}

impl Deref for ServerId {
    type Target = id::Id;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FromStr for ServerId {
    type Err = linkerd_error::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        id::Id::from_str(s).map(Self)
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
