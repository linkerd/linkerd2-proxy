use super::accept_error::AcceptError;
use futures::prelude::*;
use linkerd2_error::Error;
use linkerd2_proxy_core::Accept;
use linkerd2_proxy_transport::listen::Addrs;
use std::task::{Context, Poll};
use tower::util::ServiceExt;
use tower::Service;
use tracing::{info_span, Span};
use tracing_futures::{Instrument, Instrumented};

pub trait HasSpan {
    fn span(&self) -> Span;
}

/// Spawns a task that binds an `L`-typed listener with an `A`-typed
/// connection-accepting service.
///
/// The task is driven until shutdown is signaled.
pub async fn serve<A, C>(
    listen: impl Stream<Item = std::io::Result<C>>,
    accept: A,
    shutdown: impl Future,
) -> Result<(), Error>
where
    C: HasSpan,
    A: Accept<C> + Send + 'static,
    A::Future: Send + 'static,
    A::Error: Send + 'static,
    A::ConnectionError: Send + 'static,
    A::ConnectionFuture: Send + 'static,
{
    // Initialize tracing & log errors on the accept stack.
    let mut service = TraceAccept {
        accept: AcceptError::new(accept),
        span: Span::current(),
    }
    .into_service();

    let accept = async move {
        futures::pin_mut!(listen);
        loop {
            match listen.next().await {
                None => return Ok(()),
                Some(conn) => {
                    // If the listener returned an error, complete the task
                    let conn = conn?;

                    // Ready the service before dispatching the request to it.
                    //
                    // This allows the service to propagate errors and to exert backpressure on the
                    // listener. It also avoids a `Clone` requirement.
                    let accept = service.ready_and().await?.call(conn);

                    // Dispatch all of the work for a given connection onto a connection-specific task.
                    tokio::spawn(
                        async move {
                            match accept.await {
                                Ok(serve) => match serve.await {
                                    Ok(()) => {}
                                    Err(e) => match e {},
                                },
                                Err(e) => match e {},
                            }
                        }
                        .in_current_span(),
                    );
                }
            }
        }
    };

    // Stop the accept loop when the shutdown signal fires.
    //
    // This ensures that the accept service's readiness can't block shutdown.
    tokio::select! {
        res = accept => { res }
        _ = shutdown => { Ok(()) }
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
