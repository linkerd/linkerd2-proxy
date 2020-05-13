use crate::io::BoxedIo;
use futures_03::{
    compat::{Compat01As03, Future01CompatExt},
    TryFuture,
};
use linkerd2_conditional::Conditional;
use linkerd2_identity as identity;
use pin_project::{pin_project, project};
pub use rustls::ClientConfig as Config;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::net::TcpStream;
use tracing::{debug, trace};

pub trait HasConfig {
    fn tls_client_config(&self) -> Arc<Config>;
}

#[derive(Clone, Debug)]
pub struct ConnectLayer<L>(super::Conditional<L>);

#[derive(Clone, Debug)]
pub struct Connect<L, C> {
    local: super::Conditional<L>,
    inner: C,
}

pub type Connection = BoxedIo;

/// A socket that is in the process of connecting.
#[pin_project]
pub struct ConnectFuture<L, F: TryFuture> {
    #[pin]
    state: ConnectState<L, F>,
}
#[pin_project]
enum ConnectState<L, F: TryFuture> {
    Init {
        #[pin]
        future: F,
        tls: super::Conditional<(identity::Name, L)>,
    },
    Handshake(#[pin] tokio_rustls::Connect<F::Ok>),
}

// === impl ConnectLayer ===

impl<L> ConnectLayer<L> {
    pub fn new(l: super::Conditional<L>) -> ConnectLayer<L> {
        ConnectLayer(l)
    }
}

impl<L: Clone, C> tower::layer::Layer<C> for ConnectLayer<L> {
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
impl<L, C, T> tower::Service<T> for Connect<L, C>
where
    T: super::HasPeerIdentity,
    L: HasConfig + Clone,
    C: tower::Service<T, Response = TcpStream>,
    C::Future: Send + 'static,
    C::Error: ::std::error::Error + Send + Sync + 'static,
    C::Error: From<io::Error>,
{
    type Response = Connection;
    type Error = C::Error;
    type Future = ConnectFuture<L, C::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let peer_identity = target.peer_identity();
        debug!(peer.identity = ?peer_identity);
        let tls = self
            .local
            .clone()
            .and_then(|l| peer_identity.map(|n| (n, l)));
        ConnectFuture {
            state: ConnectState::Init {
                future: self.inner.call(target),
                tls,
            },
        }
    }
}

// ===== impl ConnectFuture =====

impl<L, F> Future for ConnectFuture<L, F>
where
    L: HasConfig,
    F: TryFuture<Ok = TcpStream>,
    F::Error: From<io::Error>,
{
    type Output = Result<Connection, F::Error>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            #[project]
            match this.state.as_mut().project() {
                ConnectState::Init { future, tls } => {
                    let io = futures_03::ready!(future.try_poll(cx))?;

                    match tls {
                        Conditional::Some((peer_identity, local_tls)) => {
                            trace!(peer.id = %peer_identity, "initiating TLS");
                            let handshake =
                                tokio_rustls::TlsConnector::from(local_tls.tls_client_config())
                                    .connect(peer_identity.as_dns_name_ref(), io);
                            this.state.set(ConnectState::Handshake(handshake));
                        }
                        Conditional::None(reason) => {
                            trace!(%reason, "skipping TLS");
                            return Poll::Ready(Ok(Connection::new(io)));
                        }
                    }
                }
                ConnectState::Handshake(fut) => {
                    let io = futures_03::ready!(fut.poll(cx))?;
                    trace!("established TLS");
                    return Poll::Ready(Ok(Connection::new(io)));
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
