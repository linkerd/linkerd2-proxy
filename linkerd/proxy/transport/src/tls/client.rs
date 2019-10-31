use crate::io::BoxedIo;
use futures::{try_ready, Future, Poll};
use linkerd2_conditional::Conditional;
use linkerd2_identity as identity;
pub use rustls::ClientConfig as Config;
use std::io;
use std::sync::Arc;
use tokio::net::TcpStream;
use tracing::trace;

pub trait HasConfig {
    fn tls_client_config(&self) -> Arc<Config>;
}

#[derive(Clone, Debug)]
pub struct Layer<L>(super::Conditional<L>);

#[derive(Clone, Debug)]
pub struct Connect<L, C> {
    local: super::Conditional<L>,
    inner: C,
}

pub type Connection = BoxedIo;

/// A socket that is in the process of connecting.
pub enum ConnectFuture<L, F: Future> {
    Init {
        future: F,
        tls: super::Conditional<(identity::Name, L)>,
    },
    Handshake(tokio_rustls::Connect<F::Item>),
}

// === impl Layer ===

pub fn layer<L: HasConfig + Clone>(l: super::Conditional<L>) -> Layer<L> {
    Layer(l)
}

impl<L, C> tower::layer::Layer<C> for Layer<L>
where
    L: HasConfig + Clone,
{
    type Service = Connect<L, C>;

    fn layer(&self, inner: C) -> Self::Service {
        Connect {
            local: self.0.clone(),
            inner,
        }
    }
}

// === impl Connect ===

/// impl MakeConnection
impl<L, C, Target> tower::Service<Target> for Connect<L, C>
where
    Target: super::HasPeerIdentity,
    L: HasConfig + Clone,
    C: tower::MakeConnection<Target, Connection = TcpStream>,
    C::Future: Send + 'static,
    C::Error: ::std::error::Error + Send + Sync + 'static,
    C::Error: From<io::Error>,
{
    type Response = Connection;
    type Error = C::Error;
    type Future = ConnectFuture<L, C::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: Target) -> Self::Future {
        let peer_identity = target.peer_identity();
        let tls = self
            .local
            .clone()
            .and_then(|l| peer_identity.map(|n| (n, l)));
        ConnectFuture::Init {
            future: self.inner.make_connection(target),
            tls,
        }
    }
}

// ===== impl ConnectFuture =====

impl<L, F> Future for ConnectFuture<L, F>
where
    L: HasConfig,
    F: Future<Item = TcpStream>,
    F::Error: From<io::Error>,
{
    type Item = Connection;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                ConnectFuture::Init { future, tls } => {
                    let io = try_ready!(future.poll());

                    match tls {
                        Conditional::Some((peer_identity, local_tls)) => {
                            trace!(peer.id = %peer_identity, "initiating TLS");
                            ConnectFuture::Handshake(
                                tokio_rustls::TlsConnector::from(local_tls.tls_client_config())
                                    .connect(peer_identity.as_dns_name_ref(), io),
                            )
                        }
                        Conditional::None(reason) => {
                            trace!(%reason, "skipping TLS");
                            return Ok(Connection::new(io).into());
                        }
                    }
                }
                ConnectFuture::Handshake(ref mut fut) => {
                    let io = try_ready!(fut.poll());
                    trace!("established TLS");
                    return Ok(Connection::new(io).into());
                }
            };
        }
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
