use super::Conditional;
use crate::io::{self, BoxedIo};
use futures::{
    future::{Either, MapOk},
    prelude::*,
};
use linkerd2_identity as identity;
use linkerd2_stack::layer;
pub use rustls::ClientConfig as Config;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::{debug, trace};

pub trait HasConfig {
    fn tls_client_config(&self) -> Arc<Config>;
}

#[derive(Clone, Debug)]
pub struct Client<L, C> {
    local: Conditional<L>,
    inner: C,
}

type Connect<F, I> = MapOk<F, fn(I) -> BoxedIo>;
type Handshake = Pin<Box<dyn Future<Output = io::Result<BoxedIo>> + Send + 'static>>;

// === impl Client ===

impl<L: Clone, C> Client<L, C> {
    pub fn layer(local: Conditional<L>) -> impl layer::Layer<C, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            local: local.clone(),
        })
    }
}

impl<L, C, T> tower::Service<T> for Client<L, C>
where
    T: super::HasPeerIdentity,
    L: HasConfig + Clone,
    C: tower::Service<T, Error = io::Error>,
    C::Response: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
    C::Future: Send + 'static,
{
    type Response = BoxedIo;
    type Error = io::Error;
    type Future = Either<Connect<C::Future, C::Response>, Handshake>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let tls = match self.local.clone() {
            Conditional::Some(l) => tokio_rustls::TlsConnector::from(l.tls_client_config()),
            Conditional::None(reason) => {
                trace!(%reason, "Local identity disabled");
                return Either::Left(self.inner.call(target).map_ok(BoxedIo::new));
            }
        };
        let peer_identity = match target.peer_identity() {
            Conditional::Some(id) => id,
            Conditional::None(reason) => {
                debug!(%reason, "Peer does not support TLS");
                return Either::Left(self.inner.call(target).map_ok(BoxedIo::new));
            }
        };

        debug!(peer.identity = ?peer_identity, "Initiating TLS connection");
        let connect = self.inner.call(target);
        Either::Right(Box::pin(async move {
            let io = connect.await?;
            let io = tls.connect(peer_identity.as_dns_name_ref(), io).await?;
            Ok(BoxedIo::new(io))
        }))
    }
}

impl HasConfig for identity::CrtKey {
    fn tls_client_config(&self) -> Arc<Config> {
        identity::CrtKey::tls_client_config(self)
    }
}

impl HasConfig for identity::TrustAnchors {
    fn tls_client_config(&self) -> Arc<Config> {
        identity::TrustAnchors::tls_client_config(self)
    }
}
