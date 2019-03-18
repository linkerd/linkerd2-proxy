use futures::{Async, Future, Poll};
use std::sync::Arc;
use std::{fmt, io};

use identity;
use svc;
use transport::{connect, io::internal::Io, tls, BoxedIo, Connection};
use Conditional;

pub use super::rustls::ClientConfig as Config;

pub trait HasConfig {
    fn tls_client_config(&self) -> Arc<Config>;
}

#[derive(Clone, Debug)]
pub struct Layer<L>(tls::Conditional<L>);

#[derive(Clone, Debug)]
pub struct Stack<L, S> {
    local: tls::Conditional<L>,
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
    Handshake {
        future: tls::tokio_rustls::Connect<F::Item>,
        server_name: identity::Name,
    },
}

// === impl Layer ===

pub fn layer<L: HasConfig + Clone>(l: tls::Conditional<L>) -> Layer<L> {
    Layer(l)
}

impl<T, L, S> svc::Layer<T, T, S> for Layer<L>
where
    L: HasConfig + fmt::Debug + Clone,
    T: tls::HasPeerIdentity,
    S: svc::Stack<T> + Clone,
    S::Value: connect::Connect + Clone + Send + Sync + 'static,
    <S::Value as connect::Connect>::Connected: Send + 'static,
    <S::Value as connect::Connect>::Future: Send + 'static,
    <S::Value as connect::Connect>::Error: ::std::error::Error + Send + Sync + 'static,
{
    type Value = <Stack<L, S> as svc::Stack<T>>::Value;
    type Error = <Stack<L, S> as svc::Stack<T>>::Error;
    type Stack = Stack<L, S>;

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
    L: HasConfig + fmt::Debug + Clone,
    T: tls::HasPeerIdentity,
    S: svc::Stack<T> + Clone,
    S::Value: connect::Connect + Clone + Send + Sync + 'static,
    <S::Value as connect::Connect>::Connected: Send + 'static,
    <S::Value as connect::Connect>::Future: Send + 'static,
    <S::Value as connect::Connect>::Error: ::std::error::Error + Send + Sync + 'static,
{
    type Value = Connect<L, S::Value>;
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
    L: HasConfig + fmt::Debug + Clone,
    C: connect::Connect + Clone + Send + Sync + 'static,
    C::Connected: Io + Send + 'static,
    C::Future: Send + 'static,
    C::Error: ::std::error::Error + Send + Sync + 'static,
    C::Error: From<io::Error>,
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

impl<L, F> Future for ConnectFuture<L, F>
where
    L: HasConfig + fmt::Debug,
    F: Future,
    F::Item: Io + 'static,
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
                        Conditional::Some((server_name, local_tls)) => {
                            trace!("initiating TLS to {}", server_name.as_ref());
                            let future = tls::Connector::from(local_tls.tls_client_config())
                                .connect(server_name.as_dns_name_ref(), io);
                            ConnectFuture::Handshake {
                                future,
                                server_name: server_name.clone(),
                            }
                        }
                        Conditional::None(why) => {
                            trace!("skipping TLS ({:?})", why);
                            return Ok(Async::Ready(tls::Connection::plain(io, *why)));
                        }
                    }
                }
                ConnectFuture::Handshake {
                    future,
                    server_name,
                } => {
                    let io = try_ready!(future.poll());
                    let io = BoxedIo::new(super::TlsIo::from(io));
                    trace!("established TLS to {}", server_name.as_ref());
                    let c = Connection::tls(io, Conditional::Some(server_name.clone()));
                    return Ok(Async::Ready(c));
                }
            };
        }
    }
}
