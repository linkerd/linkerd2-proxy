//! A middleware that boxes an inner service and its errors.
//!
//! Similar to `tower::util::BoxService`, but it also takes responsibility for
//! boxing the error type.

#![deny(warnings, rust_2018_idioms)]

use futures::{Future, Poll};
use linkerd2_error::Error;

pub struct Layer<A, B> {
    _marker: std::marker::PhantomData<fn(A) -> B>,
}

pub struct BoxService<A, B>(
    Box<dyn tower::Service<A, Response = B, Error = Error, Future = ResponseFuture<B>> + Send>,
);

pub type ResponseFuture<B> = Box<dyn Future<Item = B, Error = Error> + Send + 'static>;

impl<A, B> Clone for Layer<A, B> {
    fn clone(&self) -> Self {
        Self {
            _marker: self._marker,
        }
    }
}

impl<A, B> Layer<A, B>
where
    A: 'static,
    B: 'static,
{
    pub fn new() -> Self {
        Layer {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<S, A> tower::layer::Layer<S> for Layer<A, S::Response>
where
    A: 'static,
    S: tower::Service<A> + Send + 'static,
    S::Response: 'static,
    S::Future: Send + 'static,
    S::Error: Into<Error> + 'static,
{
    type Service = BoxService<A, S::Response>;

    fn layer(&self, inner: S) -> Self::Service {
        BoxService::new(inner)
    }
}

struct Inner<S, A, B> {
    service: S,
    _marker: std::marker::PhantomData<fn(A) -> B>,
}

impl<A: 'static, B: 'static> BoxService<A, B> {
    fn new<S>(service: S) -> Self
    where
        S: tower::Service<A, Response = B> + Send + 'static,
        S::Future: Send + 'static,
        S::Error: Into<Error> + 'static,
    {
        BoxService(Box::new(Inner {
            service,
            _marker: std::marker::PhantomData,
        }))
    }
}

impl<A: 'static, B: 'static> tower::Service<A> for BoxService<A, B> {
    type Response = B;
    type Error = Error;
    type Future = ResponseFuture<B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: A) -> Self::Future {
        self.0.call(req)
    }
}

impl<S, A> tower::Service<A> for Inner<S, A, S::Response>
where
    S: tower::Service<A>,
    S::Response: 'static,
    S::Error: Into<Error> + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Response>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: A) -> Self::Future {
        let future = self.service.call(req);
        Box::new(future.map_err(Into::into))
    }
}

impl<S: Clone, A, B> Clone for Inner<S, A, B> {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            _marker: self._marker,
        }
    }
}
