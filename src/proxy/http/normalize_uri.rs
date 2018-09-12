use http;
use futures::Poll;

use super::h1::normalize_our_view_of_uri;
use svc;

/// Rewrites HTTP/1.x requests so that their URIs are in a canonical form.
///
/// The following transformations are applied:
/// - If an absolute-form URI is received, it must replace
///   the host header (in accordance with RFC7230#section-5.4)
/// - If the request URI is not in absolute form, it is rewritten to contain
///   the authority given in the `Host:` header, or, failing that, from the
///   request's original destination according to `SO_ORIGINAL_DST`.
#[derive(Copy, Clone, Debug)]
pub struct NormalizeUri<S> {
    inner: S,
    was_absolute_form: bool,
}

// ===== impl NormalizeUri =====

impl<S> NormalizeUri<S> {
    pub fn new(inner: S, was_absolute_form: bool) -> Self {
        Self { inner, was_absolute_form }
    }
}

impl<S, B> svc::Service for NormalizeUri<S>
where
    S: svc::Service<Request = http::Request<B>>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut request: S::Request) -> Self::Future {
        if request.version() != http::Version::HTTP_2 &&
            // Skip normalizing the URI if it was received in
            // absolute form.
            !self.was_absolute_form
        {
            normalize_our_view_of_uri(&mut request);
        }
        self.inner.call(request)
    }
}
