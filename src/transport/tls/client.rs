use crate::svc;
use crate::transport::{io::internal::Io, tls, BoxedIo, Connection};
use crate::Conditional;
use futures::{try_ready, Async, Future, Poll};
pub use rustls::ClientConfig as Config;
use std::sync::Arc;
use std::{fmt, io};
use tracing::trace;

pub trait HasConfig {
    fn tls_client_config(&self) -> Arc<Config>;
}

#[derive(Clone, Debug)]
pub struct Layer<L>(tls::Conditional<L>);

#[derive(Clone, Debug)]
pub struct Connect<L, C> {
    local: tls::Conditional<L>,
    inner: C,
}

/// A socket that is in the process of connecting.
pub enum ConnectFuture<L, F: Future> {
    Init {
        future: F,
        tls: tls::Conditional<(crate::identity::Name, L)>,
    },
    Handshake {
        future: tokio_rustls::Connect<F::Item>,
        server_name: crate::identity::Name,
    },
}

// === impl Layer ===

pub fn layer<L: HasConfig + Clone>(l: tls::Conditional<L>) -> Layer<L> {
    Layer(l)
}

impl<L, C> svc::Layer<C> for Layer<L>
where
    L: HasConfig + fmt::Debug + Clone,
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
impl<L, C, Target> svc::Service<Target> for Connect<L, C>
where
    Target: tls::HasPeerIdentity,
    L: HasConfig + fmt::Debug + Clone,
    C: svc::MakeConnection<Target>,
    C::Connection: Io + Send + 'static,
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
        let server_name = target.peer_identity();
        let tls = self.local.clone().and_then(|l| server_name.map(|n| (n, l)));
        ConnectFuture::Init {
            future: self.inner.make_connection(target),
            tls,
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
