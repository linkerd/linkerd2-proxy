use super::accept_error::AcceptError;
use futures::{future, try_ready, Future, Poll};
use linkerd2_drain as drain;
use linkerd2_error::Error;
use linkerd2_proxy_core::listen::{Accept, Listen, Serve};
use linkerd2_proxy_transport::listen::Addrs;
use tracing::{debug, info_span, Span};
use tracing_futures::{Instrument, Instrumented};

pub type Task = Box<dyn Future<Item = (), Error = Error> + Send + 'static>;

pub trait HasSpan {
    fn span(&self) -> Span;
}

/// Spawns a task that binds an `L`-typed listener with an `A`-typed
/// connection-accepting service.
///
/// The task is driven until the provided `drain` is notified.
pub fn serve<L, A>(listen: L, accept: A, drain: drain::Watch) -> Task
where
    L: Listen + Send + 'static,
    L::Connection: HasSpan,
    L::Error: std::error::Error + Send + 'static,
    A: Accept<L::Connection> + Send + 'static,
    A::Error: Send + 'static,
    A::Future: Send + 'static,
    A::Error: Into<Error>,
    A::ConnectionFuture: Send + 'static,
{
    // As soon as we get a shutdown signal, the listener task completes and
    // stops accepting new connections.
    Box::new(future::lazy(move || {
        debug!(listen.addr = %listen.listen_addr(), "serving");
        drain.watch(ServeAndSpawnUntilCancel::new(listen, accept), |s| {
            s.cancel()
        })
    }))
}

struct ServeAndSpawnUntilCancel<L: Listen, A: Accept<L::Connection>>(
    Option<Serve<L, TraceAccept<AcceptError<A>>, Instrumented<tokio::executor::DefaultExecutor>>>,
);

impl<L, A> ServeAndSpawnUntilCancel<L, A>
where
    L: Listen,
    L::Connection: HasSpan,
    A: Accept<L::Connection>,
    A::Error: 'static,
    A::Future: Send + 'static,
    A::ConnectionFuture: Send + 'static,
{
    fn new(listen: L, accept: A) -> Self {
        let exec = tokio::executor::DefaultExecutor::current().in_current_span();
        let accept = TraceAccept {
            accept: AcceptError::new(accept),
            span: Span::current(),
        };
        let serve = listen.serve(accept).with_executor(exec);
        ServeAndSpawnUntilCancel(Some(serve))
    }

    fn cancel(&mut self) {
        self.0 = None;
    }
}

impl<L, A> Future for ServeAndSpawnUntilCancel<L, A>
where
    L: Listen,
    L::Connection: HasSpan,
    A: Accept<L::Connection>,
    A::Future: Send + 'static,
    A::Error: Into<Error> + Send + 'static,
    A::ConnectionFuture: Send + 'static,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.0.as_mut() {
            Some(ref mut serve) => match try_ready!(serve.poll()) {},
            None => Ok(().into()),
        }
    }
}

struct TraceAccept<A> {
    accept: A,
    span: Span,
}

impl<C: HasSpan, A: Accept<C>> tower::Service<C> for TraceAccept<A> {
    type Response = A::ConnectionFuture;
    type Error = A::Error;
    type Future = Instrumented<A::Future>;

    fn poll_ready(&mut self) -> Poll<(), A::Error> {
        let _enter = self.span.enter();
        self.accept.poll_ready()
    }

    fn call(&mut self, conn: C) -> Self::Future {
        let span = conn.span();
        let _enter = span.enter();
        self.accept.accept(conn).in_current_span()
    }
}

impl<C> HasSpan for (Addrs, C) {
    fn span(&self) -> Span {
        // The local addr should be instrumented from the listener's context.
        info_span!(
            "accept",
            peer.addr = %self.0.peer(),
        )
    }
}
