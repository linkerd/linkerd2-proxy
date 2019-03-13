use futures::{Async, Future, Poll};
use std::io;
use std::sync::Arc;

use identity;
use svc;
use transport::{connect, tls, BoxedIo, Connection};

pub trait HasConfig {
    fn tls_client_config(&self) -> Arc<rustls::ClientConfig>;
}

#[derive(Clone, Debug)]
pub struct Layer<L>(L);

#[derive(Clone, Debug)]
pub struct Stack<L, S> {
    local: L,
    inner: S,
}

#[derive(Clone, Debug)]
pub struct Connect<L, C> {
    inner: C,
    tls: tls::Conditional<(identity::Name, L)>,
}

/// A socket that is in the process of connecting.
pub enum ConnectFuture<L, F: Future> {
    Init {
        future: F,
        tls: tls::Conditional<(identity::Name, L)>,
    },
    Upgrade {
        future: tls::tokio_rustls::Connect<F::Item>,
        server_name: identity::Name,
    },
}

// === impl Layer ===

pub fn layer<L: HasConfig + Clone>(local: L) -> Layer<L> {
    Layer(local)
}

impl<T, L, S> svc::Layer<T, T, S> for Layer<L>
where
    L: HasConfig,
    T: tls::HasPeerIdentity,
    S: svc::Stack<T> + Clone,
    S::Value: connect::Connect + Clone + Send + Sync + 'static,
    <S::Value as connect::Connect>::Connected: Send + 'static,
    <S::Value as connect::Connect>::Future: Send + 'static,
    <S::Value as connect::Connect>::Error: ::std::error::Error + Send + Sync + 'static,
{
    type Value = <Stack<S> as svc::Stack<T>>::Value;
    type Error = <Stack<S> as svc::Stack<T>>::Error;
    type Stack = Stack<S>;

    fn bind(&self, inner: S) -> Self::Stack {
        Stack {
            inner,
            local: self.0.clone(),
        }
    }
}

// === impl Stack ===

impl<T, L, S> svc::Stack<T> for Stack<L, S>
where
    L: HasConfig,
    T: tls::HasPeerIdentity,
    S: svc::Stack<T> + Clone,
    S::Value: connect::Connect + Clone + Send + Sync + 'static,
    <S::Value as connect::Connect>::Connected: Send + 'static,
    <S::Value as connect::Connect>::Future: Send + 'static,
    <S::Value as connect::Connect>::Error: ::std::error::Error + Send + Sync + 'static,
{
    type Value = Connect<S::Value>;
    type Error = S::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target)?;
        let server_name = target.peer_identity();
        let tls = self.local.clone().and_then(|l| server_name.map(|n| (n, l)));
        Ok(Connect { inner, tls })
    }
}

// === impl Connect ===

impl<L, C> connect::Connect for Connect<L, C>
where
    L: HasConfig,
    C: connect::Connect + Clone + Send + Sync + 'static,
    C::Connected: Send + 'static,
    C::Future: Send + 'static,
    C::Error: ::std::error::Error + Send + Sync + 'static,
{
    type Connected = Connection;
    type Error = C::Error;
    type Future = ConnectFuture<L, C::Future>;

    fn connect(&self) -> Self::Future {
        ConnectFuture::Init {
            future: self.inner.connect(),
            tls: self.tls.clone(),
        }
    }
}

// ===== impl ConnectFuture =====

impl<F: Future> Future for ConnectFuture<F> {
    type Item = Connection;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                ConnectFuture::Init { future, tls } => {
                    let io = try_ready!(future.poll());

                    trace!("ConnectFuture: state=plaintext; tls={:?};", tls);
                    match tls {
                        tls::Conditional::Some((server_name, local_tls)) => {
                            trace!("plaintext connection established; trying to upgrade");
                            let future = tls::Connector::from(local_tls.tls_client_config())
                                .connect(server_name.as_dns_name_ref());
                            ConnectFuture::UpgradeToTls {
                                future,
                                server_name,
                            }
                        }
                        tls::Conditional::None(why) => {
                            trace!("plaintext connection established; no TLS ({:?})", why);
                            return Ok(Async::Ready(tls::Connection::plain(io, why)));
                        }
                    }
                }
                ConnectFuture::UpgradeToTls {
                    future,
                    server_name,
                } => {
                    let io = try_ready!(future.poll());
                    let c = Connection::tls(BoxedIo::new(io), server_name.clone());
                    return Ok(Async::Ready(c));
                }
            };
        }
    }
}
