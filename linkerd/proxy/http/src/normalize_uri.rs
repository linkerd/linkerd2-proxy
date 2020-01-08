use super::h1;
use futures::{try_ready, Future, Poll};
use http::uri::Authority;
use linkerd2_stack::layer;
use tracing::trace;

pub trait ShouldNormalizeUri {
    fn should_normalize_uri(&self) -> Option<Authority>;
}

#[derive(Clone, Debug)]
pub struct MakeNormalizeUri<N> {
    inner: N,
}

pub struct MakeFuture<F> {
    inner: F,
    authority: Option<Authority>,
}

#[derive(Clone, Debug)]
pub struct NormalizeUri<S> {
    inner: S,
    authority: Option<Authority>,
}

// === impl Layer ===

pub fn layer<M>() -> impl tower::layer::Layer<M, Service = MakeNormalizeUri<M>> + Copy {
    layer::mk(|inner| MakeNormalizeUri { inner })
}

// === impl MakeNormalizeUri ===

impl<T, M> tower::Service<T> for MakeNormalizeUri<M>
where
    T: ShouldNormalizeUri,
    M: tower::Service<T>,
{
    type Response = NormalizeUri<M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), M::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let authority = target.should_normalize_uri();
        tracing::trace!(?authority, "make");

        MakeFuture {
            authority,
            inner: self.inner.call(target),
        }
    }
}

// === impl MakeFuture ===

impl<F: Future> Future for MakeFuture<F> {
    type Item = NormalizeUri<F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let svc = NormalizeUri {
            inner,
            authority: self.authority.take(),
        };
        Ok(svc.into())
    }
}

// === impl NormalizeUri ===

impl<S, B> tower::Service<http::Request<B>> for NormalizeUri<S>
where
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        if let Some(ref authority) = self.authority {
            trace!(%authority, "Normalizing URI");
            debug_assert!(
                request.version() != http::Version::HTTP_2,
                "normalize_uri must only be applied to HTTP/1"
            );
            h1::set_authority(request.uri_mut(), authority.clone());
        } else {
            trace!("Not normalizing URI");
        }

        self.inner.call(request)
    }
}
