use super::{RequestMatch, Route};
use crate::{Profile, Receiver, ReceiverStream};
use futures::{future, prelude::*, ready};
use linkerd_error::Result;
use linkerd_stack::{layer, NewService, Oneshot, Param, RecognizeRoute, ServiceExt};
use std::task::{Context, Poll};
use tracing::{debug, trace};

#[derive(Clone)]
pub struct NewProxyRouter<N> {
    inner: N,
}

pub struct ProxyRouter<T, N>
where
    N: NewService<(Option<Route>, T)>,
{
    recognize: Recognize<T>,
    rx: ReceiverStream,
    inner: N,
    default: N::Service,
    http_routes: Vec<(RequestMatch, Route)>,
}

#[derive(Copy, Clone, Debug)]
pub struct NewRecognize(());

#[derive(Clone, Debug)]
pub struct Recognize<T> {
    rx: Receiver,
    target: T,
}

// === impl NewRecognize ===

impl<T: Param<Receiver>> NewService<T> for NewRecognize {
    type Service = Recognize<T>;

    fn new_service(&self, target: T) -> Self::Service {
        Recognize {
            rx: target.param(),
            target,
        }
    }
}

// === impl Recognize ===

impl<T: Clone, B> RecognizeRoute<http::Request<B>> for Recognize<T> {
    type Key = (Option<Route>, T);

    fn recognize(&self, req: &http::Request<B>) -> Result<Self::Key> {
        for (condition, route) in &self.rx.borrow().http_routes {
            if condition.is_match(req) {
                trace!(?condition, "Using configured route");
                return Ok((Some(route.clone()), self.target.clone()));
            }
        }

        trace!("No matching routes");
        Ok((None, self.target.clone()))
    }
}

// === impl NewProxyRouter ===

impl<T, N> NewService<T> for NewProxyRouter<N>
where
    T: Param<Receiver> + Clone,
    N: NewService<(Option<Route>, T)> + Clone,
{
    type Service = ProxyRouter<T, N>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx = target.param();
        let default = self.inner.new_service((None, target.clone()));
        ProxyRouter {
            rx: rx.into(),
            target,
            inner: self.inner.clone(),
            default,
            http_routes: Vec::new(),
        }
    }
}

// === impl ProxyRouter ===

impl<B, T, N, S> tower::Service<http::Request<B>> for ProxyRouter<T, N>
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

        self.default.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let (route, target) = match self.recognize.recognize(&req) {
            Ok(r) => r,
            Err(e) => {
                debug!(?e, "No matching route");
                return future::Either::Left(self.default.call(req));
            }
        };
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
