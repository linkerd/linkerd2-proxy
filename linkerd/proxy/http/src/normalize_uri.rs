use super::h1;
use std::task::{Context, Poll};
use tracing::trace;

#[derive(Clone, Debug)]
pub struct NormalizeUri<S> {
    inner: S,
}

// === impl NormalizeUri ===

impl<S> NormalizeUri<S> {
    pub fn new(inner: S) -> Self {
        NormalizeUri { inner }
    }
}

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

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        // HTTP requests from a hyper server may not have an Authority set on
        // the URI. In such cases, we set this value from the request's HOST
        // header.
        match req.version() {
            http::Version::HTTP_2 | http::Version::HTTP_3 => {}
            _ => {
                // Http/1
                if h1::is_absolute_form(req.uri()) {
                    trace!(uri = ?req.uri(), "Absolute");
                    req.extensions_mut().insert(h1::WasAbsoluteForm(()));
                } else if req.uri().authority().is_none() {
                    if let Some(host) = h1::authority_from_host(&req) {
                        trace!(%host, "Normalizing URI");
                        h1::set_authority(req.uri_mut(), host);
                    } else {
                        trace!("Missing Host");
                    }
                }
            }
        }

        self.inner.call(req)
    }
}
