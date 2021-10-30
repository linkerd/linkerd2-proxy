//! Provides a middleware that holds an inner service as long as responses are
//! being processed.

use linkerd_stack::layer;
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// Retains an inner service as long as its HTTP responses are being processsed
/// so that RAII-guarded resources are held. This is mostly intended to support
/// cache eviction.
#[derive(Debug)]
pub struct Retain<S, B> {
    inner: S,
    _marker: std::marker::PhantomData<fn() -> B>,
}

/// Wraps a `B` typed HTTP body to ensure that a `T` typed instance is held until
/// the body is fully processed.
#[pin_project]
#[derive(Debug)]
pub struct RetainBody<T, B> {
    #[pin]
    inner: B,

    _retain: Option<T>,
}

// === impl Retain ===

impl<S, B> Retain<S, B> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn layer() -> impl layer::Layer<S, Service = Self> + Copy + Clone {
        layer::mk(Self::new)
    }
}

impl<S: Clone, B> Clone for Retain<S, B> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: std::marker::PhantomData,
        }
    }
}

type RetainFuture<S, B, E> =
    Pin<Box<dyn Future<Output = Result<http::Response<RetainBody<S, B>>, E>> + Send + 'static>>;

impl<S, Req, RspB> tower::Service<Req> for Retain<S, RspB>
where
    S: tower::Service<Req, Response = http::Response<RspB>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = http::Response<RetainBody<S, RspB>>;
    type Error = S::Error;
    type Future = RetainFuture<S, RspB, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let fut = self.inner.call(req);

        // Retain a handle to the inner service until the response is completely
        // processed.
        let service = self.inner.clone();
        Box::pin(async move {
            let rsp = fut.await?;
            Ok(rsp.map(move |inner| RetainBody {
                inner,
                _retain: Some(service),
            }))
        })
    }
}

// === impl RetainBody ===

impl<T, B: Default> Default for RetainBody<T, B> {
    fn default() -> Self {
        Self {
            inner: B::default(),
            _retain: None,
        }
    }
}

impl<T, B: http_body::Body> http_body::Body for RetainBody<T, B> {
    type Data = B::Data;
    type Error = B::Error;

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    #[inline]
    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<B::Data, B::Error>>> {
        self.project().inner.poll_data(cx)
    }

    #[inline]
    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap<http::HeaderValue>>, B::Error>> {
        self.project().inner.poll_trailers(cx)
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}
