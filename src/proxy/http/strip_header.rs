use futures::Poll;
use http;
use http::header::AsHeaderName;

use svc;

/// Wraps HTTP `Service` `Stack<T>`s so that a given header is removed from a
/// request.
#[derive(Debug, Clone)]
pub struct Layer<H> {
    header: H,
}

/// Wraps an HTTP `Service` so that a given header is removed from each
/// request.
#[derive(Clone, Debug)]
pub struct Stack<H, M> {
    header: H,
    inner: M,
}

#[derive(Clone, Debug)]
pub struct Service<H, S> {
    header: H,
    inner: S,
}

// === impl Layer ===

pub fn layer<H>(header: H) -> Layer<H>
where
    H: AsHeaderName + Clone
{
    Layer { header }
}

impl<H, T, M> svc::Layer<T, T, M> for Layer<H>
where
    H: AsHeaderName + Clone,
    M: svc::Stack<T>,
{
    type Value = <Stack<H, M> as svc::Stack<T>>::Value;
    type Error = <Stack<H, M> as svc::Stack<T>>::Error;
    type Stack = Stack<H, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            header: self.header.clone(),
            inner,
        }
    }
}

// === impl Stack ===

impl<H, T, M> svc::Stack<T> for Stack<H, M>
where
    H: AsHeaderName + Clone,
    M: svc::Stack<T>,
{
    type Value = Service<H, M::Value>;
    type Error = M::Error;

    fn make(&self, t: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(t)?;
        let header = self.header.clone();
        Ok(Service { header, inner })
    }
}

// === impl Service ===

impl<H, S, B> svc::Service<http::Request<B>> for Service<H, S>
where
    H: AsHeaderName + Clone,
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let header: H = self.header.clone();
        req.headers_mut().remove(header);
        self.inner.call(req)
    }
}
