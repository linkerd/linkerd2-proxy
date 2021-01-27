//! Ensures that all requests have valid a URI, including an Authority.
//!
//! When the Hyper HTTP/1 server receives a request, it only sets the uri's
//! Authority if the request is in the absolute form (i.e. it's an HTTP proxy
//! request). However, when the Hyper HTTP/1 client receives a request, it
//! _requires_ that the the authority is set, even when it's not in the absolute
//! form.
//!
//! This middleware prepares server-provided requests to be suitable for clients:
//!
//! * If the request was originally in absolute-form, the `h1::WasAbsoluteForm`
//!   extension is added so that the `h1::Client` can differentiate the request
//!   from modified requests;
//! * Otherwise, if the request has a `Host` header, it is used as the authority;
//! * Otherwise, the target's address is used (as provided by the target).

use super::h1;
use linkerd_stack::{layer, NewService, Param};
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use tracing::trace;

#[derive(Clone, Debug)]
pub struct NewNormalizeUri<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct NormalizeUri<S> {
    inner: S,
    default: http::uri::Authority,
}

/// Detects the original form of a request URI and inserts a `WasAbsoluteForm`
/// extension.
#[derive(Clone, Debug)]
pub struct MarkAbsoluteForm<S> {
    inner: S,
}

// === impl NewNormalizeUri ===

impl<N> NewNormalizeUri<N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Copy + Clone {
        layer::mk(Self::new)
    }

    fn new(inner: N) -> Self {
        Self { inner }
    }
}

impl<T, N> NewService<T> for NewNormalizeUri<N>
where
    T: Param<SocketAddr>,
    N: NewService<T>,
{
    type Service = NormalizeUri<N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let target_addr = target.param();
        let inner = self.inner.new_service(target);
        NormalizeUri::new(inner, target_addr)
    }
}

type MakeFuture<T, E> = Pin<Box<dyn Future<Output = Result<NormalizeUri<T>, E>> + Send + 'static>>;

impl<M, T> tower::Service<T> for NewNormalizeUri<M>
where
    T: Param<SocketAddr>,
    M: tower::Service<T>,
    M::Future: Send + 'static,
{
    type Response = NormalizeUri<M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Response, M::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), M::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let target_addr = target.param();
        let fut = self.inner.call(target);
        Box::pin(async move {
            let inner = fut.await?;
            Ok(NormalizeUri::new(inner, target_addr))
        })
    }
}

// === impl NormalizeUri ===

impl<S> NormalizeUri<S> {
    fn new(inner: S, target_addr: SocketAddr) -> Self {
        let default = http::uri::Authority::from_str(&target_addr.to_string())
            .expect("SocketAddr must be a valid Authority");
        Self { inner, default }
    }
}

impl<S, B> tower::Service<http::Request<B>> for NormalizeUri<S>
where
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        if let http::Version::HTTP_10 | http::Version::HTTP_11 = req.version() {
            if !h1::is_absolute_form(req.uri()) && req.uri().authority().is_none() {
                let authority =
                    h1::authority_from_host(&req).unwrap_or_else(|| self.default.clone());
                trace!(%authority, "Normalizing URI");
                h1::set_authority(req.uri_mut(), authority);
            }
        }

        self.inner.call(req)
    }
}

// === impl MarkAbsoluteForm ===

impl<S> MarkAbsoluteForm<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }

    pub fn layer() -> impl layer::Layer<S, Service = Self> + Copy + Clone {
        layer::mk(Self::new)
    }
}

impl<S, B> tower::Service<http::Request<B>> for MarkAbsoluteForm<S>
where
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        if let http::Version::HTTP_10 | http::Version::HTTP_11 = req.version() {
            if h1::is_absolute_form(req.uri()) {
                trace!(uri = ?req.uri(), "Absolute form");
                req.extensions_mut().insert(h1::WasAbsoluteForm(()));
            } else {
                trace!(uri = ?req.uri(), "Origin form");
            };
        }

        self.inner.call(req)
    }
}
