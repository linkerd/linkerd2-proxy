use crate::{detect_sni::DetectIo, NegotiatedProtocol, ServerName};
use futures::prelude::*;
use linkerd_conditional::Conditional;
use linkerd_error::Error;
use linkerd_identity as id;
use linkerd_io::{self as io, EitherIo};
use linkerd_stack::{layer, ExtractParam, InsertParam, NewService, Param, Service, ServiceExt};
use std::{
    fmt,
    ops::Deref,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::{debug, trace};

/// Describes the authenticated identity of a remote client.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ClientId(pub id::Id);

/// Indicates a server-side connection's TLS status.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum ServerTls {
    Established {
        client_id: Option<ClientId>,
        negotiated_protocol: Option<NegotiatedProtocol>,
    },
    Passthru {
        sni: ServerName,
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

pub type Io<I, J> = EitherIo<I, DetectIo<J>>;

#[derive(Clone, Debug)]
pub struct NewDetectTls<L, P, N> {
    inner: N,
    params: P,
    _local_identity: std::marker::PhantomData<fn() -> L>,
}

#[derive(Clone, Debug)]
pub struct DetectTls<T, L, P, N> {
    target: T,
    local_identity: L,
    params: P,
    inner: N,
    sni: Option<ServerName>,
}

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

impl<T, L, P, N> NewService<(T, Option<ServerName>)> for NewDetectTls<L, P, N>
where
    P: ExtractParam<L, T> + Clone,
    N: Clone,
{
    type Service = DetectTls<T, L, P, N>;

    fn new_service(&self, t: (T, Option<ServerName>)) -> Self::Service {
        let (target, sni) = t;
        let local_identity = self.params.extract_param(&target);
        DetectTls {
            target,
            local_identity,
            sni,
            params: self.params.clone(),
            inner: self.inner.clone(),
        }
    }
}

impl<I, T, L, LIo, P, N, NSvc> Service<DetectIo<I>> for DetectTls<T, L, P, N>
where
    I: io::Peek + io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin + 'static,
    T: Clone + Send + 'static,
    P: InsertParam<ConditionalServerTls, T> + Clone + Send + Sync + 'static,
    P::Target: Send + 'static,
    L: Param<ServerName> + Clone + Send + 'static,
    L: Service<DetectIo<I>, Response = (ServerTls, LIo), Error = io::Error>,
    L::Future: Send,
    LIo: io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin + 'static,
    N: NewService<P::Target, Service = NSvc> + Clone + Send + 'static,
    NSvc: Service<Io<LIo, I>, Response = ()> + Send + 'static,
    NSvc::Error: Into<Error>,
    NSvc::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: DetectIo<I>) -> Self::Future {
        let target = self.target.clone();
        let params = self.params.clone();
        let new_accept = self.inner.clone();
        let sni = self.sni.clone();
        let tls = self.local_identity.clone();

        Box::pin(async move {
            let local_server_name = tls.param();
            let (peer, io) = match sni {
                // If we detected an SNI matching this proxy, terminate TLS.
                Some(sni) if sni == local_server_name => {
                    trace!("Identified local SNI");
                    let (peer, io) = tls.oneshot(io).await?;
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

// === impl ClientId ===

impl From<id::Id> for ClientId {
    fn from(n: id::Id) -> Self {
        Self(n)
    }
}

impl From<ClientId> for id::Id {
    fn from(ClientId(id): ClientId) -> id::Id {
        id
    }
}

impl Deref for ClientId {
    type Target = id::Id;

    fn deref(&self) -> &id::Id {
        &self.0
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for ClientId {
    type Err = linkerd_error::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        id::Id::from_str(s).map(Self)
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

// === impl ServerTls ===

impl ServerTls {
    pub fn client_id(&self) -> Option<&ClientId> {
        match self {
            ServerTls::Established { ref client_id, .. } => client_id.as_ref(),
            _ => None,
        }
    }
}
