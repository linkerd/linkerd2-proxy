use super::accept_error::AcceptError;
use futures::{try_ready, Future, Poll};
use linkerd2_drain as drain;
use linkerd2_error::Error;
use linkerd2_proxy_core::listen::{Accept, Listen, Serve};
use linkerd2_task as task;
use tracing::Span;
use tracing_futures::{Instrument, Instrumented};

/// Spawns a task that binds an `S`-typed server with an `L`-typed listener until
/// a drain is signaled.
pub fn spawn<L, A>(listen: L, accept: A, drain: drain::Watch, span: Span)
where
    L: Listen + Send + 'static,
    L::Error: std::error::Error + Send + 'static,
    A: Accept<L::Connection> + Send + 'static,
    A::Error: 'static,
    A::Future: Send + 'static,
{
    // As soon as we get a shutdown signal, the listener task completes and
    // stops accepting new connections.
    task::spawn(
        drain
            .watch(
                ServeAndSpawnUntilCancel::new(listen, accept, span.clone()),
                |s| s.cancel(),
            )
            .map_err(|e| panic!("Server failed: {}", e))
            .instrument(span),
    );
}

struct ServeAndSpawnUntilCancel<L: Listen, A: Accept<L::Connection>>(
    Option<Serve<L, TraceAccept<AcceptError<A>>, Instrumented<tokio::executor::DefaultExecutor>>>,
);

impl<L, A> ServeAndSpawnUntilCancel<L, A>
where
    L: Listen,
    A: Accept<L::Connection>,
    A::Error: 'static,
    A::Future: Send + 'static,
{
    fn new(listen: L, accept: A, span: Span) -> Self {
        let exec = tokio::executor::DefaultExecutor::current().instrument(span.clone());
        let accept = TraceAccept {
            accept: AcceptError::new(accept),
            span,
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
    A: Accept<L::Connection>,
    A::Future: Send + 'static,
    A::Error: 'static,
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

impl<C, A: Accept<C>> tower::Service<C> for TraceAccept<A> {
    type Response = ();
    type Error = A::Error;
    type Future = Instrumented<A::Future>;

    fn poll_ready(&mut self) -> Poll<(), A::Error> {
        let _enter = self.span.enter();
        self.accept.poll_ready()
    }

    fn call(&mut self, conn: C) -> Self::Future {
        let _enter = self.span.enter();
        self.accept.accept(conn).instrument(self.span.clone())
    }
}
