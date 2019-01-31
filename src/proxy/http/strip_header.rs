use std::marker::PhantomData;

use http::header::AsHeaderName;

use svc;

/// Wraps HTTP `Service` `Stack<T>`s so that a given header is removed from a
/// request or response.
#[derive(Clone, Debug)]
pub struct Layer<H, R> {
    header: H,
    _req_or_res: PhantomData<fn(R)>,
}

/// Wraps an HTTP `Service` so that a given header is removed from each
/// request or response.
#[derive(Clone, Debug)]
pub struct Stack<H, M, R> {
    header: H,
    inner: M,
    _req_or_res: PhantomData<fn(R)>,
}

#[derive(Clone, Debug)]
pub struct Service<H, S, R> {
    header: H,
    inner: S,
    _req_or_res: PhantomData<fn(R)>,
}

// === impl Layer ===

/// Call `request::layer(header)` or `response::layer(header)`.
fn layer<H, R>(header: H) -> Layer<H, R>
where
    H: AsHeaderName + Clone,
    R: Clone,
{
    Layer {
        header,
        _req_or_res: PhantomData,
    }
}

impl<H, T, M, R> svc::Layer<T, T, M> for Layer<H, R>
where
    H: AsHeaderName + Clone,
    M: svc::Stack<T>,
{
    type Value = <Stack<H, M, R> as svc::Stack<T>>::Value;
    type Error = <Stack<H, M, R> as svc::Stack<T>>::Error;
    type Stack = Stack<H, M, R>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            header: self.header.clone(),
            inner,
            _req_or_res: PhantomData,
        }
    }
}

// === impl Stack ===

impl<H, T, M, R> svc::Stack<T> for Stack<H, M, R>
where
    H: AsHeaderName + Clone,
    M: svc::Stack<T>,
{
    type Value = Service<H, M::Value, R>;
    type Error = M::Error;

    fn make(&self, t: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(t)?;
        let header = self.header.clone();
        Ok(Service {
            header,
            inner,
            _req_or_res: PhantomData,
        })
    }
}

pub mod request {
    use futures::Poll;
    use http;
    use http::header::AsHeaderName;

    use svc;

    pub fn layer<H>(header: H) -> super::Layer<H, ReqHeader>
    where
        H: AsHeaderName + Clone
    {
        super::layer(header)
    }

    /// Marker type used to specify that the `Request` headers should be stripped.
    #[derive(Clone, Debug)]
    pub enum ReqHeader {}

    impl<H, S, B> svc::Service<http::Request<B>> for super::Service<H, S, ReqHeader>
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
            req.headers_mut().remove(self.header.clone());
            self.inner.call(req)
        }
    }
}

pub mod response {
    use futures::{Future, Poll};
    use http;
    use http::header::AsHeaderName;

    use svc;

    pub fn layer<H>(header: H) -> super::Layer<H, ResHeader>
    where
        H: AsHeaderName + Clone
    {
        super::layer(header)
    }

    /// Marker type used to specify that the `Response` headers should be stripped.
    #[derive(Clone, Debug)]
    pub enum ResHeader {}

    pub struct ResponseFuture<F, H> {
        inner: F,
        header: H,
    }

    impl<H, S, B, Req> svc::Service<Req> for super::Service<H, S, ResHeader>
    where
        H: AsHeaderName + Clone,
        S: svc::Service<Req, Response = http::Response<B>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = ResponseFuture<S::Future, H>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, req: Req) -> Self::Future {
            ResponseFuture {
                inner: self.inner.call(req),
                header: self.header.clone(),
            }
        }
    }

    impl<F, H, B> Future for ResponseFuture<F, H>
    where
        F: Future<Item = http::Response<B>>,
        H: AsHeaderName + Clone,
    {
        type Item = F::Item;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let mut res = try_ready!(self.inner.poll());
            res.headers_mut().remove(self.header.clone());
            Ok(res.into())
        }
    }
}
