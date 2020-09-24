use super::h1;
use http::{self, header::AsHeaderName, uri::Authority};
use linkerd2_stack::NewService;
use std::fmt;
use std::task::{Context, Poll};
use tracing::debug;

pub trait CanOverrideAuthority {
    fn override_authority(&self) -> Option<Authority>;
}

#[derive(Debug, Clone)]
pub struct Layer<H> {
    headers_to_strip: Vec<H>,
}

#[derive(Clone, Debug)]
pub struct MakeSvc<H, M> {
    headers_to_strip: Vec<H>,
    inner: M,
}

#[derive(Clone, Debug)]
pub struct Service<S, H> {
    authority: Option<Authority>,
    headers_to_strip: Vec<H>,
    inner: S,
}

// === impl Layer ===

impl<H> Layer<H>
where
    H: AsHeaderName + Clone,
{
    pub fn new(headers_to_strip: Vec<H>) -> Self {
        Self { headers_to_strip }
    }
}

impl<H> Default for Layer<H> {
    fn default() -> Self {
        Self {
            headers_to_strip: Vec::default(),
        }
    }
}

impl<H, M> tower::layer::Layer<M> for Layer<H>
where
    H: AsHeaderName + Clone,
{
    type Service = MakeSvc<H, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            headers_to_strip: self.headers_to_strip.clone(),
            inner,
        }
    }
}

impl<H, T, M> NewService<T> for MakeSvc<H, M>
where
    T: CanOverrideAuthority + Clone + Send + Sync + 'static,
    M: NewService<T>,
    H: AsHeaderName + Clone,
{
    type Service = Service<M::Service, H>;

    fn new_service(&mut self, t: T) -> Self::Service {
        let authority = t.override_authority();
        let inner = self.inner.new_service(t);
        Service {
            authority,
            headers_to_strip: self.headers_to_strip.clone(),
            inner,
        }
    }
}

// === impl Service ===

impl<S, H, B> tower::Service<http::Request<B>> for Service<S, H>
where
    S: tower::Service<http::Request<B>>,
    H: AsHeaderName + fmt::Display + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

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
