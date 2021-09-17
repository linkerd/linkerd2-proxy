use std::marker::PhantomData;

pub struct StripHeader<H, S, R> {
    header: H,
    inner: S,
    _marker: PhantomData<fn(R)>,
}

impl<H: Clone, S: Clone, R> Clone for StripHeader<H, S, R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            header: self.header.clone(),
            _marker: self._marker,
        }
    }
}

pub mod request {
    use http::header::AsHeaderName;
    use linkerd_stack::{layer, Proxy};
    use std::{
        marker::PhantomData,
        task::{Context, Poll},
    };

    /// Marker type used to specify that the `Request` headers should be
    /// stripped.
    pub enum ReqHeader {}

    type StripHeader<H, S> = super::StripHeader<H, S, ReqHeader>;

    pub fn layer<H, S>(header: H) -> impl layer::Layer<S, Service = StripHeader<H, S>> + Clone
    where
        H: AsHeaderName + Clone,
    {
        layer::mk(move |inner| StripHeader {
            inner,
            header: header.clone(),
            _marker: PhantomData,
        })
    }

    impl<H, P, S, B> Proxy<http::Request<B>, S> for StripHeader<H, P>
    where
        P: Proxy<http::Request<B>, S>,
        H: AsHeaderName + Clone,
        S: tower::Service<P::Request>,
    {
        type Request = P::Request;
        type Response = P::Response;
        type Error = P::Error;
        type Future = P::Future;

        #[inline]
        fn proxy(&self, svc: &mut S, mut req: http::Request<B>) -> Self::Future {
            req.headers_mut().remove(self.header.clone());
            self.inner.proxy(svc, req)
        }
    }

    impl<H, S, B> tower::Service<http::Request<B>> for StripHeader<H, S>
    where
        H: AsHeaderName + Clone,
        S: tower::Service<http::Request<B>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        #[inline]
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        #[inline]
        fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
            req.headers_mut().remove(self.header.clone());
            self.inner.call(req)
        }
    }
}

pub mod response {
    use futures::{ready, Future, TryFuture};
    use http::header::AsHeaderName;
    use linkerd_error::Error;
    use linkerd_stack::layer;
    use pin_project::pin_project;
    use std::{
        marker::PhantomData,
        pin::Pin,
        task::{Context, Poll},
    };

    /// Marker type used to specify that the `Response` headers should be
    /// stripped.
    pub enum RspHeader {}

    type StripHeader<H, S> = super::StripHeader<H, S, RspHeader>;

    pub fn layer<H, S>(header: H) -> impl layer::Layer<S, Service = StripHeader<H, S>> + Clone
    where
        H: AsHeaderName + Clone,
    {
        layer::mk(move |inner| StripHeader {
            inner,
            header: header.clone(),
            _marker: PhantomData,
        })
    }

    #[pin_project]
    pub struct ResponseFuture<F, H> {
        #[pin]
        inner: F,
        header: H,
    }

    impl<H, S, B, Req> tower::Service<Req> for StripHeader<H, S>
    where
        H: AsHeaderName + Clone,
        S: tower::Service<Req, Response = http::Response<B>>,
        S::Error: Into<Error> + Send + Sync,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = ResponseFuture<S::Future, H>;

        #[inline]
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        #[inline]
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

        #[inline]
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.project();
            let mut res = ready!(this.inner.try_poll(cx))?;
            res.headers_mut().remove(this.header.clone());
            Poll::Ready(Ok(res))
        }
    }
}
