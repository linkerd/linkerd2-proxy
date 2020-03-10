use futures::{try_ready, Future, Poll};
use linkerd2_stack::NewService;
use linkerd2_timeout::Timeout;
use std::time::Duration;

/// Implement on targets to determine if a service has a timeout.
pub trait HasTimeout {
    fn timeout(&self) -> Option<Duration>;
}

/// An HTTP-specific optional timeout layer.
///
/// The stack target must implement `HasTimeout`, and if a duration is
/// specified for the target, a timeout is applied waiting for HTTP responses.
///
/// Timeout errors are translated into `http::Response`s with appropiate
/// status codes.
#[derive(Clone, Debug, Default)]
pub struct MakeTimeoutLayer(());

#[derive(Clone, Debug)]
pub struct MakeTimeout<M> {
    inner: M,
}

pub struct MakeFuture<F> {
    inner: F,
    timeout: Option<Duration>,
}

impl<M> tower::layer::Layer<M> for MakeTimeoutLayer {
    type Service = MakeTimeout<M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeTimeout { inner }
    }
}

impl<T, M> NewService<T> for MakeTimeout<M>
where
    M: NewService<T>,
    T: HasTimeout,
{
    type Service = Timeout<M::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        match target.timeout() {
            Some(t) => Timeout::new(self.inner.new_service(target), t),
            None => Timeout::passthru(self.inner.new_service(target)),
        }
    }
}

impl<T, M> tower::Service<T> for MakeTimeout<M>
where
    M: tower::Service<T>,
    T: HasTimeout,
{
    type Response = Timeout<M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let timeout = target.timeout();
        let inner = self.inner.call(target);

        MakeFuture { inner, timeout }
    }
}

impl<F: Future> Future for MakeFuture<F> {
    type Item = Timeout<F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());

        let svc = match self.timeout {
            Some(t) => Timeout::new(inner, t),
            None => Timeout::passthru(inner),
        };

        Ok(svc.into())
    }
}
