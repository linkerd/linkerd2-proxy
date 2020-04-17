use super::accept_error::AcceptError;
// use futures::{future, try_ready, Future, Poll};
use linkerd2_drain as drain;
use linkerd2_error::Error;
use linkerd2_proxy_core::listen::{Accept, Listen, Serve};
use linkerd2_proxy_transport::listen::Addrs;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::{debug, info_span, Span};
use tracing_futures::{Instrument, Instrumented};

pub trait HasSpan {
    fn span(&self) -> Span;
}

/// Spawns a task that binds an `L`-typed listener with an `A`-typed
/// connection-accepting service.
///
/// The task is driven until the provided `drain` is notified.
pub async fn serve<L, A>(listen: L, accept: A, drain: drain::Watch) -> Result<(), Error>
where
    L: Listen + Send + 'static,
    L::Connection: HasSpan,
    L::Error: std::error::Error + Send + 'static,
    A: Accept<L::Connection> + Send + 'static,
    A::Error: 'static,
    A::Future: Send + 'static,
{
    // As soon as we get a shutdown signal, the listener task completes and
    // stops accepting new connections.
    debug!(listen.addr = %listen.listen_addr(), "serving");
    drain
        .watch(ServeAndSpawnUntilCancel::new(listen, accept), |s| {
            s.cancel()
        })
        .await
}

struct ServeAndSpawnUntilCancel<L: Listen, A: Accept<L::Connection>>(
    Option<Serve<L, TraceAccept<AcceptError<A>>>>,
);

impl<L, A> ServeAndSpawnUntilCancel<L, A>
where
    L: Listen,
    L::Connection: HasSpan,
    A: Accept<L::Connection>,
    A::Error: 'static,
    A::Future: Send + 'static,
{
    fn new(listen: L, accept: A) -> Self {
        let accept = TraceAccept {
            accept: AcceptError::new(accept),
            span: Span::current(),
        };
        let serve = listen.serve(accept);
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
    A::Error: 'static,
{
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.0.as_mut() {
            Some(ref mut serve) => match futures_03::ready!(serve.poll(cx)) {},
            None => Ok(().into()),
        }
    }
}

struct TraceAccept<A> {
    accept: A,
    span: Span,
}

impl<C: HasSpan, A: Accept<C>> tower::Service<C> for TraceAccept<A> {
    type Response = ();
    type Error = A::Error;
    type Future = Instrumented<A::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), A::Error>> {
        let _enter = self.span.enter();
        self.accept.poll_ready(cx)
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
