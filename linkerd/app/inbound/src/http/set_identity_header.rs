use linkerd_app_core::{identity, proxy::http, svc};
use std::task::{Context, Poll};
use tracing::debug;

const HEADER_NAME: &str = "l5d-client-id";

#[derive(Clone, Debug)]
pub struct NewSetIdentityHeader<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct SetIdentityHeader<M> {
    inner: M,
    value: Option<http::HeaderValue>,
}

// === impl NewSetIdentityHeader ===

impl<N> NewSetIdentityHeader<N> {
    pub fn layer() -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self { inner })
    }
}

impl<T, N> svc::NewService<T> for NewSetIdentityHeader<N>
where
    T: svc::Param<Option<identity::Name>>,
    N: svc::NewService<T>,
{
    type Service = SetIdentityHeader<N::Service>;

    #[inline]
    fn new_service(&mut self, t: T) -> Self::Service {
        let value = t.param().map(|name| {
            http::HeaderValue::from_str(name.as_ref())
                .expect("identity must be a valid header value")
        });
        SetIdentityHeader {
            value,
            inner: self.inner.new_service(t),
        }
    }
}

// === impl Service ===

impl<S, B> tower::Service<http::Request<B>> for SetIdentityHeader<S>
where
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let prior = if let Some(id) = self.value.clone() {
            debug!(header = %HEADER_NAME, ?id, "Setting identity header");
            req.headers_mut().insert(HEADER_NAME, id)
        } else {
            req.headers_mut().remove(HEADER_NAME)
        };
        if let Some(value) = prior {
            debug!(header = %HEADER_NAME, ?value, "Stripped identity header");
        }

        self.inner.call(req)
    }
}
