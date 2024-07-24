use linkerd_app_core::{proxy::http, svc};
use linkerd_proxy_client_policy as policy;
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct Params {
    pub timeouts: policy::http::Timeouts,
}

#[derive(Clone, Debug)]
pub struct NewSetExtensions<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct SetExtensions<S> {
    inner: S,
    params: Params,
}

// === impl NewSetExtensions ===

impl<N> NewSetExtensions<N> {
    pub fn layer() -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(|inner| Self { inner })
    }
}

impl<T, N> svc::NewService<T> for NewSetExtensions<N>
where
    T: svc::Param<Params>,
    N: svc::NewService<T>,
{
    type Service = SetExtensions<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let params = target.param();
        let inner = self.inner.new_service(target);
        SetExtensions { params, inner }
    }
}

// === impl SetExtensions ===

impl<B, S> svc::Service<http::Request<B>> for SetExtensions<S>
where
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let timeouts = self.configure_timeouts();
        tracing::debug!(?timeouts, "Setting extensions");

        let _prior = req.extensions_mut().insert(timeouts);
        debug_assert!(
            _prior.is_none(),
            "StreamTimeouts must only be configured once"
        );

        self.inner.call(req)
    }
}

impl<S> SetExtensions<S> {
    fn configure_timeouts(&self) -> http::StreamTimeouts {
        http::StreamTimeouts {
            response_headers: None,
            response_end: self.params.timeouts.response,
            idle: self.params.timeouts.idle,
            limit: self.params.timeouts.request.map(Into::into),
        }
    }
}
