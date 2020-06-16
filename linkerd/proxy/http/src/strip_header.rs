use http::header::AsHeaderName;
use std::marker::PhantomData;

#[derive(Clone, Debug)]
pub struct Layer<H, R> {
    header: H,
    _marker: PhantomData<fn(R)>,
}

#[derive(Clone, Debug)]
pub struct Service<H, S, R> {
    header: H,
    inner: S,
    _marker: PhantomData<fn(R)>,
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
        _marker: PhantomData,
    }
}

impl<H, S, R> tower::layer::Layer<S> for Layer<H, R>
where
    H: AsHeaderName + Clone,
{
    type Service = Service<H, S, R>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            header: self.header.clone(),
            inner,
            _marker: PhantomData,
        }
    }
}

pub mod request {
    use http;
    use http::header::AsHeaderName;
    use linkerd2_stack::Proxy;
    use std::task::{Context, Poll};

    pub fn layer<H>(header: H) -> super::Layer<H, ReqHeader>
    where
        H: AsHeaderName + Clone,
    {
        super::layer(header)
    }

    /// Marker type used to specify that the `Request` headers should be stripped.
    #[derive(Clone, Debug)]
    pub enum ReqHeader {}

    impl<H, P, S, B> Proxy<http::Request<B>, S> for super::Service<H, P, ReqHeader>
    where
        P: Proxy<http::Request<B>, S>,
        H: AsHeaderName + Clone,
        S: tower::Service<P::Request>,
    {
        type Request = P::Request;
        type Response = P::Response;
        type Error = P::Error;
        type Future = P::Future;

        fn proxy(&self, svc: &mut S, mut req: http::Request<B>) -> Self::Future {
            req.headers_mut().remove(self.header.clone());
            self.inner.proxy(svc, req)
        }
    }

    impl<H, S, B> tower::Service<http::Request<B>> for super::Service<H, S, ReqHeader>
    where
        H: AsHeaderName + Clone,
        S: tower::Service<http::Request<B>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
            req.headers_mut().remove(self.header.clone());
            self.inner.call(req)
        }
    }
}

pub mod response {
    use futures::{ready, Future, TryFuture};
    use http;
    use http::header::AsHeaderName;
    use linkerd2_error::Error;
    use pin_project::pin_project;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub fn layer<H>(header: H) -> super::Layer<H, ResHeader>
    where
        H: AsHeaderName + Clone,
    {
        super::layer(header)
    }

    /// Marker type used to specify that the `Response` headers should be stripped.
    #[derive(Clone, Debug)]
    pub enum ResHeader {}

    #[pin_project]
    pub struct ResponseFuture<F, H> {
        #[pin]
        inner: F,
        header: H,
    }

    impl<H, S, B, Req> tower::Service<Req> for super::Service<H, S, ResHeader>
    where
        H: AsHeaderName + Clone,
        S: tower::Service<Req, Response = http::Response<B>>,
        S::Error: Into<Error> + Send + Sync,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = ResponseFuture<S::Future, H>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
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
        F: TryFuture<Ok = http::Response<B>>,
        H: AsHeaderName + Clone,
    {
        type Output = Result<F::Ok, F::Error>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.project();
            let mut res = ready!(this.inner.try_poll(cx))?;
            res.headers_mut().remove(this.header.clone());
            Poll::Ready(Ok(res))
        }
    }
}
