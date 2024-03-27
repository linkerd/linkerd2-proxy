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
//! * If the request was originally in absolute-form, the `UriWasOriginallyAbsoluteForm`
//!   extension is added so that the `h1::Client` can differentiate the request
//!   from modified requests;
//! * Otherwise, if the request has a `Host` header, it is used as the authority;
//! * Otherwise, the target's address is used (as provided by the target).

use super::UriWasOriginallyAbsoluteForm;
use http::uri::{Authority, Uri};
use linkerd_error::Error;
use std::task::{Context, Poll};
use tracing::trace;

#[derive(Clone, Debug)]
pub(super) struct NormalizeUri<S> {
    inner: S,
    default: http::uri::Authority,
}

/// Detects the original form of a request URI and inserts a `UriWasOriginallyAbsoluteForm`
/// extension.
#[derive(Clone, Debug)]
pub(super) struct MarkAbsoluteForm<S> {
    inner: S,
}

// === impl NormalizeUri ===

impl<S> NormalizeUri<S> {
    pub(super) fn new(default: Authority, inner: S) -> Self {
        Self { inner, default }
    }
}

impl<S, B> tower::Service<http::Request<B>> for NormalizeUri<S>
where
    S: tower::Service<http::Request<B>>,
    S::Error: Into<Error>,
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
            if req
                .extensions()
                .get::<UriWasOriginallyAbsoluteForm>()
                .is_none()
                && req.uri().authority().is_none()
            {
                let authority = crate::authority_from_header(&req, http::header::HOST)
                    .unwrap_or_else(|| self.default.clone());

                trace!(%authority, "Normalizing URI");
                crate::set_authority(req.uri_mut(), authority);
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
            if is_absolute_form(req.uri()) {
                trace!(uri = ?req.uri(), "Absolute form");
                req.extensions_mut()
                    .insert(UriWasOriginallyAbsoluteForm(()));
            } else {
                trace!(uri = ?req.uri(), "Origin form");
            };
        }

        self.inner.call(req)
    }
}

/// Returns if the request target is in `absolute-form`.
///
/// This is `absolute-form`: `https://example.com/docs`
///
/// This is not:
///
/// - `/docs`
/// - `example.com`
fn is_absolute_form(uri: &Uri) -> bool {
    // It's sufficient just to check for a scheme, since in HTTP1,
    // it's required in absolute-form, and `http::Uri` doesn't
    // allow URIs with the other parts missing when the scheme is set.
    debug_assert!(
        uri.scheme().is_none() || (uri.authority().is_some() && uri.path_and_query().is_some()),
        "is_absolute_form http::Uri invariants: {:?}",
        uri
    );

    uri.scheme().is_some()
}
