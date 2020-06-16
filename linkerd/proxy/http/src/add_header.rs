use futures::{ready, TryFuture};
use http::header::{AsHeaderName, HeaderValue};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fmt, marker::PhantomData};
use tracing::trace;

/// A function used to get the header value for a given Stack target.
type GetHeader<T> = fn(&T) -> Option<HeaderValue>;

/// Wraps HTTP `Service` `MakeAddHeader<T>`s so that a given header is removed from a
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
pub struct MakeAddHeader<H, T, M, R> {
    header: H,
    get_header: GetHeader<T>,
    inner: M,
    _req_or_res: PhantomData<fn(R)>,
}

#[pin_project]
pub struct MakeFuture<F, H, R> {
    header: Option<(H, HeaderValue)>,
    #[pin]
    inner: F,
    _req_or_res: PhantomData<fn(R)>,
}

#[derive(Clone, Debug)]
pub struct AddHeader<H, S, R> {
    header: H,
    value: HeaderValue,
    inner: S,
    _req_or_res: PhantomData<fn(R)>,
}

// === impl Layer ===

impl<H, T, R> Layer<H, T, R> {
    /// Used by the `request` and `response` modules.
    fn new(header: H, get_header: GetHeader<T>) -> Self {
        Self {
            header,
            get_header,
            _req_or_res: PhantomData,
        }
    }
}

impl<H, T, M, R> tower::layer::Layer<M> for Layer<H, T, R>
where
    H: AsHeaderName + Clone + fmt::Debug,
    T: fmt::Debug,
    M: tower::Service<T>,
{
    type Service = MakeAddHeader<H, T, M, R>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            header: self.header.clone(),
            get_header: self.get_header,
            inner,
            _req_or_res: PhantomData,
        }
    }
}

// === impl Stack ===

/// impl MakeService
impl<H, T, M, R> tower::Service<T> for MakeAddHeader<H, T, M, R>
where
    H: AsHeaderName + Clone + fmt::Debug,
    T: fmt::Debug,
    M: tower::Service<T>,
{
    type Response = tower::util::Either<AddHeader<H, M::Response, R>, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future, H, R>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, t: T) -> Self::Future {
        let header = if let Some(value) = (self.get_header)(&t) {
            Some((self.header.clone(), value))
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

// === impl MakeFuture ===

impl<F, H, R> Future for MakeFuture<F, H, R>
where
    F: TryFuture,
{
    type Output = Result<tower::util::Either<AddHeader<H, F::Ok, R>, F::Ok>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let inner = ready!(this.inner.try_poll(cx))?;
        let svc = if let Some((header, value)) = this.header.take() {
            tower::util::Either::A(AddHeader {
                header,
                value,
                inner,
                _req_or_res: PhantomData,
            })
        } else {
            tower::util::Either::B(inner)
        };
        Poll::Ready(Ok(svc))
    }
}

pub mod request {
    use http;
    use http::header::{AsHeaderName, IntoHeaderName};
    use std::task::{Context, Poll};

    pub fn layer<H, T>(header: H, get_header: super::GetHeader<T>) -> super::Layer<H, T, ReqHeader>
    where
        H: AsHeaderName + Clone,
    {
        super::Layer::new(header, get_header)
    }

    /// Marker type used to specify that the `Request` headers should be added.
    #[derive(Clone, Debug)]
    pub enum ReqHeader {}

    impl<H, S, B> tower::Service<http::Request<B>> for super::AddHeader<H, S, ReqHeader>
    where
        H: IntoHeaderName + Clone,
        S: tower::Service<http::Request<B>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
            req.headers_mut()
                .insert(self.header.clone(), self.value.clone());
            self.inner.call(req)
        }
    }
}

pub mod response {
    use futures::{ready, TryFuture};
    use http;
    use http::header::{AsHeaderName, HeaderValue, IntoHeaderName};
    use pin_project::pin_project;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub fn layer<H, T>(header: H, get_header: super::GetHeader<T>) -> super::Layer<H, T, ResHeader>
    where
        H: AsHeaderName + Clone,
    {
        super::Layer::new(header, get_header)
    }

    /// Marker type used to specify that the `Response` headers should be added.
    #[derive(Clone, Debug)]
    pub enum ResHeader {}

    #[pin_project]
    pub struct ResponseFuture<F, H> {
        #[pin]
        inner: F,
        header: H,
        value: HeaderValue,
    }

    impl<H, S, B, Req> tower::Service<Req> for super::AddHeader<H, S, ResHeader>
    where
        H: IntoHeaderName + Clone,
        S: tower::Service<Req, Response = http::Response<B>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = ResponseFuture<S::Future, H>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
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
        F: TryFuture<Ok = http::Response<B>>,
    {
        type Output = Result<F::Ok, F::Error>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.project();
            let mut res = ready!(this.inner.try_poll(cx))?;
            res.headers_mut()
                .insert(this.header.clone(), this.value.clone());
            Poll::Ready(Ok(res))
        }
    }
}
