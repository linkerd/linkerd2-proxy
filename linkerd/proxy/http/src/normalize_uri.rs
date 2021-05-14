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
use futures::{future, TryFutureExt};
use http::uri::Authority;
use linkerd_error::Error;
use linkerd_stack::{layer, NewService, Param};
use std::task::{Context, Poll};
use thiserror::Error;
use tracing::trace;

#[derive(Clone, Debug)]
pub struct NewNormalizeUri<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct NormalizeUri<S> {
    inner: S,
    default: Option<http::uri::Authority>,
}

/// Parameterizes a stack target to produce an optional default authority.
#[derive(Clone, Debug)]
pub struct DefaultAuthority(pub Option<Authority>);

#[derive(Debug, Error)]
#[error("failed to normalize URI because no authority could be determined")]
pub struct NoAuthority(());

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
    T: Param<DefaultAuthority>,
    N: NewService<T>,
{
    type Service = NormalizeUri<N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let DefaultAuthority(default) = target.param();
        let inner = self.inner.new_service(target);
        NormalizeUri::new(inner, default)
    }
}

// === impl NormalizeUri ===

impl<S> NormalizeUri<S> {
    fn new(inner: S, default: Option<Authority>) -> Self {
        Self { inner, default }
    }
}

impl<S, B> tower::Service<http::Request<B>> for NormalizeUri<S>
where
    S: tower::Service<http::Request<B>>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::ErrInto<S::Future, Error>,
        future::Ready<Result<S::Response, Error>>,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        if let http::Version::HTTP_10 | http::Version::HTTP_11 = req.version() {
            if req.extensions().get::<h1::WasAbsoluteForm>().is_none()
                && req.uri().authority().is_none()
            {
                let authority = match h1::authority_from_host(&req).or_else(|| self.default.clone())
                {
                    Some(a) => a,
                    None => return future::Either::Right(future::err(NoAuthority(()).into())),
                };

                trace!(%authority, "Normalizing URI");
                h1::set_authority(req.uri_mut(), authority);
            }
        }

        future::Either::Left(self.inner.call(req).err_into())
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

    #[inline]
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
