use super::{
    h1,
    settings::{HasSettings, Settings},
};
use crate::svc;
use futures::{try_ready, Future, Poll};
use http;

#[derive(Clone, Debug)]
pub struct Stack<N> {
    inner: N,
}

pub struct MakeFuture<F> {
    inner: F,
    should_normalize_uri: bool,
}

#[derive(Clone, Debug)]
pub struct Service<S> {
    inner: S,
}

fn should_normalize_uri(settings: &Settings) -> bool {
    !settings.is_http2() && !settings.was_absolute_form()
}

// === impl Layer ===

pub fn layer<M>() -> impl svc::Layer<M, Service = Stack<M>> + Copy {
    svc::layer::mk(|inner| Stack { inner })
}

// === impl Stack ===

impl<T, M> svc::Service<T> for Stack<M>
where
    T: HasSettings,
    M: svc::Service<T>,
{
    type Response = svc::Either<Service<M::Response>, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), M::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let should_normalize_uri = should_normalize_uri(target.http_settings());
        let inner = self.inner.call(target);

        MakeFuture {
            inner,
            should_normalize_uri,
        }
    }
}

// === impl MakeFuture ===

impl<F: Future> Future for MakeFuture<F> {
    type Item = svc::Either<Service<F::Item>, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());

        if self.should_normalize_uri {
            Ok(svc::Either::A(Service { inner }).into())
        } else {
            Ok(svc::Either::B(inner).into())
        }
    }
}

// === impl Service ===

impl<S, B> svc::Service<http::Request<B>> for Service<S>
where
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        debug_assert!(
            request.version() != http::Version::HTTP_2,
            "normalize_uri must only be applied to HTTP/1"
        );
        h1::normalize_our_view_of_uri(&mut request);
        self.inner.call(request)
    }
}
