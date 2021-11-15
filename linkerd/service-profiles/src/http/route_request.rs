use super::{RequestMatch, Route};
use crate::{Profile, Receiver, ReceiverStream};
use futures::{future, prelude::*, ready};
use linkerd_stack::{layer, NewService, Oneshot, Param, ServiceExt};
use std::task::{Context, Poll};
use tracing::{debug, trace};

pub fn layer<N>() -> impl layer::Layer<N, Service = NewRouteRequest<N>> {
    // This is saved so that the same `Arc`s are used and cloned instead of
    // calling `Route::default()` every time.
    layer::mk(move |inner| NewRouteRequest { inner })
}

#[derive(Clone)]
pub struct NewRouteRequest<N> {
    inner: N,
}

pub struct RouteRequest<T, N>
where
    N: NewService<(Option<Route>, T)>,
{
    target: T,
    rx: ReceiverStream,
    inner: N,
    default: N::Service,
    http_routes: Vec<(RequestMatch, Route)>,
}

// === impl NewRouteRequest ===

impl<T, N> NewService<T> for NewRouteRequest<N>
where
    T: Param<Receiver> + Clone,
    N: NewService<(Option<Route>, T)> + Clone,
{
    type Service = RouteRequest<T, N>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx = target.param();
        let default = self.inner.new_service((None, target.clone()));
        RouteRequest {
            rx: rx.into(),
            target,
            inner: self.inner.clone(),
            default,
            http_routes: Vec::new(),
        }
    }
}

// === impl RouteRequest ===

impl<B, T, N, S> tower::Service<http::Request<B>> for RouteRequest<T, N>
where
    T: Clone,
    N: NewService<(Option<Route>, T), Service = S> + Clone,
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = future::Either<S::Future, Oneshot<S, http::Request<B>>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        if let Poll::Ready(Some(Profile { http_routes, .. })) = self.rx.poll_next_unpin(cx) {
            debug!(routes = %http_routes.len(), "Updating HTTP routes");
            self.http_routes = http_routes;
        }

        Poll::Ready(ready!(self.default.poll_ready(cx)))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        for (ref condition, ref route) in &self.http_routes {
            if condition.is_match(&req) {
                trace!(?condition, "Using configured route");
                let svc = self
                    .inner
                    .new_service((Some(route.clone()), self.target.clone()));

                return future::Either::Right(svc.oneshot(req));
            }
        }

        trace!("No routes matched");
        future::Either::Left(self.default.call(req))
    }
}
