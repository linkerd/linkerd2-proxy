use futures::Poll;
use http;
use http::header::{IntoHeaderName, HeaderValue};

use svc;

/// Wraps HTTP `Service` `Stack<T>`s so that a displayable `T` is cloned into each request's
/// extensions.
#[derive(Debug, Clone)]
pub struct Layer<H> {
    header: H,
}

/// Wraps an HTTP `Service` so that the Stack's `T -typed target` is cloned into
/// each request's extensions.
#[derive(Clone, Debug)]
pub struct Stack<H, M> {
    header: H,
    inner: M,
}

#[derive(Clone, Debug)]
pub struct Service<H, S> {
    header: H,
    value: HeaderValue,
    inner: S,
}

// === impl Layer ===

pub fn layer<H>(header: H) -> Layer<H>
where
    H: IntoHeaderName + Clone
{
    Layer { header }
}

impl<H, T, M> svc::Layer<T, T, M> for Layer<H>
where
    H: IntoHeaderName + Clone,
    T: Clone + Send + Sync + 'static,
    HeaderValue: for<'t> From<&'t T>,
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
    H: IntoHeaderName + Clone,
    T: Clone + Send + Sync + 'static,
    HeaderValue: for<'t> From<&'t T>,
    M: svc::Stack<T>,
{
    type Value = Service<H, M::Value>;
    type Error = M::Error;

    fn make(&self, t: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(t)?;
        let header = self.header.clone();
        let value = t.into();
        Ok(Service { header, inner, value })
    }
}

// === impl Service ===

impl<H, S, B> svc::Service<http::Request<B>> for Service<H, S>
where
    H: IntoHeaderName + Clone,
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        req.headers_mut().insert(self.header.clone(), self.value.clone());
        self.inner.call(req)
    }
}
