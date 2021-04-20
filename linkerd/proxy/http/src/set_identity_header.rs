use http::{self, header::AsHeaderName, header::IntoHeaderName, HeaderValue};
use linkerd_identity::Name;
use linkerd_stack::{layer, NewService, Param};
use std::{
    fmt,
    task::{Context, Poll},
};
use tracing::debug;

#[derive(Clone, Debug)]
pub struct SetIdentityHeader<M, H> {
    header: H,
    identity: Option<Name>,
    inner: M,
}

#[derive(Clone, Debug)]
pub struct NewSetIdentityHeader<H, M> {
    header: H,
    inner: M,
}

// === impl NewSetIdentityHeader ===

impl<H: Clone, N> NewSetIdentityHeader<H, N> {
    pub fn layer(header: H) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            header: header.clone(),
            inner,
        })
    }
}

impl<H, T, M> NewService<T> for NewSetIdentityHeader<H, M>
where
    T: Param<Option<Name>>,
    M: NewService<T>,
    H: AsHeaderName + Clone,
{
    type Service = SetIdentityHeader<M::Service, H>;

    #[inline]
    fn new_service(&mut self, t: T) -> Self::Service {
        SetIdentityHeader {
            header: self.header.clone(),
            identity: t.param(),
            inner: self.inner.new_service(t),
        }
    }
}

// === impl Service ===

impl<S, H, B> tower::Service<http::Request<B>> for SetIdentityHeader<S, H>
where
    S: tower::Service<http::Request<B>>,
    H: AsHeaderName + IntoHeaderName + fmt::Display + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        if let Some(value) = req.headers_mut().remove(self.header.clone()) {
            debug!(
                %self.header,
                ?value,
                "Stripped header",
            );
        }

        if let Some(name) = self.identity.clone() {
            debug!(%name, "Setting identity");
            req.headers_mut().insert(
                self.header.clone(),
                HeaderValue::from_str(&name.to_string())
                    .expect("identity must be a valid header value"),
            );
        }

        self.inner.call(req)
    }
}
