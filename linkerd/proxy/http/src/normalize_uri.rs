use super::h1;
use linkerd2_stack::{layer, NewService};
use std::{
    net::SocketAddr,
    str::FromStr,
    task::{Context, Poll},
};
use tracing::trace;

pub fn layer<M>() -> impl layer::Layer<M, Service = MakeNormalizeUri<M>> + Copy {
    layer::mk(|inner| MakeNormalizeUri { inner })
}

#[derive(Clone, Debug)]
pub struct MakeNormalizeUri<S> {
    inner: S,
}

#[derive(Clone, Debug)]
pub struct NormalizeUri<S> {
    inner: S,
    orig_dst: SocketAddr,
}

// === impl MakeNormalizeUri ===

impl<T, M> NewService<T> for MakeNormalizeUri<M>
where
    for<'t> &'t T: Into<SocketAddr>,
    M: NewService<T>,
{
    type Service = NormalizeUri<M::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let orig_dst = (&target).into();
        let inner = self.inner.new_service(target);
        NormalizeUri { inner, orig_dst }
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
                    let host = h1::authority_from_host(&req).unwrap_or_else(|| {
                        http::uri::Authority::from_str(&self.orig_dst.to_string()).unwrap()
                    });
                    trace!(%host, "Normalizing URI");
                    h1::set_authority(req.uri_mut(), host);
                }
            }
        }

        self.inner.call(req)
    }
}
