use futures::Poll;
use http;

use super::h1;
use svc;

pub trait ShouldNormalizeUri {
    fn should_normalize_uri(&self) -> bool;
}

#[derive(Clone, Debug)]
pub struct Layer();

#[derive(Clone, Debug)]
pub struct Stack<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Service<S> {
    inner: S,
}

// === impl Layer ===

pub fn layer() -> Layer {
    Layer()
}

impl<T, M> svc::Layer<T, T, M> for Layer
where
    T: ShouldNormalizeUri,
    M: svc::Stack<T>,
{
    type Value = <Stack<M> as svc::Stack<T>>::Value;
    type Error = <Stack<M> as svc::Stack<T>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack { inner }
    }
}

// === impl Stack ===

impl<T, M> svc::Stack<T> for Stack<M>
where
    T: ShouldNormalizeUri,
    M: svc::Stack<T>,
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
