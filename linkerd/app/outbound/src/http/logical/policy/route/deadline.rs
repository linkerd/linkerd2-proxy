use linkerd_app_core::{proxy::http, svc};
use linkerd_proxy_client_policy::http::Timeouts;
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct NewSetStreamTimeoutsExtension<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct SetStreamTimeoutsExtension<S> {
    inner: S,
    timeouts: Timeouts,
}

// === impl NewSetDeadlines ===

impl<N> NewSetStreamTimeoutsExtension<N> {
    pub fn layer() -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(|inner| Self { inner })
    }
}

impl<T, N> svc::NewService<T> for NewSetStreamTimeoutsExtension<N>
where
    N: svc::NewService<T>,
    T: svc::Param<Timeouts>,
{
    type Service = SetStreamTimeoutsExtension<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let timeouts = target.param();
        SetStreamTimeoutsExtension {
            timeouts,
            inner: self.inner.new_service(target),
        }
    }
}

// === impl SetDeadlines ===

impl<B, S> svc::Service<http::Request<B>> for SetStreamTimeoutsExtension<S>
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
        // TODO(ver): Use request headers, like grpc-timeout, to set deadlines.
        let _prior = req.extensions_mut().insert(http::StreamTimeouts {
            response_headers: self.timeouts.response,
            response_end: self.timeouts.response,
            idle: self.timeouts.idle,
            limit: self.timeouts.stream.map(Into::into),
        });
        debug_assert!(_prior.is_none(), "Timeouts must only be configured once");
        self.inner.call(req)
    }
}
