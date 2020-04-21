#![deny(warnings, rust_2018_idioms)]
use futures::{
    compat::{Compat, Compat01As03, Future01CompatExt},
    TryFutureExt,
};
use linkerd2_error::Error;
use linkerd2_stack::Proxy;
use pin_project::{pin_project, project};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time;
use tokio_connect::Connect;

pub mod error;
mod failfast;
mod idle;
// mod probe_ready;

pub use self::failfast::{FailFast, FailFastError, FailFastLayer};
pub use self::idle::{Idle, IdleError, IdleLayer};
/// A timeout that wraps an underlying operation.
#[derive(Debug, Clone)]
pub struct Timeout<T> {
    inner: T,
    duration: Option<Duration>,
}

#[pin_project]
pub enum TimeoutFuture<F> {
    Passthru(#[pin] F),
    Timeout(#[pin] time::Timeout<F>, Duration),
}

//===== impl Timeout =====

impl<T> Timeout<T> {
    /// Construct a new `Timeout` wrapping `inner`.
    pub fn new(inner: T, duration: Duration) -> Self {
        Timeout {
            inner,
            duration: Some(duration),
        }
    }

    pub fn passthru(inner: T) -> Self {
        Timeout {
            inner,
            duration: None,
        }
    }
}

impl<P, S, Req> Proxy<Req, S> for Timeout<P>
where
    P: Proxy<Req, S>,
    S: tower::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = Error;
    type Future = TimeoutFuture<P::Future>;

    fn proxy(&self, svc: &mut S, req: Req) -> Self::Future {
        let inner = self.inner.proxy(svc, req);
        match self.duration {
            None => TimeoutFuture::Passthru(inner),
            Some(t) => TimeoutFuture::Timeout(time::timeout(t, inner), t),
        }
    }
}

impl<S, Req> tower::Service<Req> for Timeout<S>
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = TimeoutFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let inner = self.inner.call(req);
        match self.duration {
            None => TimeoutFuture::Passthru(inner),
            Some(t) => TimeoutFuture::Timeout(time::timeout(t, inner), t),
        }
    }
}

impl<C> Connect for Timeout<C>
where
    C: Connect,
    C::Error: Into<Error>,
{
    type Connected = C::Connected;
    type Error = Error;
    type Future = Compat<TimeoutFuture<Compat01As03<C::Future>>>;

    fn connect(&self) -> Self::Future {
        let inner = self.inner.connect();
        match self.duration {
            None => TimeoutFuture::Passthru(inner.compat()),
            Some(t) => TimeoutFuture::Timeout(time::timeout(t, inner.compat()), t),
        }
        .compat()
    }
}

impl<F, T, E> Future for TimeoutFuture<F>
where
    F: Future<Output = Result<T, E>>,
    E: Into<Error>,
{
    type Output = Result<T, Error>;
    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        #[project]
        match self.project() {
            TimeoutFuture::Passthru(f) => f.poll(cx).map_err(Into::into),
            TimeoutFuture::Timeout(f, duration) => {
                // If the `timeout` future failed, the error is aways "elapsed";
                // errors from the underlying future will be in the success arm.
                let ready = futures::ready!(f.poll(cx))
                    .map_err(|_| error::ResponseTimeout(*duration).into());
                // If the inner future failed but the timeout was not elapsed,
                // then `ready` will be an `Ok(Err(e))`, so we need to convert
                // the inner error as well.
                let ready = ready.and_then(|x| x.map_err(Into::into));
                Poll::Ready(ready)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::Error;
    use std::task::Poll;
    use tower::Service;

    pub(crate) async fn assert_svc_ready<S, R>(service: &mut S)
    where
        S: Service<R>,
        S::Error: std::fmt::Debug,
    {
        futures::future::poll_fn(|cx| match service.poll_ready(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(()),
            poll => panic!("service must be ready: {:?}", poll),
        })
        .await;
    }

    pub(crate) async fn assert_svc_pending<S, R>(service: &mut S)
    where
        S: Service<R>,
        S::Error: std::fmt::Debug,
    {
        futures::future::poll_fn(|cx| match service.poll_ready(cx) {
            Poll::Pending => Poll::Ready(()),
            poll => panic!("service must be pending: {:?}", poll),
        })
        .await;
    }

    pub(crate) async fn assert_svc_error<E, S, R>(service: &mut S)
    where
        E: std::error::Error + 'static,
        S: Service<R, Error = Error>,
    {
        futures::future::poll_fn(|cx| match service.poll_ready(cx) {
            Poll::Ready(Err(e)) => {
                assert!(
                    e.is::<E>(),
                    "error was not expected type\n  expected: {}\n    actual: {}",
                    std::any::type_name::<E>(),
                    e
                );
                Poll::Ready(())
            }
            poll => panic!("service must be errored: {:?}", poll),
        })
        .await;
    }
}
