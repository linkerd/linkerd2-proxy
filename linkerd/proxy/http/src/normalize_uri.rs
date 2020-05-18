use super::h1;
use futures_03::{ready, TryFuture};
use http::uri::Authority;
use linkerd2_stack::{layer, NewService};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::trace;

pub trait ShouldNormalizeUri {
    fn should_normalize_uri(&self) -> Option<Authority>;
}

#[derive(Clone, Debug)]
pub struct MakeNormalizeUri<N> {
    inner: N,
}

#[pin_project]
pub struct MakeFuture<F> {
    #[pin]
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

impl<T, M> NewService<T> for MakeNormalizeUri<M>
where
    T: ShouldNormalizeUri,
    M: NewService<T>,
{
    type Service = NormalizeUri<M::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let authority = target.should_normalize_uri();
        let inner = self.inner.new_service(target);
        NormalizeUri { inner, authority }
    }
}

impl<T, M> tower::Service<T> for MakeNormalizeUri<M>
where
    T: ShouldNormalizeUri,
    M: tower::Service<T>,
{
    type Response = NormalizeUri<M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), M::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let authority = target.should_normalize_uri();
        MakeFuture {
            authority,
            inner: self.inner.call(target),
        }
    }
}

// === impl MakeFuture ===

impl<F: TryFuture> Future for MakeFuture<F> {
    type Output = Result<NormalizeUri<F::Ok>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let inner = ready!(this.inner.try_poll(cx))?;
        let svc = NormalizeUri {
            inner,
            authority: this.authority.take(),
        };
        Poll::Ready(Ok(svc))
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

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        if let Some(ref default_authority) = self.authority {
            // If an authority was set, we know that normalization is needed.
            // Use the authority from the stack as a fallback, preferrring the
            // value from each request. This ensures that we don't modify
            // request semantics if, for instance, the stack's authority is
            // canonical but the request's authority is relative.
            let authority = request
                .uri()
                .authority()
                .cloned()
                .or_else(|| h1::authority_from_host(&request))
                .unwrap_or_else(|| default_authority.clone());
            trace!(%authority, "Normalizing URI");
            debug_assert!(
                request.version() != http::Version::HTTP_2,
                "normalize_uri must only be applied to HTTP/1"
            );
            h1::set_authority(request.uri_mut(), authority.clone());
        }

        self.inner.call(request)
    }
}
