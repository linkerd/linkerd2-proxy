use futures::Poll;
use http;
use std::marker::PhantomData;

use super::h1;
use svc;

pub trait ShouldNormalizeUri {
    fn should_normalize_uri(&self) -> bool;
}

pub struct Layer<T, M>(PhantomData<fn() -> (T, M)>);

pub struct Stack<T, N: svc::Stack<T>> {
    inner: N,
    _p: PhantomData<T>,
}

#[derive(Copy, Clone, Debug)]
pub struct Service<S> {
    inner: S,
}

// === impl Layer ===

impl<T, B, M> Layer<T, M>
where
    T: ShouldNormalizeUri,
    M: svc::Stack<T>,
    M::Value: svc::Service<Request = http::Request<B>>,
{
    pub fn new() -> Self {
        Layer(PhantomData)
    }
}

impl<T, B, M> Clone for Layer<T, M>
where
    T: ShouldNormalizeUri,
    M: svc::Stack<T>,
    M::Value: svc::Service<Request = http::Request<B>>,
{
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl<T, B, M> svc::Layer<T, T, M> for Layer<T, M>
where
    T: ShouldNormalizeUri,
    M: svc::Stack<T>,
    M::Value: svc::Service<Request = http::Request<B>>,
{
    type Value = <Stack<T, M> as svc::Stack<T>>::Value;
    type Error = <Stack<T, M> as svc::Stack<T>>::Error;
    type Stack = Stack<T, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            _p: PhantomData,
        }
    }
}

// === impl Stack ===

impl<T, B, M> Clone for Stack<T, M>
where
    T: ShouldNormalizeUri,
    M: svc::Stack<T> + Clone,
    M::Value: svc::Service<Request = http::Request<B>>,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, B, M> svc::Stack<T> for Stack<T, M>
where
    T: ShouldNormalizeUri,
    M: svc::Stack<T>,
    M::Value: svc::Service<Request = http::Request<B>>,
{
    type Value = svc::Either<Service<M::Value>, M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target)?;
        if target.should_normalize_uri() {
            Ok(svc::Either::A(Service { inner }))
        } else {
            Ok(svc::Either::B(inner))
        }
    }
}

// === impl Service ===

impl<S, B> svc::Service for Service<S>
where
    S: svc::Service<Request = http::Request<B>>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut request: S::Request) -> Self::Future {
        debug_assert!(
            request.version() != http::Version::HTTP_2,
            "normalize_uri must only be applied to HTTP/1"
        );
        h1::normalize_our_view_of_uri(&mut request);
        self.inner.call(request)
    }
}
