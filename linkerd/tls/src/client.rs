use crate::ReasonForNoPeerName;
use futures::{
    future::{Either, MapOk},
    prelude::*,
};
use linkerd_conditional::Conditional;
use linkerd_identity as id;
use linkerd_io as io;
use linkerd_stack::layer;
use std::{
    fmt,
    future::Future,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};
use tokio_rustls::client::TlsStream;
use tracing::{debug, trace};

/// A marker type for target server identities.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ServerId(pub id::Name);

pub type Config = Arc<rustls::ClientConfig>;

#[derive(Clone, Debug)]
pub struct Client<L, C> {
    local: Option<L>,
    inner: C,
}

type Connect<F, I> = MapOk<F, fn(I) -> io::EitherIo<I, TlsStream<I>>>;
type Handshake<I> =
    Pin<Box<dyn Future<Output = io::Result<io::EitherIo<I, TlsStream<I>>>> + Send + 'static>>;

pub type Io<T> = io::EitherIo<T, TlsStream<T>>;

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
    L: Clone,
    for<'l> &'l L: Into<Config>,
    for<'t> &'t T: Into<Conditional<ServerId, ReasonForNoPeerName>>,
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
        let tls = match self.local.as_ref() {
            Some(l) => tokio_rustls::TlsConnector::from(l.into()),
            None => {
                trace!("Local identity disabled");
                return Either::Left(self.inner.call(target).map_ok(io::EitherIo::Left));
            }
        };
        let server_id = match (&target).into() {
            Conditional::Some(ServerId(id)) => id,
            Conditional::None(reason) => {
                debug!(%reason, "Peer does not support TLS");
                return Either::Left(self.inner.call(target).map_ok(io::EitherIo::Left));
            }
        };

        debug!(target.id = ?server_id, "Initiating TLS connection");
        let connect = self.inner.call(target);
        Either::Right(Box::pin(async move {
            let io = connect.await?;
            let io = tls.connect((&server_id).into(), io).await?;
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

impl Into<id::Name> for ServerId {
    fn into(self) -> id::Name {
        self.0
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
