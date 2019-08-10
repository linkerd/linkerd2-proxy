use super::config::H2Settings;
use crate::app::identity::Local as LocalIdentity;
use crate::proxy::{http::Body, Accept, Server, Source};
use crate::svc;
use crate::transport::{Connection, GetOriginalDst, Listen, Peek};
use futures::{self, future, Future, Poll};
use http;
use linkerd2_never::Never;
use std::fmt;
use std::net::SocketAddr;
use tokio::executor::{DefaultExecutor, Executor};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::error;

#[allow(dead_code)] // rustc can't detect this is used in constraints
type Error = Box<dyn std::error::Error + Send + Sync>;

pub trait Spawn {
    fn spawn(self, drain: linkerd2_drain::Watch);
}

pub struct Proxy<A, T, C, R, B, G> {
    name: &'static str,
    listen: Listen<LocalIdentity, G>,
    accept: A,
    connect: C,
    router: R,
    h2_settings: H2Settings,
    _marker: std::marker::PhantomData<fn(T) -> B>,
}

impl<A, T, C, R, B, G> Proxy<A, T, C, R, B, G>
where
    Self: Spawn,
{
    pub fn new(
        name: &'static str,
        listen: Listen<LocalIdentity, G>,
        accept: A,
        connect: C,
        router: R,
        h2_settings: H2Settings,
    ) -> Self {
        Self {
            name,
            listen,
            accept,
            connect,
            router,
            h2_settings,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<A, T, C, R, B, G> Spawn for Proxy<A, T, C, R, B, G>
where
    A: Accept<Connection> + Send + 'static,
    A::Io: Peek + fmt::Debug + Send + 'static,
    T: From<SocketAddr> + Send + 'static,
    C: svc::Service<T> + Send + Clone + 'static,
    C::Response: AsyncRead + AsyncWrite + fmt::Debug + Send + 'static,
    C::Future: Send + 'static,
    C::Error: Into<Error>,
    R: svc::MakeService<
            Source,
            http::Request<Body>,
            Response = http::Response<B>,
            MakeError = Never,
        > + Clone
        + Send
        + 'static,
    R::Error: Into<Error> + Send,
    R::Service: Send,
    R::Future: Send,
    <R::Service as svc::Service<http::Request<Body>>>::Future: Send,
    B: hyper::body::Payload + Default + Send,
    G: GetOriginalDst + Send + 'static,
{
    fn spawn(self, drain: linkerd2_drain::Watch) {
        let server = Server::new(
            self.name,
            self.listen.local_addr(),
            self.accept,
            self.connect,
            self.router,
            drain.clone(),
        );

        let ctx = server.log().clone();
        let h2_settings = self.h2_settings;
        let accept_and_spawn = self
            .listen
            .listen_and_fold((), move |(), (connection, remote)| {
                let s = server.serve(connection, remote, h2_settings);
                // TODO: use trace spans for log contexts.
                // .instrument(info_span!("conn", %remote));
                // Logging context is configured by the server.
                let mut exec = DefaultExecutor::current();
                future::result(
                    exec.spawn(Box::new(s))
                        .map_err(linkerd2_task::Error::into_io),
                )
            })
            .map_err(|e| error!("failed to accept connection: {}", e));

        // TODO: use trace spans for log contexts.
        // .instrument(info_span!("proxy", server = %proxy_name, local = %listen_addr));

        let accept_until = Cancelable {
            future: ctx.future(accept_and_spawn),
            canceled: false,
        };

        // As soon as we get a shutdown signal, the listener
        // is canceled immediately.
        linkerd2_task::spawn(drain.watch(accept_until, |accept| {
            accept.canceled = true;
        }));
    }
}

/// Can cancel a future by setting a flag.
///
/// Used to 'watch' the accept futures, and close the listeners
/// as soon as the shutdown signal starts.
struct Cancelable<F> {
    future: F,
    canceled: bool,
}

impl<F> Future for Cancelable<F>
where
    F: Future<Item = ()>,
{
    type Item = ();
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.canceled {
            Ok(().into())
        } else {
            self.future.poll()
        }
    }
}
