use crate::svc;
use futures::{try_ready, Future, Poll};
use http::header::{AsHeaderName, HeaderValue};
use std::{fmt, marker::PhantomData};
use tracing::trace;

/// A function used to get the header value for a given Stack target.
type GetHeader<T> = fn(&T) -> Option<HeaderValue>;

/// Wraps HTTP `Service` `Stack<T>`s so that a given header is removed from a
/// request or response.
#[derive(Clone)]
pub struct Layer<H, T, R> {
    header: H,
    get_header: GetHeader<T>,
    _req_or_res: PhantomData<fn(R)>,
}

/// Wraps an HTTP `Service` so that a given header is added from each request
/// or response.
#[derive(Clone)]
pub struct Stack<H, T, M, R> {
    header: H,
    get_header: GetHeader<T>,
    inner: M,
    _req_or_res: PhantomData<fn(R)>,
}

pub struct MakeFuture<F, H, R> {
    header: Option<(H, HeaderValue)>,
    inner: F,
    _req_or_res: PhantomData<fn(R)>,
}

#[derive(Clone, Debug)]
pub struct Service<H, S, R> {
    header: H,
    value: HeaderValue,
    inner: S,
    _req_or_res: PhantomData<fn(R)>,
}

// === impl Layer ===

/// Call `request::layer(header)` or `response::layer(header)`.
fn layer<H, T, R>(header: H, get_header: GetHeader<T>) -> Layer<H, T, R>
where
    H: AsHeaderName + Clone,
    R: Clone,
{
    Layer {
        header,
        get_header,
        _req_or_res: PhantomData,
    }
}

impl<H, T, M, R> svc::Layer<M> for Layer<H, T, R>
where
    H: AsHeaderName + Clone + fmt::Debug,
    T: fmt::Debug,
    M: svc::Service<T>,
{
    type Service = Stack<H, T, M, R>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            header: self.header.clone(),
            get_header: self.get_header,
            inner,
            _req_or_res: PhantomData,
        }
    }
}

// === impl Stack ===

/// impl MakeService
impl<H, T, M, R> svc::Service<T> for Stack<H, T, M, R>
where
    H: AsHeaderName + Clone + fmt::Debug,
    T: fmt::Debug,
    M: svc::Service<T>,
{
    type Response = svc::Either<Service<H, M::Response, R>, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future, H, R>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, t: T) -> Self::Future {
        let header = if let Some(value) = (self.get_header)(&t) {
            Some((self.header.clone(), value))
        /*
        svc::Either::A(Service {
            header: self.header.clone(),
            value,
            inner,
            _req_or_res: PhantomData,
        }));
        */
        } else {
            trace!("{:?} not enabled for {:?}", self.header, t);
            None
        };
        let inner = self.inner.call(t);
        MakeFuture {
            inner,
            header,
            _req_or_res: PhantomData,
        }
    }
}

impl<H, T, M, R> fmt::Debug for Stack<H, T, M, R>
where
    H: fmt::Debug,
    T: fmt::Debug,
    M: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stack")
            .field("header", &self.header)
            .field("get_header", &format_args!("{}", "..."))
            .field("inner", &self.inner)
            .finish()
    }
}

// === impl MakeFuture ===

impl<F, H, R> Future for MakeFuture<F, H, R>
where
    F: Future,
{
    type Item = svc::Either<Service<H, F::Item, R>, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let svc = if let Some((header, value)) = self.header.take() {
            svc::Either::A(Service {
                header,
                value,
                inner,
                _req_or_res: PhantomData,
            })
        } else {
            svc::Either::B(inner)
        };
        Ok(svc.into())
    }
}

pub mod request {
    use crate::svc;
    use futures::Poll;
    use http;
    use http::header::{AsHeaderName, IntoHeaderName};

    pub fn layer<H, T>(header: H, get_header: super::GetHeader<T>) -> super::Layer<H, T, ReqHeader>
    where
        H: AsHeaderName + Clone,
    {
        super::layer(header, get_header)
    }

    /// Marker type used to specify that the `Request` headers should be added.
    #[derive(Clone, Debug)]
    pub enum ReqHeader {}

    impl<H, S, B> svc::Service<http::Request<B>> for super::Service<H, S, ReqHeader>
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
            req.headers_mut()
                .insert(self.header.clone(), self.value.clone());
            self.inner.call(req)
        }
    }
}

pub mod response {
    use crate::svc;
    use futures::{try_ready, Future, Poll};
    use http;
    use http::header::{AsHeaderName, HeaderValue, IntoHeaderName};

    pub fn layer<H, T>(header: H, get_header: super::GetHeader<T>) -> super::Layer<H, T, ResHeader>
    where
        H: AsHeaderName + Clone,
    {
        super::layer(header, get_header)
    }

    /// Marker type used to specify that the `Response` headers should be added.
    #[derive(Clone, Debug)]
    pub enum ResHeader {}

    pub struct ResponseFuture<F, H> {
        inner: F,
        header: H,
        value: HeaderValue,
    }

    impl<H, S, B, Req> svc::Service<Req> for super::Service<H, S, ResHeader>
    where
        H: IntoHeaderName + Clone,
        S: svc::Service<Req, Response = http::Response<B>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = ResponseFuture<S::Future, H>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, req: Req) -> Self::Future {
            let fut = self.inner.call(req);

            ResponseFuture {
                inner: fut,
                header: self.header.clone(),
                value: self.value.clone(),
            }
        }
    }

    impl<F, H, B> Future for ResponseFuture<F, H>
    where
        H: IntoHeaderName + Clone,
        F: Future<Item = http::Response<B>>,
    {
        type Item = F::Item;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let mut res = try_ready!(self.inner.poll());
            res.headers_mut()
                .insert(self.header.clone(), self.value.clone());
            Ok(res.into())
        }
    }
}
