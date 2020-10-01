use futures::{ready, TryFuture, TryFutureExt};
use linkerd2_error::Error;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Implements a `Service` from a `Future` that produces a `Service`.
#[derive(Debug)]
pub struct FutureService<F, S> {
    inner: Inner<F, S>,
}

#[derive(Debug)]
enum Inner<F, S> {
    Future(F),
    Service(S),
}

// === impl FutureService ===

impl<F, S> FutureService<F, S> {
    pub fn new(fut: F) -> Self {
        Self {
            inner: Inner::Future(fut),
        }
    }
}

impl<F, S, Req> tower::Service<Req> for FutureService<F, S>
where
    F: TryFuture<Ok = S> + Unpin,
    F::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = futures::future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            self.inner = match self.inner {
                Inner::Future(ref mut fut) => {
                    let fut = Pin::new(fut);
                    let svc = ready!(fut.try_poll(cx).map_err(Into::into)?);
                    Inner::Service(svc)
                }
                Inner::Service(ref mut svc) => return svc.poll_ready(cx).map_err(Into::into),
            };
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        if let Inner::Service(ref mut svc) = self.inner {
            return svc.call(req).map_err(Into::into);
        }

        panic!("Called before ready");
    }
}
