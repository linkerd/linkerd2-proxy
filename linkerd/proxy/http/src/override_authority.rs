use super::h1;
use http::{self, header::AsHeaderName, uri::Authority};
use linkerd_stack::{layer, NewService, Param};
use std::{
    fmt,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::debug;

#[derive(Clone, Debug)]
pub struct AuthorityOverride(pub Authority);

#[derive(Clone, Debug)]
pub struct NewOverrideAuthority<H, M> {
    headers_to_strip: Arc<Vec<H>>,
    inner: M,
}

#[derive(Clone, Debug)]
pub struct OverrideAuthority<S, H> {
    authority: Option<Authority>,
    headers_to_strip: Arc<Vec<H>>,
    inner: S,
}

// === impl NewOverrideAuthority ===

impl<H: Clone, N> NewOverrideAuthority<H, N> {
    pub fn layer(
        headers_to_strip: impl IntoIterator<Item = H>,
    ) -> impl layer::Layer<N, Service = Self> + Clone {
        let headers_to_strip = Arc::new(headers_to_strip.into_iter().collect::<Vec<H>>());
        layer::mk(move |inner| Self {
            inner,
            headers_to_strip: headers_to_strip.clone(),
        })
    }
}

impl<H, T, M> NewService<T> for NewOverrideAuthority<H, M>
where
    T: Param<Option<AuthorityOverride>>,
    M: NewService<T>,
    H: AsHeaderName + Clone,
{
    type Service = OverrideAuthority<M::Service, H>;

    #[inline]
    fn new_service(&mut self, t: T) -> Self::Service {
        OverrideAuthority {
            authority: t.param().map(|AuthorityOverride(a)| a),
            headers_to_strip: self.headers_to_strip.clone(),
            inner: self.inner.new_service(t),
        }
    }
}

// === impl Service ===

impl<S, H, B> tower::Service<http::Request<B>> for OverrideAuthority<S, H>
where
    S: tower::Service<http::Request<B>>,
    H: AsHeaderName + fmt::Display + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        if let Some(authority) = self.authority.clone() {
            for header in self.headers_to_strip.iter() {
                if let Some(value) = req.headers_mut().remove(header.clone()) {
                    debug!(
                        %header,
                        ?value,
                        "Stripped header",
                    );
                };
            }

            debug!(%authority, "Overriding");
            h1::set_authority(req.uri_mut(), authority);
        }

        self.inner.call(req)
    }
}
